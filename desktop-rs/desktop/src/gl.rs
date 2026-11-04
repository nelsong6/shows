//! GPU video path: mpv's OpenGL render API into a GL framebuffer backed by a
//! D3D11 texture (shared via `WGL_NV_DX_interop2`), which then feeds the
//! DirectComposition video visual. Uses native `opengl32.dll` (no ANGLE DLLs to
//! ship). This is the production replacement for the throwaway software path.
//!
//! The WGL context is created and made current on the main/UI thread, and mpv
//! renders on that same thread (the compositor's frame timer), so the context
//! stays current throughout.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::null_mut;
use std::sync::Arc;

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{GetDC, HDC};
use windows::Win32::Graphics::OpenGL::{
    wglCreateContext, wglGetProcAddress, wglMakeCurrent, ChoosePixelFormat, SetPixelFormat, HGLRC,
    PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_MAIN_PLANE, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
    PIXELFORMATDESCRIPTOR,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, CW_USEDEFAULT, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::w;

use crate::mpv::{Handle, MpvRenderParam, MPV_RENDER_PARAM_API_TYPE, MPV_RENDER_PARAM_INVALID};

// GL / WGL constants (kept local so we don't depend on which the windows crate exposes).
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_READ_FRAMEBUFFER: u32 = 0x8CA8;
const GL_DRAW_FRAMEBUFFER: u32 = 0x8CA9;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_RGBA8: u32 = 0x8058;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_LINEAR: u32 = 0x2601;
const WGL_ACCESS_READ_ONLY_NV: u32 = 0x0000;
const WGL_ACCESS_WRITE_DISCARD_NV: u32 = 0x0000_0002;

// mpv render params used here (mpv/render.h + render_gl.h).
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: i32 = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: i32 = 3;
const MPV_RENDER_PARAM_FLIP_Y: i32 = 4;
const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: i32 = 12;

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

/// Resolve a GL/WGL entry point: extensions via `wglGetProcAddress`, core 1.1
/// via `opengl32.dll`.
unsafe fn gl_proc(name: *const c_char) -> *mut c_void {
    if let Some(f) = wglGetProcAddress(PCSTR(name as *const u8)) {
        let addr = f as usize;
        // wglGetProcAddress returns 1/2/3/usize::MAX for "not found" on some drivers.
        if addr > 3 && addr != usize::MAX {
            return addr as *mut c_void;
        }
    }
    if let Ok(h) = LoadLibraryW(w!("opengl32.dll")) {
        if let Some(f) = GetProcAddress(h, PCSTR(name as *const u8)) {
            return f as *mut c_void;
        }
    }
    null_mut()
}

extern "C" fn mpv_get_proc(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    unsafe { gl_proc(name) }
}

extern "system" fn dummy_wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

unsafe fn load<T: Copy>(name: &CStr) -> std::result::Result<T, String> {
    let p = gl_proc(name.as_ptr());
    if p.is_null() {
        return Err(format!("GL/WGL proc missing: {}", name.to_string_lossy()));
    }
    Ok(std::mem::transmute_copy::<*mut c_void, T>(&p))
}

struct GlProcs {
    gen_textures: unsafe extern "system" fn(i32, *mut u32),
    gen_framebuffers: unsafe extern "system" fn(i32, *mut u32),
    bind_framebuffer: unsafe extern "system" fn(u32, u32),
    framebuffer_texture_2d: unsafe extern "system" fn(u32, u32, u32, u32, i32),
    viewport: unsafe extern "system" fn(i32, i32, i32, i32),
    flush: unsafe extern "system" fn(),
    bind_texture: unsafe extern "system" fn(u32, u32),
    tex_image_2d: unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void),
    delete_textures: unsafe extern "system" fn(i32, *const u32),
    delete_framebuffers: unsafe extern "system" fn(i32, *const u32),
    check_framebuffer: unsafe extern "system" fn(u32) -> u32,
    blit_framebuffer: unsafe extern "system" fn(i32, i32, i32, i32, i32, i32, i32, i32, u32, u32),
    read_pixels: unsafe extern "system" fn(i32, i32, i32, i32, u32, u32, *mut c_void),
    get_error: unsafe extern "system" fn() -> u32,
    dx_object_access: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    dx_open_device: unsafe extern "system" fn(*mut c_void) -> *mut c_void,
    dx_register_object:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> *mut c_void,
    dx_unregister_object: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    dx_lock_objects: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32,
    dx_unlock_objects: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32,
}

