//! The single-window compositor: a transparent, composition-hosted WebView2
//! overlay (the React UI, served by the control server) layered over a
//! DirectComposition visual that libmpv renders video into. Promoted from
//! `spike/rust-keystone` — the proven keystone — and wired to the real engine.
//!
//! Runs on the main/UI thread (WebView2 + the message loop live here); the
//! runner and control server run on background threads.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    MONITORINFO, MonitorFromRect, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, ReleaseCapture, TME_LEAVE,
    TRACKMOUSEEVENT, TrackMouseEvent, VK_F,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
use windows::core::{Error, Result};

use crate::gl::GlVideo;
use crate::mpv::Handle;

type QuitCb = Box<dyn Fn() + Send + Sync>;

/// `WM_MOUSELEAVE` lives in the `Win32_UI_Controls` feature module; we use one
/// constant from it, so define it here rather than pull in the whole module.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Custom message: toggle borderless fullscreen on the UI thread. Posted from
/// the control-server thread when the overlay hits `POST /fullscreen` (or the
/// `f` keybind), since window styles must be changed on the owning thread.
const WM_TOGGLE_FULLSCREEN: u32 = WM_APP + 1;
const WM_WINDOW_MINIMIZE: u32 = WM_APP + 2;
const WM_WINDOW_MAXIMIZE: u32 = WM_APP + 3;
const WM_WINDOW_CLOSE: u32 = WM_APP + 4;
// Set pin (stay-on-top) state on the UI thread. WPARAM carries the desired state
// (1 = pinned, 0 = unpinned) — an idempotent set, not a flip, so a duplicated
// request from one click can't cancel itself out.
const WM_SET_STAY_ON_TOP: u32 = WM_APP + 5;
// Begin a native window move (caption drag) initiated from the overlay — used in
// pin mode, where there is no titlebar and the bare video surface is the drag
// handle. The overlay decides what is draggable (it owns the DOM) and asks the
// host to start the standard move loop.
const WM_BEGIN_DRAG: u32 = WM_APP + 6;
// Observability: id for the global Ctrl+Alt+F hotkey that stamps a monitor-flash
// marker into the log. Global (not an overlay keybind) because the flash happens
// while shows is backgrounded and another app holds focus.
const FLASH_MARKER_HOTKEY_ID: i32 = 0xF1A5;

// Custom-chrome geometry, defined ONCE in logical (96-DPI) pixels and scaled by
// the window DPI everywhere it is used, so the Win32 hit-test regions line up
// with what the overlay renders at any DPI. The overlay CSS mirrors these:
// `.titlebar { height: 32px }` and `.titlebar-actions { width: 138px }`. See
// `docs/feature-contracts/desktop-shell.md`.
const TITLEBAR_LOGICAL_H: i32 = 32;
const WINDOW_BUTTONS_LOGICAL_W: i32 = 138;
const RESIZE_BORDER_LOGICAL: i32 = 6;

/// Scale a logical (96-DPI) length to physical pixels for `dpi`.
fn scale_for_dpi(logical: i32, dpi: i32) -> i32 {
    (logical * dpi.max(96)) / 96
}

/// Custom titlebar height in physical pixels for this window's DPI. The single
/// source the swapchain offset, overlay bounds, and caption hit-test all use.
fn titlebar_height_px(hwnd: HWND) -> i32 {
    scale_for_dpi(TITLEBAR_LOGICAL_H, unsafe { GetDpiForWindow(hwnd) } as i32)
}

type StatusUpdateCb = Box<dyn Fn(serde_json::Value) + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CursorMotion {
    last_pos: Option<(i32, i32)>,
}

impl CursorMotion {
    fn new(last_pos: Option<POINT>) -> Self {
        Self {
            last_pos: last_pos.map(|p| (p.x, p.y)),
        }
    }

    fn observe(&mut self, point: POINT) -> bool {
        let next = (point.x, point.y);
        let moved = self.last_pos.is_some_and(|last| last != next);
        self.last_pos = Some(next);
        moved
    }
}

fn rect_width(rect: RECT) -> i32 {
    rect.right - rect.left
}

fn rect_height(rect: RECT) -> i32 {
    rect.bottom - rect.top
}

/// Map a client-relative point to a non-client hit-test code for the custom
/// chrome. Pure (no HWND) so it is unit-tested across DPIs. `cx`/`cy` are
/// relative to the window's top-left; `width`/`height` are the window size.
/// All geometry derives from the shared logical constants, scaled by `dpi`, so
/// the draggable caption and the window-button zone match the rendered overlay
/// at every scale factor.
fn nc_hit(
    cx: i32,
    cy: i32,
    width: i32,
    height: i32,
    dpi: i32,
    is_max: bool,
    has_titlebar: bool,
) -> u32 {
    let titlebar = scale_for_dpi(TITLEBAR_LOGICAL_H, dpi);
    let buttons = scale_for_dpi(WINDOW_BUTTONS_LOGICAL_W, dpi);
    let border = scale_for_dpi(RESIZE_BORDER_LOGICAL, dpi).max(1);

    if !is_max {
        let is_top = cy < border;
        let is_bottom = cy >= height - border;
        let is_left = cx < border;
        let is_right = cx >= width - border;
        if is_top && is_left {
            return HTTOPLEFT;
        }
        if is_top && is_right {
            return HTTOPRIGHT;
        }
        if is_bottom && is_left {
            return HTBOTTOMLEFT;
        }
        if is_bottom && is_right {
            return HTBOTTOMRIGHT;
        }
        if is_top {
            return HTTOP;
        }
        if is_bottom {
            return HTBOTTOM;
        }
        if is_left {
            return HTLEFT;
        }
        if is_right {
            return HTRIGHT;
        }
    }

    // Within the titlebar height and left of the window-button cluster: drag.
    // Pin mode has no titlebar — the bare surface is dragged via the overlay
    // (WM_BEGIN_DRAG), so no caption strip here; everything non-border is client.
    if has_titlebar && cy >= 0 && cy < titlebar && cx < width - buttons {
        return HTCAPTION;
    }
    HTCLIENT
}

