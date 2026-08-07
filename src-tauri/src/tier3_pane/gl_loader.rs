//! GL proc-address loading for single-window compositing (items.id=202 real
//! positioning fix, 2026-08-07).
//!
//! The standard gtk-rs pattern for this is the `epoxy` crate (GTK itself
//! loads GL via libepoxy on Linux, so a Rust caller doing its own GL calls
//! inside a `GLArea` is expected to dispatch through the same loader).
//! Dropped after `cargo build` found it unbuildable via crates.io as of this
//! session: `epoxy`'s `gl_generator` build-dependency requires
//! `xml-rs = "^0.7.0"`, and every published 0.7.x version of `xml-rs` is
//! yanked -- dependency resolution fails outright, independent of which
//! `epoxy` version is requested.
//!
//! This dlopen's `libepoxy.so` directly via `libloading` (a stable,
//! actively-maintained crate, unaffected by the above) and resolves
//! `epoxy_get_proc_address` -- libepoxy's own single stable C entry point --
//! by hand. No codegen, no `gl_generator`. The resulting loader closure feeds
//! both `wgpu_hal::gles::Adapter::new_external` (render.rs) and a `glow`
//! `Context` (kept here) for the handful of raw GL calls single-window
//! compositing needs outside wgpu's own abstraction --
//! `GL_DRAW_FRAMEBUFFER_BINDING` capture/rebind around wgpu's draw calls,
//! since wgpu-hal does not preserve the host's framebuffer binding
//! (confirmed this session; see pane_host.rs's `GLArea::render` handler).

use std::ffi::{c_void, CString};

/// Owns the `libepoxy` handle for the process lifetime (leaked deliberately
/// via `Box::leak` inside `open()`'s caller pattern is NOT done here --
/// callers keep this alive for as long as they need proc-address
/// resolution, matching the GLArea's own realized lifetime).
pub struct EpoxyLoader {
    _lib: libloading::Library,
    get_proc_address: unsafe extern "C" fn(*const std::os::raw::c_char) -> *const c_void,
}

impl EpoxyLoader {
    /// # Safety-relevant note
    /// Must run on the main thread, with GTK already initialized -- GTK's
    /// own GL/EGL setup is what guarantees `libepoxy.so` is already loaded
    /// into the process (dlopen here just resolves a handle to it, does not
    /// perform first-load initialization).
    pub fn open() -> Self {
        let lib = unsafe {
            libloading::Library::new("libepoxy.so.0")
                .or_else(|_| libloading::Library::new("libepoxy.so"))
                .expect(
                    "tier3_pane::gl_loader: could not dlopen libepoxy -- required for GTK GL \
                     interop on Linux",
                )
        };
        let get_proc_address = unsafe {
            *lib.get::<unsafe extern "C" fn(*const std::os::raw::c_char) -> *const c_void>(
                b"epoxy_get_proc_address\0",
            )
            .expect("tier3_pane::gl_loader: epoxy_get_proc_address symbol not found in libepoxy")
        };
        Self {
            _lib: lib,
            get_proc_address,
        }
    }

    pub fn get_proc_address(&self, name: &str) -> *const c_void {
        let cname = CString::new(name).unwrap_or_default();
        unsafe { (self.get_proc_address)(cname.as_ptr()) }
    }

    /// A loader closure suitable for
    /// `wgpu_hal::gles::Adapter::new_external`/`glow::Context::from_loader_function`
    /// -- both take `impl FnMut(&str) -> *const c_void`. Each call site
    /// should open its own `EpoxyLoader` (dlopen of an already-loaded
    /// shared object is cheap -- the OS returns the cached handle) rather
    /// than share one across the two consumers, avoiding any ownership
    /// entanglement between wgpu-hal's internal `glow::Context` and this
    /// module's own separate one.
    pub fn loader_fn(&self) -> impl FnMut(&str) -> *const c_void + '_ {
        move |name: &str| self.get_proc_address(name)
    }
}
