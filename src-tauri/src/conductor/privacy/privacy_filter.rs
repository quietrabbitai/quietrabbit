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
//   2. ~/.local/share/quietrabbit/models/privacy-filter-q8_0.gguf
//
// Build setup (one-time on Garuda):
//   git clone https://github.com/localai-org/privacy-filter.cpp
//   cd privacy-filter.cpp
//   cmake --preset release-portable
//   cmake --build --preset release-portable -j
//   export PRIVACY_FILTER_LIB_DIR=<path>/build/release-portable/lib

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
    /// The text slice identified by the filter, extracted using byte offsets.
    /// Uses from_utf8_lossy — recovers partial text if offsets land mid-character.
    pub span_text: String,
}

// ---------------------------------------------------------------------------
// Live path — compiled only when PRIVACY_FILTER_LIB_DIR was set at build time
// ---------------------------------------------------------------------------

#[cfg(privacy_filter_available)]
mod inner {
    use std::ffi::{CStr, CString};
    use std::marker::{PhantomData, PhantomPinned};
    use std::sync::{Mutex, OnceLock};

    use super::PfEntityDecoded;

    // ABI version this wrapper was written against.
    // Verify this constant matches PF_ABI_VERSION in pf.h when updating the
    // library. Initialisation fails if pf_abi_version() returns a different value.
    const EXPECTED_PF_ABI_VERSION: u32 = 1;

    // -- Opaque C context type -----------------------------------------------
    //
    // PfCtx is an opaque handle. PhantomData<(*mut u8, PhantomPinned)> marks it
    // as !Send and !Sync — only PrivacyFilter (which wraps it behind a Mutex)
    // implements Send explicitly.
    #[repr(C)]
    pub struct PfCtx {
        _data:   [u8; 0],
        _marker: PhantomData<(*mut u8, PhantomPinned)>,
    }

    // -- C entity span struct (repr C, must match pf.h layout) ---------------
    #[repr(C)]
    pub struct PfEntity {
        pub start: libc::size_t,        // byte offset, inclusive
        pub end:   libc::size_t,        // byte offset, exclusive
        pub score: libc::c_float,       // confidence in [0.0, 1.0]
        pub label: *const libc::c_char, // static string owned by the library
    }

    // -- extern "C" declarations (pf.h) --------------------------------------
    extern "C" {
        /// Returns the library ABI version. Compared against EXPECTED_PF_ABI_VERSION.
        pub fn pf_abi_version() -> libc::c_uint;

        /// Initialise a Privacy Filter context.
        /// model_path: path to the GGUF model file (UTF-8, null-terminated).
        /// device:     NULL or "cpu" | "gpu" | "cuda" | "vulkan" (optionally ":N").
        /// n_threads:  <= 0 selects a default (CPU only).
        /// Returns NULL on failure; call pf_last_error for the reason.
        pub fn pf_init(
            model_path: *const libc::c_char,
            device:     *const libc::c_char,
            n_threads:  libc::c_int,
        ) -> *mut PfCtx;

        /// Free a context created by pf_init.
        pub fn pf_free(ctx: *mut PfCtx);

        /// Return the last error string for ctx, or NULL if none.
        /// The pointer is valid until the next pf_* call on this ctx.
        pub fn pf_last_error(ctx: *mut PfCtx) -> *const libc::c_char;

        /// Set the token window size (default 4096). Must be > 2048 to window.
        /// Longer inputs run as overlapping halo windows automatically.
        #[allow(dead_code)]
        pub fn pf_set_window(ctx: *mut PfCtx, max_forward_tokens: libc::c_int);

        /// Classify text and return entity spans as a malloc'd array.
        /// threshold: spans scoring below this value are dropped.
        /// *out:      set to the allocated array on success; free with pf_entities_free.
        /// *n:        set to the number of entities.
        /// Returns 0 on success, non-zero on error.
        pub fn pf_classify(
            ctx:       *mut PfCtx,
            text:      *const libc::c_char,
            threshold: libc::c_float,
            out:       *mut *mut PfEntity,
            n:         *mut libc::size_t,
        ) -> libc::c_int;

        /// Free the entity array allocated by pf_classify. NULL-safe.
        pub fn pf_entities_free(entities: *mut PfEntity);
    }

    // -- Safe Rust wrapper ---------------------------------------------------

    /// Owns a PfCtx pointer. Drop calls pf_free.
    pub struct PrivacyFilter {
        ctx: *mut PfCtx,
    }

    impl Drop for PrivacyFilter {
        fn drop(&mut self) {
            // Safety: ctx was returned by pf_init and has not been freed.
            unsafe { pf_free(self.ctx); }
        }
    }

    // Safety: PrivacyFilter is only ever accessed through the Mutex in
    // PF_INSTANCE — never shared between threads concurrently.
    unsafe impl Send for PrivacyFilter {}

    // -- Global singleton ----------------------------------------------------
    //
    // OnceLock<Option<Mutex<...>>>:
    //   Some(mutex)       = filter available and ready
    //   None              = initialisation was attempted and failed (permanent
    //                       until process restart — intentional for Release 1)
    static PF_INSTANCE: OnceLock<Option<Mutex<PrivacyFilter>>> = OnceLock::new();

