//! GL proc-address loading for single-window compositing (items.id=202 real
//! positioning fix, 2026-08-07; loader rewritten items.id=225, 2026-08-08).
//!
//! The standard gtk-rs pattern for this is the `epoxy` crate (GTK itself
//! loads GL via libepoxy on Linux, so a Rust caller doing its own GL calls
//! inside a `GLArea` is expected to dispatch through the same loader).
//! Dropped after `cargo build` found it unbuildable via crates.io as of the
//! original session: `epoxy`'s `gl_generator` build-dependency requires
//! `xml-rs = "^0.7.0"`, and every published 0.7.x version of `xml-rs` is
//! yanked -- dependency resolution fails outright, independent of which
//! `epoxy` version is requested.
//!
//! REWRITTEN (items.id=225, 2026-08-08): the original hand-rolled
//! replacement dlopen'd `libepoxy.so` and resolved a symbol named
//! `epoxy_get_proc_address`, describing it as "libepoxy's own single stable
//! C entry point". That symbol does not exist -- confirmed against the real
//! installed library (`nm -D /usr/lib/libepoxy.so.0`, libepoxy 1.5.10):
//! libepoxy exports per-function version/extension-check helpers
//! (`epoxy_gl_version`, `epoxy_has_gl_extension`, ...) but no generic
//! lookup-by-name entry point at all -- real libepoxy dispatches per GL
//! function internally via generated headers, not a callable-from-outside
//! resolver. The old code crashed (`panic in a function that cannot
//! unwind`, aborting the process) the first time it actually ran -- masked
//! until this session because of a separate bug (pane_host.rs's signal
//! handlers were connected after `show_all()` already fired `realize`, so
//! this code path had never once executed in the live app).
//!
//! This session's dev environment confirmed native Wayland
//! (`XDG_SESSION_TYPE=wayland`, `GDK_BACKEND=wayland`) -- GTK's Wayland
//! backend has no GLX equivalent (GLX is X11-only), so `GtkGLArea`'s context
//! here is EGL-backed. The replacement loader uses the two real, standard,
//! confirmed-present entry points for that stack instead of a fictional
//! generic epoxy call:
//!   1. `dlsym` directly against `libGLESv2.so` for core GL(ES) functions
//!      (e.g. `glClear`, `glDrawArrays` are real, directly dlsym-able
//!      exported symbols -- confirmed via `nm -D`).
//!   2. `eglGetProcAddress` (real, standard, confirmed-exported symbol in
//!      `libEGL.so.1`) as the fallback, for extension functions -- per the
//!      EGL spec, extensions are not guaranteed plain-dlsym-able, only
//!      reachable via `eglGetProcAddress`.
//!
//! This dlsym-first-then-eglGetProcAddress order is the standard pattern
//! real GL loaders (SDL2, GLFW, ANGLE) use on EGL platforms, not a
//! one-off guess for this bug.
//!
//! The resulting loader closure feeds both `wgpu_hal::gles::Adapter::
//! new_external` (render.rs) and a `glow` `Context` (kept here) for the
//! handful of raw GL calls single-window compositing needs outside wgpu's
//! own abstraction -- `GL_DRAW_FRAMEBUFFER_BINDING` capture/rebind around
//! wgpu's draw calls, since wgpu-hal does not preserve the host's
//! framebuffer binding (confirmed the original session; see pane_host.rs's
//! `GLArea::render` handler).

use std::ffi::{c_void, CStr, CString};

/// Owns the `libGLESv2`/`libEGL` handles for the process lifetime. Callers
/// keep this alive for as long as they need proc-address resolution,
/// matching the GLArea's own realized lifetime -- same convention as the
/// module this replaces.
pub struct GlProcLoader {
    gles: libloading::Library,
    _egl: libloading::Library,
    egl_get_proc_address: unsafe extern "C" fn(*const std::os::raw::c_char) -> *const c_void,
}

