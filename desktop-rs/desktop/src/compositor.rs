//! The single-window compositor: a transparent, composition-hosted WebView2
//! overlay (the React UI, served by the control server) layered over a
//! DirectComposition visual that libmpv renders video into. Promoted from
//! `spike/rust-keystone` — the proven keystone — and wired to the real engine.
//!
//! Runs on the main/UI thread (WebView2 + the message loop live here); the
//! runner and control server run on background threads.

use std::cell::RefCell;
use std::sync::mpsc;
use std::sync::Arc;
use std::path::PathBuf;

use webview2_com::{Microsoft::Web::WebView2::Win32::*, *};
use windows::core::*;
use windows::core::{Error, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, ScreenToClient, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::*;

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

/// Render state + COM keep-alives the window proc needs for the lifetime of the
/// message loop.
struct State {
    swapchain: IDXGISwapChain1,
    context: ID3D11DeviceContext1,
    dcomp: IDCompositionDevice,
    gl: GlVideo,
    quit: Option<QuitCb>,
    device: ID3D11Device, // needed by gl.resize on WM_SIZE
    // Cursor the windowless overlay last asked the host to show (NULL =
    // hidden). The host owns the OS cursor; WM_SETCURSOR applies this. `arrow`
    // is the fallback shown the instant the pointer moves while hidden.
    cursor: HCURSOR,
    arrow: HCURSOR,
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
    saved_style: isize,
    saved_exstyle: isize,
    saved_rect: RECT,
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
    pub fn create(handle: Arc<Handle>, overlay_url: &str) -> std::result::Result<Compositor, Box<dyn std::error::Error>> {
        // Transparent default background via env var to kill the first-paint flash.
        unsafe { std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "00000000") };
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE)? };

        let (hwnd, maximized, has_saved) = create_window()?;
        let mut rc = RECT::default();
        unsafe { GetClientRect(hwnd, &mut rc)? };
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);

        // D3D11 device + composition swapchain (the video layer).
        let (device, context) = create_d3d_device()?;
        let context: ID3D11DeviceContext1 = context.cast()?;
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi_device.GetAdapter()? };
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent()? };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: cw as u32,
            Height: ch as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
                    CreateCoreWebView2Environment(&handler).map_err(webview2_com::Error::WindowsError)
                }),
                Box::new(move |hr, env| {
                    hr?;
                    tx.send(env.ok_or_else(|| Error::from(E_POINTER))).expect("send env");
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
                    tx.send(c.ok_or_else(|| Error::from(E_POINTER))).expect("send controller");
                    Ok(())
                }),
            )?;
            rx.recv().expect("recv controller")?
        };
        let base: ICoreWebView2Controller = controller.cast()?;
        let base2: ICoreWebView2Controller2 = controller.cast()?;
        let web_unknown: IUnknown = web.cast()?;
        unsafe {
            base.SetBounds(RECT { left: 0, top: 0, right: cw, bottom: ch })?;
            base.SetIsVisible(true)?;
            base2.SetDefaultBackgroundColor(COREWEBVIEW2_COLOR { A: 0, R: 0, G: 0, B: 0 })?;
            controller.SetRootVisualTarget(&web_unknown)?;
            dcomp.Commit()?;
        }

        // Honor the overlay's requested cursor. A windowless (visual-hosted)
        // WebView2 can't own the OS cursor: it reports the cursor it wants
        // (arrow over video, pointer over buttons, NULL for CSS `cursor: none`)
        // via CursorChanged, and the host must apply it. Caching it here is what
        // lets the overlay's idle auto-hide and button hover cursors reach the
        // screen; WM_SETCURSOR applies the cache.
        let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or(HCURSOR(std::ptr::null_mut()));
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
        // Same-origin to the control server so the overlay's fetch('/status') works.
        let url = CoTaskMemPWSTR::from(overlay_url);
        unsafe { webview.Navigate(*url.as_ref().as_pcwstr())? };

        // GPU video: mpv OpenGL render -> shared D3D texture (the production path).
        let gl = GlVideo::create(&device, handle.clone(), cw, ch)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                swapchain,
                context,
                dcomp,
                gl,
                quit: None,
                device,
                cursor: arrow,
                arrow,
                _target: target,
                _root: root,
                _bottom: bottom,
                _web: web,
                _env: environment,
                controller,
                base,
                _webview: webview,
                is_fullscreen: false,
                saved_style: 0,
                saved_exstyle: 0,
                saved_rect: RECT::default(),
            });
        });

        Ok(Compositor { hwnd, maximized, has_saved })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn has_saved(&self) -> bool {
        self.has_saved
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
            let cmd = if self.maximized { SW_SHOWMAXIMIZED } else { SW_SHOW };
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
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn render(s: &State) {
    unsafe {
        s.gl.render(); // mpv OpenGL render -> the shared D3D texture
        if let Ok(backbuffer) = s.swapchain.GetBuffer::<ID3D11Texture2D>(0) {
            s.context.CopyResource(&backbuffer, s.gl.texture());
        }
        let _ = s.swapchain.Present(1, DXGI_PRESENT(0));
        // WebView2's visual-hosting output appears only after a device commit.
        let _ = s.dcomp.Commit();
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                STATE.with(|s| {
                    if let Some(state) = s.borrow().as_ref() {
                        render(state);
                    }
                });
                LRESULT(0)
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP => {
                if msg == WM_MOUSEMOVE {
                    // The overlay's idle auto-hide sets `cursor: none`; reveal the
                    // pointer the instant the mouse moves rather than waiting for
                    // the overlay's JS to clear idle and round-trip a CursorChanged.
                    // Updating the cache too stops the very next WM_SETCURSOR from
                    // re-hiding it mid-move. (Scoped borrow, dropped before the
                    // SendMouseInput below, so no nested STATE borrow.)
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            if st.cursor.0.is_null() {
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
                        let lp = l.0 as u32;
                        let point = POINT { x: (lp & 0xFFFF) as i16 as i32, y: (lp >> 16) as i16 as i32 };
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
            WM_SETCURSOR => {
                // The windowless overlay can't own the OS cursor, so the host
                // applies the cursor it requested. Over the client area use the
                // cached cursor (NULL = hidden — this is how the overlay's idle
                // auto-hide reaches the OS); returning TRUE stops DefWindowProc
                // from resetting it to the class arrow. Non-client area (resize
                // borders, caption) falls through so system cursors still show.
                if (l.0 as u32 & 0xFFFF) == HTCLIENT {
                    let handled = STATE.with(|s| {
                        if let Some(st) = s.borrow().as_ref() {
                            SetCursor(if st.cursor.0.is_null() { None } else { Some(st.cursor) });
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
                        let mut point =
                            POINT { x: (lp & 0xFFFF) as i16 as i32, y: (lp >> 16) as i16 as i32 };
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
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        let _ = st.controller.SendMouseInput(
                            COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                            COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS(0),
                            0,
                            POINT { x: 0, y: 0 },
                        );
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
                        let _ = st.base.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                    }
                });
                LRESULT(0)
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
                let lp = l.0 as u32;
                let cw = (lp & 0xFFFF) as i32;
                let ch = (lp >> 16) as i32;
                if cw > 0 && ch > 0 {
                    STATE.with(|s| {
                        if let Some(st) = s.borrow_mut().as_mut() {
                            let _ = st.swapchain.ResizeBuffers(
                                0,
                                cw as u32,
                                ch as u32,
                                DXGI_FORMAT_UNKNOWN,
                                DXGI_SWAP_CHAIN_FLAG(0),
                            );
                            let _ = st.base.SetBounds(RECT {
                                left: 0,
                                top: 0,
                                right: cw,
                                bottom: ch,
                            });
                            let _ = st.gl.resize(&st.device, cw, ch);
                            // The webview's visual-hosting output needs a Commit
                            // to publish the new bounds in the DComp tree.
                            let _ = st.dcomp.Commit();
                            render(st);
                        }
                    });
                }
                LRESULT(0)
            }
            m if m == WM_TOGGLE_FULLSCREEN => {
                // Two-phase: snapshot/update state inside a short borrow, then
                // drop the borrow before calling `SetWindowPos` — that call
                // synchronously dispatches `WM_SIZE`, whose arm also borrows
                // STATE, and nesting would panic the RefCell.
                enum Action {
                    Enter { new_style: isize, monitor_rect: RECT },
                    Exit { restore_style: isize, restore_exstyle: isize, restore_rect: RECT },
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
                        let new_style =
                            ((saved_style as u32 & !WS_OVERLAPPEDWINDOW.0) | WS_POPUP.0) as isize;
                        let mon: HMONITOR = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                        let mut mi = MONITORINFO {
                            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                            ..Default::default()
                        };
                        let _ = GetMonitorInfoW(mon, &mut mi);
                        Some(Action::Enter { new_style, monitor_rect: mi.rcMonitor })
                    } else {
                        let action = Action::Exit {
                            restore_style: st.saved_style,
                            restore_exstyle: st.saved_exstyle,
                            restore_rect: st.saved_rect,
                        };
                        st.is_fullscreen = false;
                        Some(action)
                    }
                });
                match action {
                    Some(Action::Enter { new_style, monitor_rect: mr }) => {
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
                    }
                    Some(Action::Exit { restore_style, restore_exstyle, restore_rect: r }) => {
                        SetWindowLongPtrW(hwnd, GWL_STYLE, restore_style);
                        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, restore_exstyle);
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
                    None => {}
                }
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
    std::path::Path::new(&base).join("shows").join("window.json")
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
    if right - left > 100 && bottom - top > 100 {
        Some((left, top, right - left, bottom - top, maximized))
    } else {
        None
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
            
            let is_fs = STATE.with(|s| {
                s.borrow().as_ref().map(|st| (st.is_fullscreen, st.saved_rect, st.saved_style))
            });
            if let Some((true, saved_rect, saved_style)) = is_fs {
                rect = saved_rect;
                maximized = (saved_style as u32 & WS_MAXIMIZE.0) != 0;
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
        let mut rc = RECT { left: 0, top: 0, right: vw, bottom: vh };
        let _ = AdjustWindowRectEx(&mut rc, WINDOW_STYLE(style), false, WINDOW_EX_STYLE(exstyle));
        
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
