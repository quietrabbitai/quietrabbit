// src-tauri/src/conductor/privacy/privacy_filter.rs
//
// FFI wrapper for privacy-filter.cpp (localai-org/privacy-filter.cpp).
// Provides a safe Rust interface over the flat C API (pf.h).
//
// Compiled only when PRIVACY_FILTER_LIB_DIR is set at build time
// (cargo:rustc-cfg=privacy_filter_available emitted by build.rs).
// When not compiled in, the stub functions return Err/false so gate3.rs
// can call them unconditionally without scattering cfg attributes.
//
// Usage: always call run_classify_blocking() from within
// tokio::task::spawn_blocking — it holds a Mutex lock for the duration
// of the C library call and must not block the async executor.
//
// Model path resolution (in order):
//   1. QR_PRIVACY_FILTER_MODEL environment variable
//   2. ~/.local/share/quietrabbit/models/privacy-filter-q8.gguf
//
// Build setup (one-time on Garuda):
//   git clone https://github.com/localai-org/privacy-filter.cpp
//   cd privacy-filter.cpp
//   cmake --preset release-portable
//   cmake --build --preset release-portable -j
//   export PRIVACY_FILTER_LIB_DIR=<path>/build/release-portable
//                                          ^^^^ build root, not lib/

// ---------------------------------------------------------------------------
// Shared types (always compiled — stub and live paths share the same API)
// ---------------------------------------------------------------------------

/// Owned Rust representation of a single Privacy Filter entity span.
/// Populated after C→Rust conversion, before pf_entities_free is called.
/// Byte offsets index into the original UTF-8 text passed to run_classify_blocking.
#[derive(Debug, Clone)]
pub struct PfEntityDecoded {
    /// Start byte offset in the original text (inclusive).
    pub start_byte: usize,
    /// End byte offset in the original text (exclusive).
    pub end_byte: usize,
    /// Confidence score in [0.0, 1.0].
    pub score: f32,
    /// PF category label — one of the eight base taxonomy labels:
    ///   "private_person" | "private_address" | "private_email" |
    ///   "private_phone"  | "private_url"     | "private_date"  |
    ///   "account_number" | "secret"
    /// Empty string if the label pointer was null (defensive; should not occur).
    pub label: String,
    /// Extracted from the original text using byte offsets returned by the
    /// privacy-filter.cpp library. Offsets are clamped to valid byte range
    /// before slicing; assumes the library returns valid UTF-8 boundaries
    /// (from_utf8_lossy replaces any invalid bytes rather than panicking).
    pub span_text: String,
}

// ---------------------------------------------------------------------------
// Live path — compiled only when PRIVACY_FILTER_LIB_DIR was set at build time
// ---------------------------------------------------------------------------

#[cfg(privacy_filter_available)]
mod inner {
    use std::ffi::{CStr, CString};
    use std::marker::{PhantomData, PhantomPinned};
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use super::PfEntityDecoded;

    // Directory ggml should search for backend .so files (CPU-ISA variants,
    // GPU backends). Set once via set_backend_dir() — from main.rs's setup()
    // hook, using Tauri's resource dir — before the first pf_load(). Falls
    // back to the build-time-baked dev bin/ dir (see build.rs) for contexts
    // with no Tauri AppHandle, e.g. `cargo test` or a standalone harness.
    //
    // ggml's own default search (compile-time GGML_BACKEND_DIR, then the
    // running executable's directory, then cwd — see privacy-filter.cpp's
    // ggml-backend-reg.cpp) does not resolve correctly for a Tauri app,
    // whose resource directory varies by install/bundle format and isn't
    // known at privacy-filter.cpp's own build time. Hence this override,
    // threaded through pf_set_backend_dir (added to pf.h for this purpose).
    static BACKEND_DIR: OnceLock<PathBuf> = OnceLock::new();

    /// Set the directory ggml searches for backend .so files. Must be called
    /// before the first is_available()/run_classify_blocking() call in the
    /// process — the underlying loader runs once, lazily, on first use, and
    /// is cached for the process lifetime (see pf_set_backend_dir in pf.h).
    /// Second and later calls are ignored (OnceLock — logs a warning).
    pub fn set_backend_dir(dir: PathBuf) {
        if BACKEND_DIR.set(dir.clone()).is_err() {
            log::warn!(
                "privacy_filter: set_backend_dir({dir:?}) ignored — \
                 already set to {:?}",
                BACKEND_DIR.get()
            );
        }
    }