/// Constrain a `WM_SIZING` rect to `aspect` (width / height), in place. `edge` is
/// the `WMSZ_*` handle being dragged; we adjust the dimension the user is *not*
/// driving and anchor the corner/edge that stays put so the window doesn't crawl
/// across the screen as it resizes:
/// - dragging a left/right edge drives width → derive height (keep top fixed),
/// - dragging a top/bottom edge drives height → derive width (keep left fixed),
/// - dragging a corner drives width → derive height, pinned to that corner.
///
/// Pure (operates on a plain `RECT`) so it is unit-tested without a window. In
/// pin mode the window has no non-client frame, so the window rect equals the
/// client/video rect and this locks the visible video shape directly.
fn constrain_rect_aspect(rect: &mut RECT, edge: u32, aspect: f64) {
    let width = rect_width(*rect);
    let height = rect_height(*rect);
    match edge {
        WMSZ_LEFT | WMSZ_RIGHT => {
            rect.bottom = rect.top + ((width as f64 / aspect).round() as i32).max(1);
        }
        WMSZ_TOP | WMSZ_BOTTOM => {
            rect.right = rect.left + ((height as f64 * aspect).round() as i32).max(1);
        }
        // Corners: width-driven height, anchored to the dragged corner so the
        // fixed corner stays put.
        WMSZ_TOPLEFT | WMSZ_TOPRIGHT => {
            rect.top = rect.bottom - ((width as f64 / aspect).round() as i32).max(1);
        }
        _ => {
            // WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT
            rect.bottom = rect.top + ((width as f64 / aspect).round() as i32).max(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    #[test]
    fn cursor_motion_ignores_first_unseeded_position() {
        let mut motion = CursorMotion::new(None);

        assert!(!motion.observe(point(320, 180)));
        assert!(!motion.observe(point(320, 180)));
    }

    #[test]
    fn cursor_motion_reveals_only_after_coordinate_change() {
        let mut motion = CursorMotion::new(Some(point(320, 180)));

        assert!(!motion.observe(point(320, 180)));
        assert!(motion.observe(point(321, 180)));
        assert!(!motion.observe(point(321, 180)));
        assert!(motion.observe(point(321, 181)));
    }

    // The whole point of the DPI fix: the caption drag region must cover the
    // full *rendered* titlebar height (logical 32 × dpi/96), not a flat 32px.
    #[test]
    fn caption_drag_spans_full_titlebar_height_at_every_dpi() {
        for dpi in [96, 120, 144, 192] {
            let tb = scale_for_dpi(TITLEBAR_LOGICAL_H, dpi);
            // Bottom row of the visible titlebar, left of the buttons: drags.
            assert_eq!(
                nc_hit(200, tb - 1, 1000, 800, dpi, false, true),
                HTCAPTION,
                "dpi={dpi}"
            );
            // One pixel below the titlebar is client, not caption.
            assert_eq!(
                nc_hit(200, tb + 1, 1000, 800, dpi, false, true),
                HTCLIENT,
                "dpi={dpi}"
            );
        }
    }

    // The window-button cluster must stay clickable (HTCLIENT, forwarded to the
    // overlay), and the pixel just left of it must drag, at every DPI.
    #[test]
    fn button_cluster_stays_client_and_caption_starts_left_of_it() {
        for dpi in [96, 120, 144, 192] {
            let tb = scale_for_dpi(TITLEBAR_LOGICAL_H, dpi);
            let buttons = scale_for_dpi(WINDOW_BUTTONS_LOGICAL_W, dpi);
            assert_eq!(
                nc_hit(1000 - buttons + 5, tb / 2, 1000, 800, dpi, false, true),
                HTCLIENT,
                "dpi={dpi}"
            );
            assert_eq!(
                nc_hit(1000 - buttons - 5, tb / 2, 1000, 800, dpi, false, true),
                HTCAPTION,
                "dpi={dpi}"
            );
        }
    }

    #[test]
    fn resize_borders_exist_only_when_not_maximized() {
        assert_eq!(nc_hit(0, 0, 1000, 800, 96, false, true), HTTOPLEFT);
        assert_eq!(nc_hit(500, 799, 1000, 800, 96, false, true), HTBOTTOM);
        assert_eq!(nc_hit(999, 400, 1000, 800, 96, false, true), HTRIGHT);
        // Maximized: no resize band — the corner falls through to caption.
        assert_eq!(nc_hit(0, 0, 1000, 800, 96, true, true), HTCAPTION);
    }

    // Pin mode (no titlebar): the top strip is client (the overlay drags the
    // surface), but the resize borders still work.
    #[test]
    fn pin_mode_has_no_caption_strip_but_keeps_resize_borders() {
        assert_eq!(nc_hit(200, 10, 1000, 800, 96, false, false), HTCLIENT);
        assert_eq!(nc_hit(0, 0, 1000, 800, 96, false, false), HTTOPLEFT);
        assert_eq!(nc_hit(999, 400, 1000, 800, 96, false, false), HTRIGHT);
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT { left, top, right, bottom }
    }

    // Dragging a side edge drives that axis; the perpendicular axis is derived
    // and the opposite edge stays anchored.
    #[test]
    fn aspect_lock_side_edges_derive_the_other_axis() {
        // 16:9. Drag the right edge to width 1600 → height becomes 900, top fixed.
        let mut r = rect(100, 50, 1700, 1000);
        constrain_rect_aspect(&mut r, WMSZ_RIGHT, 16.0 / 9.0);
        assert_eq!(r.left, 100);
        assert_eq!(r.top, 50);
        assert_eq!(rect_width(r), 1600);
        assert_eq!(rect_height(r), 900);

        // Drag the bottom edge to height 450 → width becomes 800, left fixed.
        let mut r = rect(100, 50, 900, 500);
        constrain_rect_aspect(&mut r, WMSZ_BOTTOM, 16.0 / 9.0);
        assert_eq!(r.left, 100);
        assert_eq!(r.top, 50);
        assert_eq!(rect_height(r), 450);
        assert_eq!(rect_width(r), 800);
    }

    // A corner derives height from width and pins the dragged corner: the fixed
    // (diagonally opposite) corner must not move.
    #[test]
    fn aspect_lock_corners_pin_the_fixed_corner() {
        // Bottom-right drag: top-left (100,50) is the anchor and stays put.
        let mut r = rect(100, 50, 1700, 1234);
        constrain_rect_aspect(&mut r, WMSZ_BOTTOMRIGHT, 16.0 / 9.0);
        assert_eq!((r.left, r.top), (100, 50));
        assert_eq!(rect_width(r), 1600);
        assert_eq!(rect_height(r), 900);

        // Top-left drag: bottom-right (1700,1234) is the anchor and stays put;
        // the top moves up/down to give width 1600 → height 900.
        let mut r = rect(100, 50, 1700, 1234);
        constrain_rect_aspect(&mut r, WMSZ_TOPLEFT, 16.0 / 9.0);
        assert_eq!((r.right, r.bottom), (1700, 1234));
        assert_eq!(rect_width(r), 1600);
        assert_eq!(rect_height(r), 900);
        assert_eq!(r.top, 334);
    }
}

/// Render state + COM keep-alives the window proc needs for the lifetime of the
/// message loop.
struct State {
    swapchain: IDXGISwapChain1,
    context: ID3D11DeviceContext1,
    dcomp: IDCompositionDevice,
    gl: GlVideo,
    quit: Option<QuitCb>,
    status_callback: Option<StatusUpdateCb>,
    device: ID3D11Device, // needed by gl.resize on WM_SIZE
    // Cursor the windowless overlay last asked the host to show (NULL =
    // hidden). The host owns the OS cursor; WM_SETCURSOR applies this. `arrow`
    // is the fallback shown the instant the pointer moves while hidden.
    cursor: HCURSOR,
    arrow: HCURSOR,
    cursor_motion: CursorMotion,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual,
    _bottom: IDCompositionVisual,
    _web: IDCompositionVisual,
    _env: ICoreWebView2Environment,
    controller: ICoreWebView2CompositionController,
    base: ICoreWebView2Controller,
    _webview: ICoreWebView2,
    // Borderless-fullscreen toggle: saved on entry, restored on exit.
    is_fullscreen: bool,
    // Stay-on-top: a pure topmost Z-order flag. Chrome/layout are unchanged.
    is_on_top: bool,
    saved_style: isize,
    saved_exstyle: isize,
    saved_rect: RECT,
    // Pre-pin "normal" rect, captured on entering pin mode. Pin snaps the window
    // to the video aspect (a smaller, frameless-looking shape); persisting that
    // on close would reopen the windowed app mis-sized, so we save this instead.
    prepin_rect: Option<RECT>,
    // Occlusion standby. Set when `Present` reports `DXGI_STATUS_OCCLUDED`
    // (window fully covered), cleared when a `DXGI_PRESENT_TEST` poll reports it
    // visible again. While set, the frame timer only polls — no render, present,
    // or commit — so the GPU video path is not driven while nothing is on screen
    // (DXGI best practice; reduces churn across focus/occlusion transitions).
    standby: Cell<bool>,
    /// Force a present on the next render tick even if mpv has no new frame —
    /// set on resize/relayout, first paint, and standby resume so the surface is
    /// never left stale while the pump is otherwise gated to new-frames-only.
    force_present: Cell<bool>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
pub struct Compositor {
    hwnd: HWND,
    maximized: bool,
    has_saved: bool,
}

unsafe impl Send for Compositor {}
unsafe impl Sync for Compositor {}

impl Compositor {
    /// Build the window, GPU + DirectComposition tree, the transparent WebView2
    /// (navigated to `overlay_url`), and an mpv render context on `handle`.
    pub fn create(
        handle: Arc<Handle>,
        overlay_url: &str,
    ) -> std::result::Result<Compositor, Box<dyn std::error::Error>> {
        // Transparent default background via env var to kill the first-paint flash.
        unsafe { std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "00000000") };
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE)? };

        let (hwnd, maximized, has_saved) = create_window()?;
        let mut rc = RECT::default();
        unsafe { GetClientRect(hwnd, &mut rc)? };
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);

        let video_top = titlebar_height_px(hwnd);
        let video_h = (ch - video_top).max(1);

        // D3D11 device + composition swapchain (the video layer).
        let (device, context) = create_d3d_device()?;
        let context: ID3D11DeviceContext1 = context.cast()?;
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: cw as u32,
            Height: video_h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            ..Default::default()
        };
        let swapchain =
            unsafe { factory.CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>)? };

        // DirectComposition tree: video visual (bottom), overlay visual (top).
        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device)? };
        let target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true)? };
        let root = unsafe { dcomp.CreateVisual()? };
        let bottom = unsafe { dcomp.CreateVisual()? };
        let web = unsafe { dcomp.CreateVisual()? };
        let sc_unknown: IUnknown = swapchain.cast()?;
        unsafe {
            bottom.SetContent(&sc_unknown)?;
            bottom.SetOffsetY2(video_top as f32)?;
            root.AddVisual(&bottom, false, None::<&IDCompositionVisual>)?;
            root.AddVisual(&web, true, &bottom)?; // overlay in front of the video
            target.SetRoot(&root)?;
            dcomp.Commit()?;
        }

        // WebView2 environment + composition controller (visual hosting).
        let environment = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
                Box::new(|handler| unsafe {
                    CreateCoreWebView2Environment(&handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |hr, env| {
                    hr?;
                    tx.send(env.ok_or_else(|| Error::from(E_POINTER)))
                        .expect("send env");
                    Ok(())
                }),
            )?;
            rx.recv().expect("recv env")?
        };
        let env3: ICoreWebView2Environment3 = environment.cast()?;
        let controller: ICoreWebView2CompositionController = {
            let (tx, rx) = mpsc::channel();
            CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
                Box::new(move |handler| unsafe {
                    env3.CreateCoreWebView2CompositionController(hwnd, &handler)
                        .map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |hr, c| {
                    hr?;
                    tx.send(c.ok_or_else(|| Error::from(E_POINTER)))
                        .expect("send controller");
                    Ok(())
                }),
            )?;
            rx.recv().expect("recv controller")?
        };
        let base: ICoreWebView2Controller = controller.cast()?;
        let base2: ICoreWebView2Controller2 = controller.cast()?;
        let web_unknown: IUnknown = web.cast()?;
        unsafe {
            base.SetBounds(RECT {
                left: 0,
                top: 0,
                right: cw,
                bottom: ch,
            })?;
            base.SetIsVisible(true)?;
            base2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                A: 0,
                R: 0,
                G: 0,
                B: 0,
            })?;
            controller.SetRootVisualTarget(&web_unknown)?;
            dcomp.Commit()?;
        }

        // Honor the overlay's requested cursor. A windowless (visual-hosted)
        // WebView2 can't own the OS cursor: it reports the cursor it wants
        // (arrow over video, pointer over buttons, NULL for CSS `cursor: none`)
        // via CursorChanged, and the host must apply it. Caching it here is what
        // lets the overlay's idle auto-hide and button hover cursors reach the
        // screen; WM_SETCURSOR applies the cache.
        let arrow =
            unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or(HCURSOR(std::ptr::null_mut()));
        let cursor_handler = CursorChangedEventHandler::create(Box::new(
            |sender: Option<ICoreWebView2CompositionController>, _args| {
                if let Some(c) = sender {
                    let mut hc = HCURSOR(std::ptr::null_mut());
                    unsafe { c.Cursor(&mut hc)? };
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            st.cursor = hc;
                        }
                    });
                    unsafe { SetCursor(if hc.0.is_null() { None } else { Some(hc) }) };
                }
                Ok(())
            },
        ));
        let mut cursor_token = 0i64;
        unsafe { controller.add_CursorChanged(&cursor_handler, &mut cursor_token)? };

        let webview = unsafe { base.CoreWebView2()? };
        // Same-origin to the control server so the overlay's EventSource/fetch calls work.
        let url = CoTaskMemPWSTR::from(overlay_url);
        unsafe { webview.Navigate(*url.as_ref().as_pcwstr())? };

        // GPU video: mpv OpenGL render -> shared D3D texture (the production path).
        let gl = GlVideo::create(&device, handle.clone(), cw, video_h)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                swapchain,
                context,
                dcomp,
                gl,
                quit: None,
                status_callback: None,
                device,
                cursor: arrow,
                arrow,
                cursor_motion: CursorMotion::new(current_cursor_screen_pos()),
                _target: target,
                _root: root,
                _bottom: bottom,
                _web: web,
                _env: environment,
                controller,
                base,
                _webview: webview,
                is_fullscreen: false,
                is_on_top: false,
                saved_style: 0,
                saved_exstyle: 0,
                saved_rect: RECT::default(),
                prepin_rect: None,
                standby: Cell::new(false),
                force_present: Cell::new(true),
            });
        });

        Ok(Compositor {
            hwnd,
            maximized,
            has_saved,
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn has_saved(&self) -> bool {
        self.has_saved
    }

    pub fn maximized(&self) -> bool {
        self.maximized
    }

    pub fn set_status_callback(&self, cb: Box<dyn Fn(serde_json::Value) + Send + Sync>) {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.status_callback = Some(cb);
            }
        });
    }

    pub fn window_action_callback(
        &self,
    ) -> Box<dyn Fn(crate::webserver::WindowAction) + Send + Sync> {
        let raw = self.hwnd.0 as usize;
        Box::new(move |action| unsafe {
            let msg = match action {
                crate::webserver::WindowAction::Minimize => WM_WINDOW_MINIMIZE,
                crate::webserver::WindowAction::Maximize => WM_WINDOW_MAXIMIZE,
                crate::webserver::WindowAction::Close => WM_WINDOW_CLOSE,
                crate::webserver::WindowAction::BeginDrag => WM_BEGIN_DRAG,
            };
            let _ = PostMessageW(Some(HWND(raw as *mut _)), msg, WPARAM(0), LPARAM(0));
        })
    }

    pub fn auto_fit(&self, vw: i32, vh: i32) {
        auto_fit_window(self.hwnd, vw, vh);
    }

    /// A `Send + Sync` closure that posts a fullscreen-toggle message to the UI
    /// thread. Wire into `ControlServer::set_on_fullscreen` so the overlay's
    /// `/fullscreen` endpoint and `f` keybind both work — the actual window
    /// style/size change happens on the message-loop thread that owns the HWND.
    pub fn fullscreen_callback(&self) -> Box<dyn Fn() + Send + Sync> {
        let raw = self.hwnd.0 as usize;
        Box::new(move || unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_TOGGLE_FULLSCREEN,
                WPARAM(0),
                LPARAM(0),
            );
        })
    }

    /// A `Send + Sync` closure that posts the desired pin (stay-on-top) state to
    /// the UI thread (the topmost Z-order change must happen on the owning
    /// thread). `on_top` is the target state, carried in WPARAM.
    pub fn stay_on_top_callback(&self) -> Box<dyn Fn(bool) + Send + Sync> {
        let raw = self.hwnd.0 as usize;
        Box::new(move |on_top: bool| unsafe {
            let _ = PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_SET_STAY_ON_TOP,
                WPARAM(on_top as usize),
                LPARAM(0),
            );
        })
    }

    /// Set the shutdown hook run on window close (save resume + flush sync).
    pub fn set_quit(&self, cb: QuitCb) {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.quit = Some(cb);
            }
        });
    }

    /// Show the window and run the message loop (blocks until the window closes).
    pub fn run(&self) {
        unsafe {
            let cmd = if self.maximized {
                SW_SHOWMAXIMIZED
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(self.hwnd, cmd);
            let _ = SetForegroundWindow(self.hwnd);
            if std::env::var("SHOWS_TOPMOST").is_ok() {
                // Test-only: float above the IDE so a full-screen capture sees it.
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                );
            }
            SetTimer(Some(self.hwnd), 1, 16, None); // ~60fps render pump
            // Observability: global Ctrl+Alt+F stamps a flash marker into the log
            // even when shows is not focused (the flash happens while another app
            // is foreground). Best-effort — ignore registration failure/conflict.
            let _ = RegisterHotKey(
                Some(self.hwnd),
                FLASH_MARKER_HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_F.0 as u32,
            );
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// Observability heartbeat: ~once/sec, log shows' window state and the current
/// foreground window class, so an intermittent monitor flash (marked via the
/// Ctrl+Alt+F hotkey, or an `OBS DISPLAYCHANGE` line) can be correlated with what
/// shows and the foreground app were doing in the seconds around it. Lines are
/// prefixed `OBS ` for easy grep in `%APPDATA%\shows\shows.log`.
fn obs_heartbeat(hwnd: HWND, state: &State) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static OBS_TICK: AtomicU64 = AtomicU64::new(0);
    // 16ms render pump → every 60th tick ≈ 1s.
    if OBS_TICK.fetch_add(1, Ordering::Relaxed) % 60 != 0 {
        return;
    }
    unsafe {
        let fg = GetForegroundWindow();
        let is_fg = fg == hwnd;
        let mut buf = [0u16; 96];
        let len = GetClassNameW(fg, &mut buf).max(0) as usize;
        let fg_class = String::from_utf16_lossy(&buf[..len]);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        log::info!(
            "OBS hb fg_self={is_fg} fg_class=\"{fg_class}\" on_top={} fullscreen={} standby={} client={}x{}",
            state.is_on_top,
            state.is_fullscreen,
            state.standby.get(),
            rc.right - rc.left,
            rc.bottom - rc.top,
        );
    }
}

fn current_cursor_screen_pos() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point).ok()?;
    }
    Some(point)
}