impl GlProcs {
    unsafe fn load() -> std::result::Result<GlProcs, String> {
        Ok(GlProcs {
            gen_textures: load(c"glGenTextures")?,
            gen_framebuffers: load(c"glGenFramebuffers")?,
            bind_framebuffer: load(c"glBindFramebuffer")?,
            framebuffer_texture_2d: load(c"glFramebufferTexture2D")?,
            viewport: load(c"glViewport")?,
            flush: load(c"glFlush")?,
            bind_texture: load(c"glBindTexture")?,
            tex_image_2d: load(c"glTexImage2D")?,
            delete_textures: load(c"glDeleteTextures")?,
            delete_framebuffers: load(c"glDeleteFramebuffers")?,
            check_framebuffer: load(c"glCheckFramebufferStatus")?,
            blit_framebuffer: load(c"glBlitFramebuffer")?,
            read_pixels: load(c"glReadPixels")?,
            get_error: load(c"glGetError")?,
            dx_object_access: load(c"wglDXObjectAccessNV")?,
            dx_open_device: load(c"wglDXOpenDeviceNV")?,
            dx_register_object: load(c"wglDXRegisterObjectNV")?,
            dx_unregister_object: load(c"wglDXUnregisterObjectNV")?,
            dx_lock_objects: load(c"wglDXLockObjectsNV")?,
            dx_unlock_objects: load(c"wglDXUnlockObjectsNV")?,
        })
    }
}

pub struct GlVideo {
    handle: Arc<Handle>,
    rctx: *mut c_void,
    procs: GlProcs,
    dx_device: *mut c_void,
    gl_object: *mut c_void,
    fbo: u32,
    gl_tex: u32,
    texture: ID3D11Texture2D,
    w: i32,
    h: i32,
    _hglrc: HGLRC,
    _dummy: HWND,
}