/// Raw `dlsym`-style lookup against an already-open library. `Library::get`
/// is generic over the symbol's type; typing it as a plain pointer (rather
/// than a specific function-pointer signature) is the same pattern other
/// dlsym-based GL loaders (e.g. glutin's) use for this exact purpose --
/// dlsym itself is signature-agnostic, it just hands back an address.
fn dlsym_raw(lib: &libloading::Library, name: &CStr) -> Option<*const c_void> {
    unsafe {
        lib.get::<*const c_void>(name.to_bytes_with_nul())
            .ok()
            .map(|sym| *sym)
    }
}

impl GlProcLoader {
    /// # Safety-relevant note
    /// Must run on the main thread, with GTK already initialized and the
    /// GLArea's context current (`area.make_current()` called first) --
    /// GTK's own GL/EGL setup is what guarantees `libGLESv2.so`/`libEGL.so`
    /// are already loaded into the process (dlopen here just resolves a
    /// handle to them, does not perform first-load initialization).
    pub fn open() -> Self {
        let gles = unsafe {
            libloading::Library::new("libGLESv2.so.2")
                .or_else(|_| libloading::Library::new("libGLESv2.so"))
                .expect(
                    "tier3_pane::gl_loader: could not dlopen libGLESv2 -- required for GTK GL \
                     interop on Linux/EGL",
                )
        };
        let egl = unsafe {
            libloading::Library::new("libEGL.so.1")
                .or_else(|_| libloading::Library::new("libEGL.so"))
                .expect(
                    "tier3_pane::gl_loader: could not dlopen libEGL -- required for GTK GL \
                     interop on Linux/EGL",
                )
        };
        let egl_get_proc_address = unsafe {
            *egl.get::<unsafe extern "C" fn(*const std::os::raw::c_char) -> *const c_void>(
                b"eglGetProcAddress\0",
            )
            .expect(
                "tier3_pane::gl_loader: eglGetProcAddress symbol not found in libEGL -- \
                 confirmed present via `nm -D` this session, so a missing symbol here means \
                 a different libEGL than the one checked is being loaded",
            )
        };
        Self {
            gles,
            _egl: egl,
            egl_get_proc_address,
        }
    }

    pub fn get_proc_address(&self, name: &str) -> *const c_void {
        let cname = CString::new(name).unwrap_or_default();
        // Core functions: real, directly dlsym-able symbols in libGLESv2.
        if let Some(ptr) = dlsym_raw(&self.gles, &cname) {
            if !ptr.is_null() {
                return ptr;
            }
        }
        // Extension functions: not guaranteed dlsym-able per the EGL spec,
        // must go through eglGetProcAddress.
        unsafe { (self.egl_get_proc_address)(cname.as_ptr()) }
    }

    /// A loader closure suitable for
    /// `wgpu_hal::gles::Adapter::new_external`/`glow::Context::from_loader_function`
    /// -- both take `impl FnMut(&str) -> *const c_void`.
    ///
    /// CORRECTED (items.id=226, 2026-08-08): this used to recommend that
    /// each call site open its own `GlProcLoader` rather than share one.
    /// That was the root cause of a real SIGSEGV, confirmed via gdb against
    /// a coredump: both consumers only call the loader closure at
    /// construction time to eagerly resolve and cache raw function
    /// pointers, then never touch it again -- they do not keep the
    /// `GlProcLoader` (or its `libloading::Library` handles) alive
    /// themselves. A `GlProcLoader` dropped right after use `dlclose`s
    /// `libGLESv2.so.2`/`libEGL.so.1`, and once nothing else in the process
    /// independently references them (confirmed: nothing else in this
    /// GTK/EGL/Mesa stack loads `libGLESv2.so.2` directly), that's a real
    /// `munmap`, not just a refcount decrement -- every pointer already
    /// cached by either consumer is left dangling. The one instance this
    /// module's own struct doc already says to keep alive for the GLArea's
    /// realized lifetime must in fact be the *same* instance shared by
    /// every consumer that resolves pointers from it during that lifetime,
    /// not one each.
    pub fn loader_fn(&self) -> impl FnMut(&str) -> *const c_void + '_ {
        move |name: &str| self.get_proc_address(name)
    }
}