fn is_cursor_in_window(hwnd: HWND) -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        windows::Win32::Graphics::Gdi::PtInRect(&rect, pt).as_bool()
    }
}

fn render(s: &State) {
    unsafe {
        // Occlusion standby: while the window is fully covered, only poll for
        // visibility — no draw, present, or commit. `DXGI_PRESENT_TEST` checks
        // occlusion without presenting; any non-occluded result means the window
        // is visible again, so resume.
        if s.standby.get() {
            if s.swapchain.Present(0, DXGI_PRESENT_TEST) != DXGI_STATUS_OCCLUDED {
                s.standby.set(false);
                // Re-present the current frame on resume even if paused.
                s.force_present.set(true);
                log::info!("render: visible again — leaving occlusion standby");
            }
            return;
        }

        // Re-drive the mpv GL renderer ONLY on a new frame (or a forced
        // resize/first-paint/standby-resume). Re-rendering the SAME frame ~60x/sec
        // is the committed-memory leak (~18 MB/min, even while paused). But
        // CopyResource + Present + Commit must run EVERY tick regardless: the
        // WebView2 overlay's repaints (button state, the titlebar reappearing on
        // unpin) are coupled to the per-tick present, so gating the present strands
        // overlay updates while the video is paused — the pin/titlebar regression.
        if s.gl.poll_new_frame() || s.force_present.replace(false) {
            s.gl.render(); // mpv OpenGL render -> the shared D3D texture
        }
        if let Ok(backbuffer) = s.swapchain.GetBuffer::<ID3D11Texture2D>(0) {
            s.context.CopyResource(&backbuffer, s.gl.texture());
        }
        // `Present` returns DXGI_STATUS_OCCLUDED (a success HRESULT) when the
        // window is fully covered and nothing was shown.
        if s.swapchain.Present(1, DXGI_PRESENT(0)) == DXGI_STATUS_OCCLUDED {
            s.standby.set(true);
            log::info!("render: occluded — entering standby");
        }
        // WebView2's visual-hosting output appears only after a device commit.
        let _ = s.dcomp.Commit();
    }
}