    fn model_path_cstring() -> Option<CString> {
        // 1. Explicit env var override.
        if let Ok(path) = std::env::var("QR_PRIVACY_FILTER_MODEL") {
            return CString::new(path).ok();
        }
        // 2. XDG-aligned default in user data directory.
        let home = std::env::var("HOME").ok()?;
        let path = format!(
            "{home}/.local/share/quietrabbit/models/privacy-filter-q8_0.gguf"
        );
        CString::new(path).ok()
    }

    fn get_or_init() -> Option<&'static Mutex<PrivacyFilter>> {
        PF_INSTANCE
            .get_or_init(|| {
                // ABI check — fail before attempting model load if the library
                // version this wrapper was written against no longer matches.
                // Safety: pf_abi_version() has no preconditions.
                let abi = unsafe { pf_abi_version() };
                if abi != EXPECTED_PF_ABI_VERSION {
                    log::error!(
                        "privacy_filter: ABI mismatch — expected {EXPECTED_PF_ABI_VERSION}, \
                         got {abi}. Update EXPECTED_PF_ABI_VERSION in privacy_filter.rs \
                         and audit FFI declarations before using this library version."
                    );
                    return None;
                }
                log::info!("privacy_filter: ABI version {abi} confirmed");

                let model_path = match model_path_cstring() {
                    Some(p) => p,
                    None => {
                        log::warn!("privacy_filter: could not resolve model path");
                        return None;
                    }
                };

                // Safety: model_path is a valid null-terminated C string.
                let ctx = unsafe {
                    pf_init(
                        model_path.as_ptr(),
                        std::ptr::null(), // device: NULL → library default (cpu)
                        0,               // n_threads: 0 → library default
                    )
                };

                if ctx.is_null() {
                    log::warn!(
                        "privacy_filter: pf_init returned null — \
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
        text:      &str,
        threshold: f32,
    ) -> Result<Vec<PfEntityDecoded>, String> {
        let mutex = get_or_init()
            .ok_or_else(|| "Privacy Filter unavailable".to_owned())?;

        let guard = mutex
            .lock()
            .map_err(|e| format!("Privacy Filter mutex poisoned: {e}"))?;

        let c_text = CString::new(text)
            .map_err(|e| format!("text contains interior null byte: {e}"))?;

        let mut out_ptr: *mut PfEntity = std::ptr::null_mut();
        let mut n: libc::size_t = 0;

        // Safety: ctx is valid (initialised in get_or_init, freed only on Drop).
        // c_text is a valid null-terminated string. out_ptr and n are valid
        // stack locations for the output parameters.
        let rc = unsafe {
            pf_classify(
                guard.ctx,
                c_text.as_ptr(),
                threshold as libc::c_float,
                &mut out_ptr,
                &mut n,
            )
        };

        if rc != 0 {
            let reason = unsafe {
                let p = pf_last_error(guard.ctx);
                if p.is_null() {
                    "unknown error (pf_last_error returned null)".to_owned()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            return Err(format!("pf_classify failed (rc={rc}): {reason}"));
        }

        // Decode all spans into owned Rust values before freeing the C buffer.
        // label pointers inside PfEntity are static strings in the library —
        // we copy them to produce fully owned PfEntityDecoded values.
        let entities: Vec<PfEntityDecoded> = if out_ptr.is_null() || n == 0 {
            Vec::new()
        } else {
            // Safety: out_ptr is a valid array of n PfEntity values allocated
            // by pf_classify. Read before pf_entities_free.
            let slice = unsafe { std::slice::from_raw_parts(out_ptr, n) };
            let text_bytes = text.as_bytes();

            slice.iter().map(|e| {
                let label = unsafe {
                    if e.label.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(e.label).to_string_lossy().into_owned()
                    }
                };

                // Clamp byte offsets to valid range before slicing.
                let start = (e.start as usize).min(text_bytes.len());
                let end   = (e.end   as usize).min(text_bytes.len());

                // from_utf8_lossy recovers partial text if the C library returns
                // offsets that land mid-UTF-8 character (replaces invalid bytes
                // with U+FFFD rather than discarding the span entirely).
                let span_text = String::from_utf8_lossy(&text_bytes[start..end])
                    .into_owned();

                PfEntityDecoded {
                    start_byte: e.start as usize,
                    end_byte:   e.end   as usize,
                    score:      e.score,
                    label,
                    span_text,
                }
            }).collect()
        };

        // Free the C-allocated array. pf_entities_free is NULL-safe per docs,
        // but we guard here for explicitness.
        if !out_ptr.is_null() {
            unsafe { pf_entities_free(out_ptr); }
        }

        Ok(entities)
    }
} // end mod inner

// Re-export live implementations when compiled in.
#[cfg(privacy_filter_available)]
pub use inner::{is_available, run_classify_blocking};

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
pub fn run_classify_blocking(
    _text:      &str,
    _threshold: f32,
) -> Result<Vec<PfEntityDecoded>, String> {
    Err(
        "Privacy Filter not compiled in \
         (set PRIVACY_FILTER_LIB_DIR at build time)"
            .to_owned(),
    )
}

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
            end_byte:   20,
            score:      0.95,
            label:      "private_person".to_owned(),
            span_text:  "Alice Smith".to_owned(),
        };
        assert_eq!(e.start_byte, 10);
        assert_eq!(e.end_byte, 20);
        assert!((e.score - 0.95).abs() < f32::EPSILON);
        assert_eq!(e.label, "private_person");
        assert_eq!(e.span_text, "Alice Smith");
    }
}