    /// Compile-time fallback for dev/test/example contexts that never call
    /// set_backend_dir() (no Tauri AppHandle to resolve a resource dir from).
    /// Baked by build.rs from PRIVACY_FILTER_LIB_DIR/bin at compile time.
    const BACKEND_DIR_DEV_FALLBACK: &str = env!("PRIVACY_FILTER_BACKEND_DIR_DEV");

    // ABI version this wrapper was written against.
    // Matches PF_ABI_VERSION in pf.h. Initialisation fails if the runtime
    // library returns a different value from pf_abi_version().
    const EXPECTED_PF_ABI_VERSION: u32 = 1;

    // -- Opaque C context type -----------------------------------------------
    //
    // PfCtx is an opaque handle. PhantomData<(*mut u8, PhantomPinned)> marks it
    // as !Send and !Sync — only PrivacyFilter (which wraps it behind a Mutex)
    // implements Send explicitly.
    #[repr(C)]
    pub struct PfCtx {
        _data: [u8; 0],
        _marker: PhantomData<(*mut u8, PhantomPinned)>,
    }

    // -- C entity span struct (repr C, must match pf.h layout) ---------------
    // pf.h: typedef struct { int32_t start; int32_t end; float score; const char* label; } pf_entity;
    #[repr(C)]
    pub struct PfEntity {
        pub start: i32,                 // int32_t — byte offset, inclusive
        pub end: i32,                   // int32_t — byte offset, exclusive
        pub score: libc::c_float,       // float   — confidence in [0.0, 1.0]
        pub label: *const libc::c_char, // const char* — ctx-owned, valid until pf_free
    }

    // -- extern "C" declarations matching pf.h exactly -----------------------
    extern "C" {
        /// Returns the library ABI version (int in pf.h).
        /// Compare as u32 against EXPECTED_PF_ABI_VERSION.
        pub fn pf_abi_version() -> libc::c_int;

        /// Override the directory ggml searches for backend .so files.
        /// Must be called before the first pf_load() in the process —
        /// the underlying search happens once, lazily, on first load.
        /// NULL restores ggml's built-in default search.
        pub fn pf_set_backend_dir(dir_path: *const libc::c_char);

        /// Initialise a Privacy Filter context.
        /// gguf_path: path to the GGUF model file (UTF-8, null-terminated).
        /// device:    NULL or "cpu" | "gpu" | "cuda" | "vulkan" (optionally ":N").
        /// n_threads: <= 0 selects a default (CPU only).
        /// Returns NULL on failure; call pf_last_error for the reason.
        pub fn pf_load(
            gguf_path: *const libc::c_char,
            device: *const libc::c_char,
            n_threads: libc::c_int,
        ) -> *mut PfCtx;

        /// Free a context created by pf_load.
        pub fn pf_free(ctx: *mut PfCtx);

        /// Return the last error string for ctx, or NULL if none.
        /// Takes const pointer — ctx is not mutated. Pointer valid until next
        /// pf_* call on this ctx.
        pub fn pf_last_error(ctx: *const PfCtx) -> *const libc::c_char;

        /// Set the token window size (default 4096). Must be > 2048 to window.
        /// Longer inputs run as overlapping halo windows automatically.
        #[allow(dead_code)]
        pub fn pf_set_window(ctx: *mut PfCtx, max_forward_tokens: i32);

        /// Classify text and return entity spans as a malloc'd array.
        /// text:      UTF-8 input, not required to be null-terminated (len provided).
        /// len:       byte length of text.
        /// threshold: spans scoring below this value are dropped.
        /// out:       set to the allocated array on success.
        /// n_out:     set to the number of entities.
        /// Returns 0 on success, non-zero on error.
        pub fn pf_classify(
            ctx: *mut PfCtx,
            text: *const libc::c_char,
            len: libc::size_t,
            threshold: libc::c_float,
            out: *mut *mut PfEntity,
            n_out: *mut libc::size_t,
        ) -> libc::c_int;

        /// Free the entity array allocated by pf_classify.
        /// Requires both pointer and count (NULL-safe per pf.h).
        pub fn pf_entities_free(ents: *mut PfEntity, n: libc::size_t);
    }

    // -- Safe Rust wrapper ---------------------------------------------------

    /// Owns a PfCtx pointer. Drop calls pf_free.
    pub struct PrivacyFilter {
        ctx: *mut PfCtx,
    }

    impl Drop for PrivacyFilter {
        fn drop(&mut self) {
            // Safety: ctx was returned by pf_load and has not been freed.
            unsafe {
                pf_free(self.ctx);
            }
        }
    }

    // Safety: PrivacyFilter is only ever accessed through the Mutex in
    // PF_INSTANCE — never shared between threads concurrently.
    unsafe impl Send for PrivacyFilter {}