/// Rebuild everything that depends on the client size: swapchain backbuffer,
/// video-visual vertical offset (below the custom titlebar unless fullscreen),
/// overlay bounds, and the shared GL/D3D texture mpv renders into. The single
/// place layout is recomputed — called on `WM_SIZE` and after the fullscreen
/// toggle. Forces a present so the resized frame shows even while paused.
fn relayout(hwnd: HWND, st: &mut State, cw: i32, ch: i32) {
    if cw <= 0 || ch <= 0 {
        return;
    }
    unsafe {
        // Fullscreen and pin mode both hide the custom titlebar, so the video
        // fills the whole client area.
        let video_top = if st.is_fullscreen || st.is_on_top {
            0
        } else {
            titlebar_height_px(hwnd)
        };
        let video_h = (ch - video_top).max(1);
        let _ = st.swapchain.ResizeBuffers(
            0,
            cw as u32,
            video_h as u32,
            DXGI_FORMAT_UNKNOWN,
            DXGI_SWAP_CHAIN_FLAG(0),
        );
        let _ = st._bottom.SetOffsetY2(video_top as f32);
        let _ = st.base.SetBounds(RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        });
        let _ = st.gl.resize(&st.device, cw, video_h);
        // The webview's visual-hosting output needs a Commit to publish the new
        // bounds in the DComp tree.
        let _ = st.dcomp.Commit();
        // Force a present so the resized frame shows even if mpv has no new frame
        // (e.g. paused) — the render pump is otherwise gated to new-frames-only.
        st.force_present.set(true);
        render(st);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCALCSIZE => {
                if w.0 != 0 {
                    let is_fs = STATE.with(|s| {
                        s.borrow()
                            .as_ref()
                            .map(|st| st.is_fullscreen)
                            .unwrap_or(false)
                    });
                    let is_max = IsZoomed(hwnd).as_bool();
                    if is_max && !is_fs {
                        let params = &mut *(l.0 as *mut NCCALCSIZE_PARAMS);
                        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        let mut mi = MONITORINFO {
                            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        if GetMonitorInfoW(mon, &mut mi).as_bool() {
                            params.rgrc[0] = mi.rcWork;
                        }
                    }
                }
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let (is_fs, is_on_top) = STATE.with(|s| {
                    s.borrow()
                        .as_ref()
                        .map(|st| (st.is_fullscreen, st.is_on_top))
                        .unwrap_or((false, false))
                });
                if is_fs {
                    return LRESULT(HTCLIENT as isize);
                }

                let x = (l.0 & 0xFFFF) as i16 as i32;
                let y = ((l.0 >> 16) & 0xFFFF) as i16 as i32;

                let mut rect = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rect);

                let dpi = GetDpiForWindow(hwnd) as i32;
                let is_max = IsZoomed(hwnd).as_bool();
                // Pin mode hides the titlebar, so there is no caption strip.
                let code = nc_hit(
                    x - rect.left,
                    y - rect.top,
                    rect_width(rect),
                    rect_height(rect),
                    dpi,
                    is_max,
                    !is_on_top,
                );
                LRESULT(code as isize)
            }
            WM_ACTIVATE => {
                // Observability: log focus transitions + the other window's class
                // to correlate with the monitor flash. Low word == WA_INACTIVE(0)
                // when losing activation; lParam is the other window in the switch.
                let active = (w.0 & 0xFFFF) != 0;
                let other = HWND(l.0 as *mut _);
                let mut buf = [0u16; 96];
                let len = GetClassNameW(other, &mut buf).max(0) as usize;
                let other_class = String::from_utf16_lossy(&buf[..len]);
                log::info!("OBS activate active={active} other_class=\"{other_class}\"");
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_ACTIVATEAPP => {
                // App-boundary activation (alt-tab to/from a DIFFERENT process) —
                // the flash-correlated edge. w != 0 => this app becoming active.
                log::info!("OBS activateapp active={}", w.0 != 0);
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_DISPLAYCHANGE => {
                // A modeset (resolution/bpp/refresh re-publish). If the flash is a
                // modeset this fires at the exact instant — an automatic marker.
                let w_px = (l.0 as u32) & 0xFFFF;
                let h_px = ((l.0 as u32) >> 16) & 0xFFFF;
                log::warn!("OBS DISPLAYCHANGE bpp={} res={w_px}x{h_px}", w.0);
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_HOTKEY => {
                if w.0 as i32 == FLASH_MARKER_HOTKEY_ID {
                    log::warn!("OBS ===== FLASH MARKER (Ctrl+Alt+F) =====");
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // Minimized: nothing is on screen — skip render/present entirely
                // rather than driving the swapchain into a hidden window.
                if IsIconic(hwnd).as_bool() {
                    return LRESULT(0);
                }
                STATE.with(|s| {
                    if let Some(state) = s.borrow().as_ref() {
                        obs_heartbeat(hwnd, state);
                        render(state);
                    }
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP => {
                let lp = l.0 as u32;
                let point = POINT {
                    x: (lp & 0xFFFF) as i16 as i32,
                    y: (lp >> 16) as i16 as i32,
                };
                if msg == WM_MOUSEMOVE {
                    // Keyboard/media input can synthesize a same-position
                    // WM_MOUSEMOVE. Only real pointer displacement may reveal
                    // the cursor, otherwise idle-hidden state gets overwritten
                    // with an arrow and never re-hides while the mouse is still.
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            let mut screen_point = point;
                            let _ = ClientToScreen(hwnd, &mut screen_point);
                            if st.cursor_motion.observe(screen_point) && st.cursor.0.is_null() {
                                SetCursor(Some(st.arrow));
                                st.cursor = st.arrow;
                            }
                        }
                    });
                }
                // Forward mouse input to the composition-hosted overlay (it has no
                // HWND of its own). The COREWEBVIEW2_MOUSE_EVENT_KIND values equal
                // the WM_* message ids, so `msg` maps straight through. lParam holds
                // client coordinates; wParam's low word holds the MK_* virtual keys.
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        if msg == WM_MOUSEMOVE {
                            // Re-arm WM_MOUSELEAVE so CSS :hover clears when the
                            // pointer exits the window (TrackMouseEvent is one-shot).
                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            let _ = TrackMouseEvent(&mut tme);
                        }
                        let vkeys = COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS((w.0 & 0xFFFF) as i32);
                        let _ = st.controller.SendMouseInput(
                            COREWEBVIEW2_MOUSE_EVENT_KIND(msg as i32),
                            vkeys,
                            0,
                            point,
                        );
                    }
                });
                LRESULT(0)
            }
            WM_NCMOUSEMOVE => {
                // The custom titlebar is a non-client (caption) area, so pointer
                // moves over it arrive here, not as WM_MOUSEMOVE. Forward them as
                // overlay MOVE events so titlebar buttons still show hover.
                let lp = l.0 as u32;
                let mut point = POINT {
                    x: (lp & 0xFFFF) as i16 as i32,
                    y: (lp >> 16) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut point);

                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE | windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT_FLAGS(0x00000010), // TME_NONCLIENT
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        let _ = TrackMouseEvent(&mut tme);

                        let vkeys = COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS((w.0 & 0xFFFF) as i32);
                        // Forward as standard MOUSE_MOVE so the webview sees the hover
                        let _ = st.controller.SendMouseInput(
                            COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                            vkeys,
                            0,
                            point,
                        );
                    }
                });
                DefWindowProcW(hwnd, msg, w, l)
            }
            0x02A2 => {
                // WM_NCMOUSELEAVE
                let cursor_in_window = is_cursor_in_window(hwnd);
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        // Only clear overlay hover when the pointer truly left the
                        // window; moving between client area and caption must not.
                        if !cursor_in_window {
                            let _ = st.controller.SendMouseInput(
                                COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                                0,
                                POINT { x: 0, y: 0 },
                            );
                        }
                    }
                });
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_SETCURSOR => {
                // The windowless overlay can't own the OS cursor, so the host
                // applies the cursor it requested. Over the client area use the
                // cached cursor (NULL = hidden — this is how the overlay's idle
                // auto-hide reaches the OS); returning TRUE stops DefWindowProc
                // from resetting it to the class arrow. Non-client areas fall
                // through so Windows owns titlebar and resize cursors.
                let hit = l.0 as u32 & 0xFFFF;
                if hit == HTCLIENT {
                    let handled = STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            SetCursor(if st.cursor.0.is_null() {
                                None
                            } else {
                                Some(st.cursor)
                            });
                            true
                        } else {
                            false
                        }
                    });
                    if handled {
                        return LRESULT(1);
                    }
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                // Wheel lParam is in screen coordinates (unlike the other mouse
                // messages); SendMouseInput wants client. The signed delta is the
                // high word of wParam, the MK_* keys the low word.
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        let lp = l.0 as u32;
                        let mut point = POINT {
                            x: (lp & 0xFFFF) as i16 as i32,
                            y: (lp >> 16) as i16 as i32,
                        };
                        let _ = ScreenToClient(hwnd, &mut point);
                        let delta = ((w.0 >> 16) & 0xFFFF) as i16 as i32;
                        let vkeys = COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS((w.0 & 0xFFFF) as i32);
                        let _ = st.controller.SendMouseInput(
                            COREWEBVIEW2_MOUSE_EVENT_KIND(msg as i32),
                            vkeys,
                            delta as u32,
                            point,
                        );
                    }
                });
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let cursor_in_window = is_cursor_in_window(hwnd);
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        if !cursor_in_window {
                            let _ = st.controller.SendMouseInput(
                                COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                                0,
                                POINT { x: 0, y: 0 },
                            );
                        }
                    }
                });
                LRESULT(0)
            }
            WM_SETFOCUS => {
                // The windowless WebView2 has no HWND, so it can't take Win32 focus
                // directly. When our host window gains focus, hand logical focus to
                // the overlay so its keyboard shortcuts (space/n/d/f/h/j/l/arrows/…)
                // receive key events — there is no SendKeyboardInput in visual
                // hosting; keyboard routes through the parent window once focused.
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        let _ = st
                            .base
                            .MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                    }
                });
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                // Never let the window shrink below its own custom chrome: if the
                // client width drops under the window-button cluster, nc_hit's
                // caption test (`cx < width - buttons`) has no room left and the
                // titlebar stops dragging entirely. Scale with the per-window DPI
                // so it tracks nc_hit at every scale factor. (Fires before STATE
                // exists during window creation — it touches no STATE, so safe.)
                let dpi = GetDpiForWindow(hwnd) as i32;
                let min_w = scale_for_dpi(WINDOW_BUTTONS_LOGICAL_W + 80, dpi);
                let min_h = scale_for_dpi(TITLEBAR_LOGICAL_H + 80, dpi);
                let mmi = &mut *(l.0 as *mut MINMAXINFO);
                if mmi.ptMinTrackSize.x < min_w {
                    mmi.ptMinTrackSize.x = min_w;
                }
                if mmi.ptMinTrackSize.y < min_h {
                    mmi.ptMinTrackSize.y = min_h;
                }
                LRESULT(0)
            }
            WM_SIZING => {
                // Lock the window to the video's aspect ratio while in pin mode,
                // so the floating player never grows letterbox bars (wasted
                // screen space). Only pin mode constrains: windowed mode keeps a
                // titlebar and free resizing. `l` points at the proposed window
                // rect, which the OS reads back after we return TRUE.
                let aspect = STATE.with(|s| {
                    s.borrow()
                        .as_ref()
                        .filter(|st| st.is_on_top)
                        .and_then(|st| st.gl.video_aspect())
                });
                match aspect {
                    Some(aspect) => {
                        let rect = &mut *(l.0 as *mut RECT);
                        constrain_rect_aspect(rect, w.0 as u32, aspect);
                        LRESULT(1)
                    }
                    None => DefWindowProcW(hwnd, msg, w, l),
                }
            }
            WM_SIZE => {
                // The first WM_SIZE fires during CreateWindowEx, before STATE
                // is set — borrow_mut().as_mut() handles that as a no-op. We
                // skip on minimize (lParam size is 0,0); restore/maximize both
                // come back through here and rebuild against the new client
                // area: swapchain backbuffer, webview bounds, and the shared
                // GL/D3D texture mpv renders into.
                if w.0 as u32 == SIZE_MINIMIZED {
                    return LRESULT(0);
                }
                let is_max = w.0 as u32 == SIZE_MAXIMIZED;
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        if let Some(cb) = &st.status_callback {
                            cb(serde_json::json!({ "window_maximized": is_max }));
                        }
                    }
                });
                let lp = l.0 as u32;
                let cw = (lp & 0xFFFF) as i32;
                let ch = (lp >> 16) as i32;
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        relayout(hwnd, st, cw, ch);
                    }
                });
                LRESULT(0)
            }
            m if m == WM_TOGGLE_FULLSCREEN => {
                // Two-phase: snapshot/update state inside a short borrow, then
                // drop the borrow before calling `SetWindowPos` — that call
                // synchronously dispatches `WM_SIZE`, whose arm also borrows
                // STATE, and nesting would panic the RefCell.
                enum Action {
                    Enter {
                        new_style: isize,
                        monitor_rect: RECT,
                    },
                    Exit {
                        restore_style: isize,
                        restore_exstyle: isize,
                        restore_rect: RECT,
                        was_on_top: bool,
                    },
                }
                let action = STATE.with(|s| -> Option<Action> {
                    let mut b = s.borrow_mut();
                    let st = b.as_mut()?;
                    if !st.is_fullscreen {
                        let mut rc = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rc);
                        let saved_style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                        let saved_exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                        st.saved_rect = rc;
                        st.saved_style = saved_style;
                        st.saved_exstyle = saved_exstyle;
                        st.is_fullscreen = true;
                        if let Some(cb) = &st.status_callback {
                            cb(serde_json::json!({ "window_fullscreen": true }));
                        }
                        let new_style =
                            ((saved_style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0) as isize;
                        let mon: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        let mut mi = MONITORINFO {
                            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        let _ = GetMonitorInfoW(mon, &mut mi);
                        Some(Action::Enter {
                            new_style,
                            monitor_rect: mi.rcMonitor,
                        })
                    } else {
                        let action = Action::Exit {
                            restore_style: st.saved_style,
                            restore_exstyle: st.saved_exstyle,
                            restore_rect: st.saved_rect,
                            was_on_top: st.is_on_top,
                        };
                        st.is_fullscreen = false;
                        if let Some(cb) = &st.status_callback {
                            cb(serde_json::json!({ "window_fullscreen": false }));
                        }
                        Some(action)
                    }
                });
                match action {
                    Some(Action::Enter {
                        new_style,
                        monitor_rect: mr,
                    }) => {
                        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOP),
                            mr.left,
                            mr.top,
                            mr.right - mr.left,
                            mr.bottom - mr.top,
                            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
                        );
                        let _ = SetForegroundWindow(hwnd);
                    }
                    Some(Action::Exit {
                        restore_style,
                        restore_exstyle,
                        restore_rect: r,
                        was_on_top,
                    }) => {
                        SetWindowLongPtrW(hwnd, GWL_STYLE, restore_style);
                        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, restore_exstyle);
                        if (restore_style as u32 & WS_MAXIMIZE.0) != 0 {
                            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
                        } else {
                            let _ = SetWindowPos(
                                hwnd,
                                Some(HWND_TOP),
                                r.left,
                                r.top,
                                r.right - r.left,
                                r.bottom - r.top,
                                SWP_FRAMECHANGED | SWP_SHOWWINDOW,
                            );
                        }
                        // The restored exstyle predates any pin done WHILE
                        // fullscreen, so it lacks WS_EX_TOPMOST. Re-assert the real
                        // Z-order from the live pin state, or the host would think
                        // it is pinned (titlebar hidden) while the window no longer
                        // floats — and the titlebar would never come back.
                        if was_on_top {
                            let _ = SetWindowPos(
                                hwnd,
                                Some(HWND_TOPMOST),
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            );
                        }
                        let _ = SetForegroundWindow(hwnd);
                    }
                    None => {}
                }

                // Rebuild layout after entering/exiting fullscreen so the video
                // swapchain, visual offset, and overlay bounds match the new size.
                let mut rc = RECT::default();
                if GetClientRect(hwnd, &mut rc).is_ok() {
                    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            relayout(hwnd, st, cw, ch);
                        }
                    });
                }
                LRESULT(0)
            }
            m if m == WM_SET_STAY_ON_TOP => {
                // Idempotent set, not a flip: WPARAM is the desired pin state, so a
                // duplicated request from one click lands on that state instead of
                // toggling twice and undoing itself. A request matching the current
                // state is a no-op (None below). No style swap or native frame swap
                // (this replaces the old mini-player/PiP mode). Entering pin mode
                // hides the titlebar, so it also snaps the window to the video
                // aspect ratio — and resizing stays locked to it (see WM_SIZING)
                // — so the floating player wastes no screen space.
                let desired = w.0 != 0;
                let changed = STATE.with(|s| {
                    s.borrow_mut().as_mut().and_then(|st| {
                        if st.is_on_top == desired {
                            return None;
                        }
                        st.is_on_top = desired;
                        if let Some(cb) = &st.status_callback {
                            cb(serde_json::json!({ "window_on_top": st.is_on_top }));
                        }
                        // Aspect is only needed when entering pin mode; query it
                        // now so we don't hold a STATE borrow across SetWindowPos
                        // (which dispatches WM_SIZE and re-borrows STATE). Skip it
                        // while fullscreen — snapping a fullscreen window to the
                        // video aspect would shrink it off the monitor.
                        let aspect = (st.is_on_top && !st.is_fullscreen)
                            .then(|| st.gl.video_aspect())
                            .flatten();
                        Some((st.is_on_top, aspect))
                    })
                });
                if let Some((on_top, enter_aspect)) = changed {
                    let insert_after = if on_top { HWND_TOPMOST } else { HWND_NOTOPMOST };
                    let _ = SetWindowPos(
                        hwnd,
                        Some(insert_after),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    log::info!("window on_top={on_top}");
                    if on_top {
                        // Remember the windowed rect before pin reshapes it, so a
                        // close-while-pinned persists the right size (see State).
                        let mut wp = WINDOWPLACEMENT {
                            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                            ..Default::default()
                        };
                        let prepin = GetWindowPlacement(hwnd, &mut wp)
                            .is_ok()
                            .then_some(wp.rcNormalPosition);
                        STATE.with(|s| {
                            if let Some(st) = s.borrow_mut().as_mut() {
                                st.prepin_rect = prepin;
                            }
                        });
                        // The aspect snap below resizes; if the window is maximized
                        // it would shrink while WS_MAXIMIZE stays set, leaving
                        // IsZoomed() true so nc_hit drops the resize borders. Clear
                        // the maximized state first so pin gets a real floating rect.
                        if IsZoomed(hwnd).as_bool() {
                            let _ = ShowWindow(hwnd, SW_RESTORE);
                        }
                    } else {
                        STATE.with(|s| {
                            if let Some(st) = s.borrow_mut().as_mut() {
                                st.prepin_rect = None;
                            }
                        });
                    }
                    // Entering pin mode removes the titlebar, so the old window
                    // shape (sized for video + titlebar) would letterbox the
                    // video. Snap to the video aspect — keep the top-left and
                    // width, derive the height — so no screen space is wasted.
                    if let Some(aspect) = enter_aspect {
                        let mut wr = RECT::default();
                        if GetWindowRect(hwnd, &mut wr).is_ok() {
                            let width = rect_width(wr);
                            let new_h = ((width as f64 / aspect).round() as i32).max(1);
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                0,
                                0,
                                width,
                                new_h,
                                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                    }
                    // Pin mode hides the titlebar (video_top changes), so rebuild
                    // layout even though the window size didn't change.
                    let mut rc = RECT::default();
                    if GetClientRect(hwnd, &mut rc).is_ok() {
                        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
                        STATE.with(|s| {
                            if let Some(st) = s.borrow_mut().as_mut() {
                                relayout(hwnd, st, cw, ch);
                            }
                        });
                    }
                }
                LRESULT(0)
            }
            m if m == WM_BEGIN_DRAG => {
                // Start the standard window-move loop as if the caption were
                // grabbed at the current cursor. The overlay posts this when the
                // user drags the bare video surface in pin mode (no titlebar).
                let _ = ReleaseCapture();
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let lp =
                    ((pt.x as i16 as u16 as u32) | ((pt.y as i16 as u16 as u32) << 16)) as isize;
                SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    Some(WPARAM(HTCAPTION as usize)),
                    Some(LPARAM(lp)),
                );
                // SendMessageW returns only when the modal move loop ends. The
                // loop swallowed all pointer input, so the overlay saw no movement
                // and its idle timer may have hidden the bottom control bar — the
                // ONLY way to unpin with the mouse in pin mode (no titlebar). Feed
                // the overlay the post-drag cursor position so the bar reveals.
                let mut after = POINT::default();
                if GetCursorPos(&mut after).is_ok() {
                    let _ = ScreenToClient(hwnd, &mut after);
                    STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            let _ = st.controller.SendMouseInput(
                                COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                                COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                                0,
                                after,
                            );
                        }
                    });
                }
                LRESULT(0)
            }
            m if m == WM_WINDOW_MINIMIZE => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                LRESULT(0)
            }
            m if m == WM_WINDOW_MAXIMIZE => {
                let is_max = IsZoomed(hwnd).as_bool();
                let cmd = if is_max { SW_RESTORE } else { SW_MAXIMIZE };
                let _ = ShowWindow(hwnd, cmd);
                let _ = SetForegroundWindow(hwnd);
                LRESULT(0)
            }
            m if m == WM_WINDOW_CLOSE => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                save_window_placement(hwnd);
                STATE.with(|s| {
                    if let Some(state) = s.borrow().as_ref() {
                        if let Some(cb) = &state.quit {
                            cb();
                        }
                    }
                });
                let _ = KillTimer(Some(hwnd), 1);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

