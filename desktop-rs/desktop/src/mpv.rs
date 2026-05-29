//! Minimal libmpv FFI, loaded dynamically from `libmpv-2.dll` (no build-time
//! linking). Exposes just what the player and the compositor need: a core
//! handle (commands / properties / events) and the render API (software now,
//! OpenGL-via-ANGLE next) used to composite video into the DirectComposition
//! visual under the transparent WebView2 overlay.
//!
//! This is the only place that touches libmpv's C ABI; everything above it is
//! safe Rust.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr::null_mut;
use std::sync::Arc;

// ── render-API parameter ABI (mpv/render.h) ─────────────────────────────
pub const MPV_RENDER_PARAM_INVALID: i32 = 0;
pub const MPV_RENDER_PARAM_API_TYPE: i32 = 1;
pub const MPV_RENDER_PARAM_SW_SIZE: i32 = 17;
pub const MPV_RENDER_PARAM_SW_FORMAT: i32 = 18;
pub const MPV_RENDER_PARAM_SW_STRIDE: i32 = 19;
pub const MPV_RENDER_PARAM_SW_POINTER: i32 = 20;
pub const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: i32 = 12;

#[repr(C)]
pub struct MpvRenderParam {
    pub kind: i32,
    pub data: *mut c_void,
}

// ── event ABI (mpv/client.h) ─────────────────────────────────────────────
pub const MPV_EVENT_NONE: c_int = 0;
pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_END_FILE_REASON_EOF: c_int = 0;

#[repr(C)]
pub struct MpvEvent {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct MpvEventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

// ── dynamically-loaded entry points ──────────────────────────────────────
type FnCreate = unsafe extern "C" fn() -> *mut c_void;
type FnInit = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnTerminate = unsafe extern "C" fn(*mut c_void);
type FnSetOpt = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnSetPropStr = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
type FnGetPropStr = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char;
type FnCommand = unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int;
type FnObserve = unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> c_int;
type FnWaitEvent = unsafe extern "C" fn(*mut c_void, f64) -> *mut MpvEvent;
type FnWakeup = unsafe extern "C" fn(*mut c_void);
type FnFree = unsafe extern "C" fn(*mut c_void);
type FnRenderCtxCreate =
    unsafe extern "C" fn(*mut *mut c_void, *mut c_void, *mut MpvRenderParam) -> c_int;
type FnRenderCtxRender = unsafe extern "C" fn(*mut c_void, *mut MpvRenderParam) -> c_int;
type FnRenderCtxFree = unsafe extern "C" fn(*mut c_void);

/// Resolved libmpv entry points (kept alive for the process via a leaked
/// `Library`, so the function pointers stay valid).
pub struct Api {
    pub create: FnCreate,
    pub initialize: FnInit,
    pub terminate_destroy: FnTerminate,
    pub set_option_string: FnSetOpt,
    pub set_property_string: FnSetPropStr,
    pub get_property_string: FnGetPropStr,
    pub command: FnCommand,
    pub observe_property: FnObserve,
    pub wait_event: FnWaitEvent,
    pub wakeup: FnWakeup,
    pub free: FnFree,
    pub render_context_create: FnRenderCtxCreate,
    pub render_context_render: FnRenderCtxRender,
    pub render_context_free: FnRenderCtxFree,
}

impl Api {
    /// Load `libmpv-2.dll` and resolve the entry points. The DLL is leaked
    /// (never unloaded) so the resolved function pointers remain valid.
    pub fn load() -> Result<Arc<Api>, String> {
        unsafe {
            let lib = libloading::Library::new("libmpv-2.dll").map_err(|e| e.to_string())?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).map_err(|e| e.to_string())?
                };
            }
            let api = Api {
                create: sym!("mpv_create"),
                initialize: sym!("mpv_initialize"),
                terminate_destroy: sym!("mpv_terminate_destroy"),
                set_option_string: sym!("mpv_set_option_string"),
                set_property_string: sym!("mpv_set_property_string"),
                get_property_string: sym!("mpv_get_property_string"),
                command: sym!("mpv_command"),
                observe_property: sym!("mpv_observe_property"),
                wait_event: sym!("mpv_wait_event"),
                wakeup: sym!("mpv_wakeup"),
                free: sym!("mpv_free"),
                render_context_create: sym!("mpv_render_context_create"),
                render_context_render: sym!("mpv_render_context_render"),
                render_context_free: sym!("mpv_render_context_free"),
            };
            std::mem::forget(lib);
            Ok(Arc::new(api))
        }
    }
}