    // -- Global singleton ----------------------------------------------------
    //
    // OnceLock<Option<Mutex<...>>>:
    //   Some(mutex) = filter available and ready
    //   None        = initialisation attempted and failed (permanent until
    //                 process restart — intentional for Release 1)
    static PF_INSTANCE: OnceLock<Option<Mutex<PrivacyFilter>>> = OnceLock::new();

    fn model_path_cstring() -> Option<CString> {
        // 1. Explicit env var override.
        if let Ok(path) = std::env::var("QR_PRIVACY_FILTER_MODEL") {
            return CString::new(path).ok();
        }
        // 2. XDG-aligned default in user data directory.
        let home = std::env::var("HOME").ok()?;
        let path = format!("{home}/.local/share/quietrabbit/models/privacy-filter-q8.gguf");
        CString::new(path).ok()
    }

    fn get_or_init() -> Option<&'static Mutex<PrivacyFilter>> {
        PF_INSTANCE
            .get_or_init(|| {
                // ABI check — fail before model load if library version changed.
                // Safety: pf_abi_version() has no preconditions.
                let abi = unsafe { pf_abi_version() } as u32;
                if abi != EXPECTED_PF_ABI_VERSION {
                    log::error!(
                        "privacy_filter: ABI mismatch — expected {EXPECTED_PF_ABI_VERSION}, \
                         got {abi}. Update EXPECTED_PF_ABI_VERSION in privacy_filter.rs \
                         and audit FFI declarations before using this library version."
                    );
                    return None;
                }
                log::info!("privacy_filter: ABI version {abi} confirmed");

                // Must happen before pf_load — see BACKEND_DIR doc comment.
                // BACKEND_DIR is populated by set_backend_dir() (called from
                // main.rs's setup() hook with Tauri's resolved resource dir)
                // if this process has a Tauri AppHandle; otherwise fall back
                // to the build-time-baked dev bin/ dir (calibration harness,
                // `cargo test`, etc).
                let backend_dir = BACKEND_DIR
                    .get()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| BACKEND_DIR_DEV_FALLBACK.to_owned());
                match CString::new(backend_dir.clone()) {
                    Ok(c_dir) => unsafe { pf_set_backend_dir(c_dir.as_ptr()) },
                    Err(e) => log::warn!(
                        "privacy_filter: backend dir {backend_dir:?} has interior \
                         null byte, cannot set ({e}) — falling back to ggml's \
                         built-in search"
                    ),
                }

                let model_path = match model_path_cstring() {
                    Some(p) => p,
                    None => {
                        log::warn!("privacy_filter: could not resolve model path");
                        return None;
                    }
                };

                // Safety: model_path is a valid null-terminated C string.
                let ctx = unsafe {
                    pf_load(
                        model_path.as_ptr(),
                        std::ptr::null(), // device: NULL → library default (cpu)
                        0,                // n_threads: 0 → library default
                    )
                };

                if ctx.is_null() {
                    log::warn!(
                        "privacy_filter: pf_load returned null — \
                         model missing or path incorrect"
                    );
                    return None;
                }

                log::info!("privacy_filter: initialised successfully");
                Some(Mutex::new(PrivacyFilter { ctx }))
            })
            .as_ref()
    }

    // -- Public API ----------------------------------------------------------

    /// Returns true if the Privacy Filter library was compiled in and the
    /// GGUF model loaded successfully on first call.
    pub fn is_available() -> bool {
        get_or_init().is_some()
    }

    /// Run the Privacy Filter classifier on text. Returns decoded entity spans.
    ///
    /// Synchronous — MUST be called inside tokio::task::spawn_blocking.
    /// Holds the PrivacyFilter Mutex for the duration of the C call.
    ///
    /// Pass threshold=0.0 to receive all spans regardless of confidence.
    /// Gate3 applies its own tier routing on top of the scores (D6-362).
    ///
    /// Returns Ok(vec) on success; vec may be empty if no spans found.
    /// Returns Err(reason) if the filter is unavailable or the call fails.
    pub fn run_classify_blocking(
        text: &str,
        threshold: f32,
    ) -> Result<Vec<PfEntityDecoded>, String> {
        let mutex = get_or_init().ok_or_else(|| "Privacy Filter unavailable".to_owned())?;

        let guard = mutex
            .lock()
            .map_err(|e| format!("Privacy Filter mutex poisoned: {e}"))?;

        // pf_classify does not require null termination (takes explicit len),
        // but CString is still used to satisfy *const c_char type. The byte
        // slice length is passed separately.
        let c_text =
            CString::new(text).map_err(|e| format!("text contains interior null byte: {e}"))?;
        let text_len = text.len();

        let mut out_ptr: *mut PfEntity = std::ptr::null_mut();
        let mut n: libc::size_t = 0;

        // Safety: ctx is valid (initialised in get_or_init, freed only on Drop).
        // c_text is a valid C string; text_len matches the original str length.
        // out_ptr and n are valid stack locations for the output parameters.
        let rc = unsafe {
            pf_classify(
                guard.ctx,
                c_text.as_ptr(),
                text_len as libc::size_t,
                threshold as libc::c_float,
                &mut out_ptr,
                &mut n,
            )
        };

        if rc != 0 {
            let reason = unsafe {
                // pf_last_error takes *const PfCtx.
                let p = pf_last_error(guard.ctx as *const PfCtx);
                if p.is_null() {
                    "unknown error (pf_last_error returned null)".to_owned()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            return Err(format!("pf_classify failed (rc={rc}): {reason}"));
        }

        // Decode all spans into owned Rust values before freeing the C buffer.
        // label pointers inside PfEntity are ctx-owned strings — copy them now.
        let entities: Vec<PfEntityDecoded> = if out_ptr.is_null() || n == 0 {
            Vec::new()
        } else {
            // Safety: out_ptr is a valid array of n PfEntity values allocated
            // by pf_classify. Read before pf_entities_free.
            let slice = unsafe { std::slice::from_raw_parts(out_ptr, n) };
            let text_bytes = text.as_bytes();

            slice
                .iter()
                .map(|e| {
                    let label = unsafe {
                        if e.label.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(e.label).to_string_lossy().into_owned()
                        }
                    };

                    // i32 offsets — clamp to valid byte range before slicing.
                    let start = (e.start.max(0) as usize).min(text_bytes.len());
                    let end = (e.end.max(0) as usize).min(text_bytes.len());

                    let span_text = String::from_utf8_lossy(&text_bytes[start..end]).into_owned();

                    PfEntityDecoded {
                        start_byte: e.start as usize,
                        end_byte: e.end as usize,
                        score: e.score,
                        label,
                        span_text,
                    }
                })
                .collect()
        };

        // Free the C-allocated array. pf_entities_free requires both pointer
        // and count (matches pf.h signature).
        if !out_ptr.is_null() {
            unsafe {
                pf_entities_free(out_ptr, n);
            }
        }

        Ok(entities)
    }
} // end mod inner