fn window_config_path() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&base)
        .join("shows")
        .join("window.json")
}

fn load_window_placement() -> Option<(i32, i32, i32, i32, bool)> {
    let path = window_config_path();
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let left = json.get("left")?.as_i64()? as i32;
    let top = json.get("top")?.as_i64()? as i32;
    let right = json.get("right")?.as_i64()? as i32;
    let bottom = json.get("bottom")?.as_i64()? as i32;
    let maximized = json.get("maximized")?.as_bool().unwrap_or(false);
    let rect = RECT {
        left,
        top,
        right,
        bottom,
    };
    if right - left > 100 && bottom - top > 100 && placement_is_visible(rect) {
        Some((left, top, right - left, bottom - top, maximized))
    } else {
        None
    }
}

fn placement_is_visible(rect: RECT) -> bool {
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONULL);
        !monitor.is_invalid()
    }
}

fn save_window_placement(hwnd: HWND) {
    unsafe {
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut wp).is_ok() {
            let mut maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32;
            let mut rect = wp.rcNormalPosition;

            // Fullscreen swaps the window rect, so persist the pre-fullscreen
            // rect instead of the borderless one. Pin mode aspect-snaps the rect,
            // so persist the pre-pin windowed rect instead. (Fullscreen wins when
            // both hold — its saved_rect already predates any in-fullscreen pin.)
            let saved_transient = STATE.with(|s| {
                s.borrow().as_ref().map(|st| {
                    (
                        st.is_fullscreen,
                        st.saved_rect,
                        st.saved_style,
                        st.is_on_top,
                        st.prepin_rect,
                    )
                })
            });
            match saved_transient {
                Some((true, saved_rect, saved_style, _, _)) => {
                    rect = saved_rect;
                    maximized = (saved_style as u32 & WS_MAXIMIZE.0) != 0;
                }
                Some((false, _, _, true, Some(prepin))) => {
                    rect = prepin;
                }
                _ => {}
            }

            let json = serde_json::json!({
                "left": rect.left,
                "top": rect.top,
                "right": rect.right,
                "bottom": rect.bottom,
                "maximized": maximized,
            });
            if let Ok(content) = serde_json::to_string(&json) {
                let path = window_config_path();
                let _ = std::fs::create_dir_all(path.parent().unwrap());
                let _ = std::fs::write(path, content);
            }
        }
    }
}