impl GlVideo {
    /// Build the WGL context + DX-interop'd D3D texture + mpv OpenGL render
    /// context. `device` is the compositor's D3D11 device (the texture lives on
    /// it so a plain `CopyResource` lands it in the swapchain).
    pub fn create(
        device: &ID3D11Device,
        handle: Arc<Handle>,
        w: i32,
        h: i32,
    ) -> std::result::Result<GlVideo, String> {
        unsafe {
            // --- WGL context on a hidden dummy window ---
            let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            let class = w!("shows-gl-dummy");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(dummy_wndproc),
                hInstance: hinstance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let dummy = CreateWindowExW(
                Default::default(),
                class,
                w!("gl"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1,
                1,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
            .map_err(|e| e.to_string())?;
            let hdc: HDC = GetDC(Some(dummy));
            if hdc.is_invalid() {
                return Err("GetDC for GL dummy window failed".into());
            }
            let pfd = PIXELFORMATDESCRIPTOR {
                nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
                nVersion: 1,
                dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
                iPixelType: PFD_TYPE_RGBA,
                cColorBits: 32,
                cDepthBits: 24,
                iLayerType: PFD_MAIN_PLANE.0 as u8,
                ..Default::default()
            };
            let pf = ChoosePixelFormat(hdc, &pfd);
            if pf == 0 {
                return Err("ChoosePixelFormat failed".into());
            }
            SetPixelFormat(hdc, pf, &pfd).map_err(|e| e.to_string())?;
            let hglrc = wglCreateContext(hdc).map_err(|e| e.to_string())?;
            wglMakeCurrent(hdc, hglrc).map_err(|e| e.to_string())?;

            let procs = GlProcs::load()?;

            // --- D3D11 texture mpv renders into (via the interop'd GL FBO) ---
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32,
                Height: h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&desc, None, Some(&mut texture)).map_err(|e| e.to_string())?;
            let texture = texture.unwrap();

            // --- DX interop: register the D3D texture as a GL texture + FBO ---
            let dx_device = (procs.dx_open_device)(device.as_raw());
            if dx_device.is_null() {
                return Err("wglDXOpenDeviceNV failed (no WGL_NV_DX_interop2?)".into());
            }
            let mut gl_tex: u32 = 0;
            (procs.gen_textures)(1, &mut gl_tex);
            let gl_object = (procs.dx_register_object)(
                dx_device,
                texture.as_raw(),
                gl_tex,
                GL_TEXTURE_2D,
                WGL_ACCESS_WRITE_DISCARD_NV,
            );
            if gl_object.is_null() {
                return Err("wglDXRegisterObjectNV failed".into());
            }
            let mut fbo: u32 = 0;
            (procs.gen_framebuffers)(1, &mut fbo);
            (procs.bind_framebuffer)(GL_FRAMEBUFFER, fbo);
            (procs.framebuffer_texture_2d)(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, gl_tex, 0);

            // --- mpv OpenGL render context on the shared handle ---
            let mut init = MpvOpenglInitParams {
                get_proc_address: mpv_get_proc,
                get_proc_address_ctx: null_mut(),
            };
            let mut cparams = [
                MpvRenderParam { kind: MPV_RENDER_PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
                MpvRenderParam {
                    kind: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                    data: &mut init as *mut _ as *mut c_void,
                },
                MpvRenderParam { kind: MPV_RENDER_PARAM_INVALID, data: null_mut() },
            ];
            let mut rctx: *mut c_void = null_mut();
            let rc = (handle.api.render_context_create)(&mut rctx, handle.ctx, cparams.as_mut_ptr());
            if rc < 0 {
                return Err(format!("mpv_render_context_create(opengl) failed: {rc}"));
            }

            Ok(GlVideo {
                handle,
                rctx,
                procs,
                dx_device,
                gl_object,
                fbo,
                gl_tex,
                texture,
                w,
                h,
                _hglrc: hglrc,
                _dummy: dummy,
            })
        }
    }

    /// Resize the shared D3D texture (and rebind it through DX-interop) to a new
    /// size. The GL texture name + FBO + mpv render context are kept; only the
    /// backing storage changes — re-registration against the same GL name
    /// refreshes the interop binding. Call on `WM_SIZE` after the swapchain is
    /// resized; subsequent renders write into the new texture at the new size.
    pub fn resize(&mut self, device: &ID3D11Device, w: i32, h: i32) -> std::result::Result<(), String> {
        if w <= 0 || h <= 0 || (w == self.w && h == self.h) {
            return Ok(());
        }
        unsafe {
            // Release the GL/D3D interop binding on the old texture, then drop
            // the texture itself (struct field will be replaced).
            (self.procs.dx_unregister_object)(self.dx_device, self.gl_object);

            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32,
                Height: h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut new_tex: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&desc, None, Some(&mut new_tex)).map_err(|e| e.to_string())?;
            let new_tex = new_tex.unwrap();

            let new_obj = (self.procs.dx_register_object)(
                self.dx_device,
                new_tex.as_raw(),
                self.gl_tex,
                GL_TEXTURE_2D,
                WGL_ACCESS_WRITE_DISCARD_NV,
            );
            if new_obj.is_null() {
                return Err("wglDXRegisterObjectNV failed on resize".into());
            }
            // Re-attach the (same-name, freshly-bound) GL texture to the FBO's
            // color slot so the next render targets the new storage.
            (self.procs.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (self.procs.framebuffer_texture_2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                self.gl_tex,
                0,
            );

            self.texture = new_tex;
            self.gl_object = new_obj;
            self.w = w;
            self.h = h;
            Ok(())
        }
    }

    /// The D3D texture mpv renders into — `CopyResource` it to the swapchain.
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// Display aspect ratio (width / height) of the currently-loaded video, or
    /// `None` if nothing is loaded yet. `video-params/aspect` is mpv's DAR after
    /// sample-aspect and rotation, i.e. exactly the shape the frame is drawn at —
    /// used to lock the window to the video in pin mode so there are no letterbox
    /// bars.
    pub fn video_aspect(&self) -> Option<f64> {
        let a: f64 = self.handle.get_property("video-params/aspect")?.parse().ok()?;
        (a.is_finite() && a > 0.0).then_some(a)
    }

    pub fn preview(&self) -> Result<shows_control::preview::VideoPreview, String> {
        use shows_control::preview::{HEIGHT, VideoPreview, WIDTH};
        let started = std::time::Instant::now();
        if self.handle.get_property("path").is_none() {
            return Err("No video is loaded on the target".into());
        }
        unsafe {
            let p = &self.procs;
            let mut object = self.gl_object;
            // Read-only access preserves the displayed frame, including while paused.
            if (p.dx_object_access)(object, WGL_ACCESS_READ_ONLY_NV) == 0 {
                return Err("Could not read the target video texture".into());
            }
            if (p.dx_lock_objects)(self.dx_device, 1, &mut object) == 0 {
                (p.dx_object_access)(object, WGL_ACCESS_WRITE_DISCARD_NV);
                return Err("Could not lock the target video texture".into());
            }
            let mut texture = 0;
            let mut framebuffer = 0;
            (p.gen_textures)(1, &mut texture);
            (p.bind_texture)(GL_TEXTURE_2D, texture);
            (p.tex_image_2d)(GL_TEXTURE_2D, 0, GL_RGBA8 as i32, WIDTH as i32, HEIGHT as i32, 0, GL_RGBA, GL_UNSIGNED_BYTE, std::ptr::null());
            (p.gen_framebuffers)(1, &mut framebuffer);
            (p.bind_framebuffer)(GL_DRAW_FRAMEBUFFER, framebuffer);
            (p.framebuffer_texture_2d)(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
            let complete = (p.check_framebuffer)(GL_DRAW_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
            let mut pixels = vec![0u8; WIDTH * HEIGHT * 4];
            if complete {
                (p.bind_framebuffer)(GL_READ_FRAMEBUFFER, self.fbo);
                (p.blit_framebuffer)(0, 0, self.w, self.h, 0, 0, WIDTH as i32, HEIGHT as i32, GL_COLOR_BUFFER_BIT, GL_LINEAR);
                (p.bind_framebuffer)(GL_READ_FRAMEBUFFER, framebuffer);
                (p.read_pixels)(0, 0, WIDTH as i32, HEIGHT as i32, GL_RGBA, GL_UNSIGNED_BYTE, pixels.as_mut_ptr().cast());
            }
            let error = (p.get_error)();
            (p.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (p.bind_texture)(GL_TEXTURE_2D, 0);
            (p.delete_framebuffers)(1, &framebuffer);
            (p.delete_textures)(1, &texture);
            let unlocked = (p.dx_unlock_objects)(self.dx_device, 1, &mut object) != 0;
            let restored = (p.dx_object_access)(object, WGL_ACCESS_WRITE_DISCARD_NV) != 0;
            if !complete || error != 0 || !unlocked || !restored {
                return Err(format!("Video preview capture failed (GL {error:#x})"));
            }
            // This D3D-shared surface already reads in display order, matching
            // render()'s FLIP_Y=0. Reversing its rows would invert the preview.
            VideoPreview::from_rgba(&pixels, self.w as f64 / self.h as f64, started.elapsed().as_secs_f64() * 1000.0)
        }
    }

    /// Render the current mpv frame into the shared texture.
    pub fn render(&self) {
        unsafe {
            let mut obj = self.gl_object;
            (self.procs.dx_lock_objects)(self.dx_device, 1, &mut obj);
            (self.procs.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (self.procs.viewport)(0, 0, self.w, self.h);

            let mut fbo = MpvOpenglFbo { fbo: self.fbo as c_int, w: self.w, h: self.h, internal_format: 0 };
            // WGL_NV_DX_interop2 already lands mpv's default GL output upright in the
            // D3D texture; an extra FLIP_Y inverts it (video shows upside down).
            let mut flip: c_int = 0;
            let mut block: c_int = 0;
            let mut params = [
                MpvRenderParam { kind: MPV_RENDER_PARAM_OPENGL_FBO, data: &mut fbo as *mut _ as *mut c_void },
                MpvRenderParam { kind: MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
                MpvRenderParam {
                    kind: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                    data: &mut block as *mut _ as *mut c_void,
                },
                MpvRenderParam { kind: MPV_RENDER_PARAM_INVALID, data: null_mut() },
            ];
            (self.handle.api.render_context_render)(self.rctx, params.as_mut_ptr());
            (self.procs.flush)();
            (self.procs.dx_unlock_objects)(self.dx_device, 1, &mut obj);
        }
    }

    /// True if mpv reports a new video frame is ready (`MPV_RENDER_UPDATE_FRAME`,
    /// bit 0). The render pump uses this to avoid re-rendering the same frame
    /// ~60x/sec, which leaks committed memory in the GL/driver path even while
    /// paused. Cheap (returns cached flags); does not itself render.
    pub fn poll_new_frame(&self) -> bool {
        unsafe { (self.handle.api.render_context_update)(self.rctx) & 1 != 0 }
    }
}

unsafe impl Send for GlVideo {}
unsafe impl Sync for GlVideo {}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    };

    #[test]
    #[ignore = "Requires a GPU with WGL_NV_DX_interop2 and libmpv-2.dll"]
    fn preview_preserves_rendered_video_orientation() {
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None, D3D_DRIVER_TYPE_HARDWARE, Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION,
                Some(&mut device), None, None,
            ).unwrap();
        }
        let handle = Arc::new(Handle::create(crate::mpv::Api::load().unwrap()).unwrap());
        handle.set_property_checked("pause", "yes").unwrap();
        let video = GlVideo::create(&device.unwrap(), handle.clone(), 160, 90).unwrap();
        let path = std::env::temp_dir().join(format!("shows-orientation-{}.ppm", std::process::id()));
        // This image's top half is white and bottom half is black.
        let mut image = b"P6\n160 90\n255\n".to_vec();
        image.extend(std::iter::repeat_n(255, 160 * 45 * 3));
        image.extend(std::iter::repeat_n(0, 160 * 45 * 3));
        std::fs::write(&path, image).unwrap();
        let captured = (|| {
            handle.command_checked(&["loadfile", path.to_str().unwrap()])?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                handle.wait_event(0.01);
                if video.poll_new_frame() {
                    video.render();
                    if handle.get_property("video-params/w").is_some() {
                        return video.preview();
                    }
                }
            }
            Err("Timed out rendering the orientation fixture".to_string())
        })();
        unsafe { (handle.api.render_context_free)(video.rctx); }
        std::fs::remove_file(path).unwrap();
        let captured = captured.unwrap();
        assert_eq!(captured.rows[10], "@".repeat(160), "white top of the video");
        assert_eq!(captured.rows[80], " ".repeat(160), "black bottom of the video");
    }
}