// Re-export live implementations when compiled in.
#[cfg(privacy_filter_available)]
pub use inner::{is_available, run_classify_blocking, set_backend_dir};

// ---------------------------------------------------------------------------
// Stub path — when PRIVACY_FILTER_LIB_DIR was not set at build time
// ---------------------------------------------------------------------------

/// Returns false when the Privacy Filter library was not compiled in.
#[cfg(not(privacy_filter_available))]
pub fn is_available() -> bool {
    false
}

/// Returns Err when the Privacy Filter library was not compiled in.
/// Gate3 calls this and falls back to pre-filter sensitivity block.
#[cfg(not(privacy_filter_available))]
pub fn run_classify_blocking(_text: &str, _threshold: f32) -> Result<Vec<PfEntityDecoded>, String> {
    Err("Privacy Filter not compiled in \
         (set PRIVACY_FILTER_LIB_DIR at build time)"
        .to_owned())
}

/// No-op when the Privacy Filter library was not compiled in — lets callers
/// (main.rs's setup hook) call this unconditionally without cfg-gating.
#[cfg(not(privacy_filter_available))]
pub fn set_backend_dir(_dir: std::path::PathBuf) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(privacy_filter_available))]
    #[test]
    fn stub_is_available_returns_false() {
        assert!(!is_available());
    }

    #[cfg(not(privacy_filter_available))]
    #[test]
    fn stub_run_classify_returns_err() {
        let result = run_classify_blocking("Hello, my name is Alice.", 0.5);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("not compiled in"), "unexpected error: {msg}");
    }

    #[test]
    fn pf_entity_decoded_fields_accessible() {
        let e = PfEntityDecoded {
            start_byte: 10,
            end_byte: 20,
            score: 0.95,
            label: "private_person".to_owned(),
            span_text: "Alice Smith".to_owned(),
        };
        assert_eq!(e.start_byte, 10);
        assert_eq!(e.end_byte, 20);
        assert!((e.score - 0.95).abs() < f32::EPSILON);
        assert_eq!(e.label, "private_person");
        assert_eq!(e.span_text, "Alice Smith");
    }
}