fn auto_fit_window(hwnd: HWND, vw: i32, vh: i32) {
    unsafe {
        let mon: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork;
        let work_w = work.right - work.left;
        let work_h = work.bottom - work.top;

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: vw,
            bottom: vh,
        };
        let _ = AdjustWindowRectEx(
            &mut rc,
            WINDOW_STYLE(style),
            false,
            WINDOW_EX_STYLE(exstyle),
        );

        let mut win_w = rc.right - rc.left;
        let mut win_h = rc.bottom - rc.top;

        let max_w = (work_w as f64 * 0.85) as i32;
        let max_h = (work_h as f64 * 0.85) as i32;
        if win_w > max_w || win_h > max_h {
            let scale = (max_w as f64 / win_w as f64).min(max_h as f64 / win_h as f64);
            win_w = (win_w as f64 * scale) as i32;
            win_h = (win_h as f64 * scale) as i32;
        }

        let x = work.left + (work_w - win_w) / 2;
        let y = work.top + (work_h - win_h) / 2;

        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            win_w,
            win_h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

fn create_window() -> Result<(HWND, bool, bool)> {
    let hinstance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
    let class = w!("shows-desktop");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinstance,
        lpszClassName: class,
        hIcon: unsafe { LoadIconW(Some(hinstance), w!("IDI_ICON1")) }
            .unwrap_or(HICON(std::ptr::null_mut())),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or(HCURSOR(std::ptr::null_mut())),
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };

    let mut x = CW_USEDEFAULT;
    let mut y = CW_USEDEFAULT;
    let mut w = 1280;
    let mut h = 800;
    let mut maximized = false;
    let mut has_saved = false;

    if let Some((saved_x, saved_y, saved_w, saved_h, saved_maximized)) = load_window_placement() {
        x = saved_x;
        y = saved_y;
        w = saved_w;
        h = saved_h;
        maximized = saved_maximized;
        has_saved = true;
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            class,
            w!("shows"),
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance),
            None,
        )?
    };
    Ok((hwnd, maximized, has_saved))
}

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    Ok((device.unwrap(), context.unwrap()))
}