/// A libmpv core handle. `Send`/`Sync`: libmpv's client API is documented as
/// safe to call from multiple threads, which is how the runner (commands),
/// control server (properties), and event pump all share one handle.
pub struct Handle {
    pub api: Arc<Api>,
    pub ctx: *mut c_void,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    /// Create + initialize a core configured for embedded render-API output
    /// (no window of its own).
    pub fn create(api: Arc<Api>) -> Result<Handle, String> {
        let ctx = unsafe { (api.create)() };
        if ctx.is_null() {
            return Err("mpv_create returned null".into());
        }
        let h = Handle { api, ctx };
        // Sensible defaults for the round-robin player; ignore individual failures.
        // `vo=libmpv` is critical: it routes ALL video through the embedded
        // render context. Without it mpv defaults to `vo=gpu`, which spawns
        // its own top-level window in addition to our composited render — the
        // user sees mpv's separate window flicker over the overlay.
        h.set_option("terminal", "no");
        h.set_option("config", "no");
        h.set_option("vo", "libmpv");
        h.set_option("force-window", "no");
        h.set_option("idle", "yes");
        h.set_option("keep-open", "no");
        h.set_option("hwdec", "no");
        let r = unsafe { (h.api.initialize)(h.ctx) };
        if r < 0 {
            return Err(format!("mpv_initialize failed: {r}"));
        }
        Ok(h)
    }

    pub fn set_option(&self, name: &str, value: &str) {
        if let (Ok(n), Ok(v)) = (CString::new(name), CString::new(value)) {
            unsafe { (self.api.set_option_string)(self.ctx, n.as_ptr(), v.as_ptr()) };
        }
    }

    pub fn set_property(&self, name: &str, value: &str) {
        if let (Ok(n), Ok(v)) = (CString::new(name), CString::new(value)) {
            unsafe { (self.api.set_property_string)(self.ctx, n.as_ptr(), v.as_ptr()) };
        }
    }

    pub fn get_property(&self, name: &str) -> Option<String> {
        let n = CString::new(name).ok()?;
        unsafe {
            let p = (self.api.get_property_string)(self.ctx, n.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.api.free)(p as *mut c_void);
            Some(s)
        }
    }

    /// Run an mpv command given as a NULL-terminated argument list.
    pub fn command(&self, args: &[&str]) {
        let cstrs: Vec<CString> = args.iter().filter_map(|a| CString::new(*a).ok()).collect();
        let mut ptrs: Vec<*const c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(null_mut());
        unsafe { (self.api.command)(self.ctx, ptrs.as_ptr()) };
    }

    pub fn observe_property_string(&self, reply: u64, name: &str) {
        // MPV_FORMAT_STRING = 1
        if let Ok(n) = CString::new(name) {
            unsafe { (self.api.observe_property)(self.ctx, reply, n.as_ptr(), 1) };
        }
    }

    /// Block up to `timeout` seconds for the next event (negative = forever).
    /// The returned pointer is owned by mpv and valid until the next call on
    /// this thread.
    pub fn wait_event(&self, timeout: f64) -> *mut MpvEvent {
        unsafe { (self.api.wait_event)(self.ctx, timeout) }
    }

    pub fn wakeup(&self) {
        unsafe { (self.api.wakeup)(self.ctx) };
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { (self.api.terminate_destroy)(self.ctx) };
    }
}
