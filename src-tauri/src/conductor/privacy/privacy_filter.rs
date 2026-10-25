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

use std::borrow::Cow;

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
    /// privacy-filter.cpp library. Offsets are checked by validate_span
    /// before slicing (a span with a negative offset, end < start or start
    /// past the text fails the whole classify call; an end past the text is
    /// clamped); assumes the library returns valid UTF-8 boundaries
    /// (from_utf8_lossy replaces any invalid bytes rather than panicking).
    pub span_text: String,
}

// ---------------------------------------------------------------------------
// Fail-closed helpers (always compiled, pure — items.id=613)
// ---------------------------------------------------------------------------

/// Result of a Privacy Filter classify call once the timeout has been handled.
/// Anything that is not a successful classification is `Failed`; callers must
/// fail closed on it.
#[derive(Debug)]
pub(crate) enum PfOutcome {
    Entities(Vec<PfEntityDecoded>),
    /// `reason` is for logs only: offsets, return codes and library error
    /// text, never the classified text.
    Failed {
        reason: String,
    },
}

/// The one mapping from `run_classify_blocking`'s nested result. A classifier
/// Err (poisoned mutex, NUL, rc != 0, bad span, unavailable) and a JoinError
/// (panic) are both `Failed`. The timeout is the outer `tokio::time::timeout`
/// Err, not part of this result, and stays a separate arm in each caller.
pub(crate) fn classify_outcome(
    result: Result<Result<Vec<PfEntityDecoded>, String>, tokio::task::JoinError>,
) -> PfOutcome {
    match result {
        Ok(Ok(entities)) => PfOutcome::Entities(entities),
        Ok(Err(reason)) => PfOutcome::Failed { reason },
        // JoinError's Display can carry a panic message; say only which kind.
        Err(join_err) if join_err.is_panic() => PfOutcome::Failed {
            reason: "Privacy Filter task panicked".to_owned(),
        },
        Err(_) => PfOutcome::Failed {
            reason: "Privacy Filter task was cancelled".to_owned(),
        },
    }
}

/// Replace each NUL with a space. NUL and space are one byte each, so byte
/// length and every char boundary are unchanged and span offsets stay valid.
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) fn sanitize_for_pf(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', " "))
    } else {
        Cow::Borrowed(text)
    }
}

/// Validate one classifier span against the text length. A span the
/// classifier flagged is never dropped (a dropped span is a missed finding):
/// anything unusable is an Err, which makes the whole classify call fail.
/// `end` past `len` clamps to `len`, so the span still covers the tail.
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) fn validate_span(start: i32, end: i32, len: usize) -> Result<(usize, usize), String> {
    if start < 0 || end < 0 {
        return Err(format!(
            "span has a negative offset (start={start}, end={end})"
        ));
    }
    // Both non-negative i32: the casts are lossless.
    let (start, end) = (start as usize, end as usize);
    if end < start {
        return Err(format!("span end {end} is before start {start}"));
    }
    if start > len {
        return Err(format!("span start {start} is past text length {len}"));
    }
    if end > len {
        log::warn!("privacy_filter: span end {end} past text length {len}, clamped");
        return Ok((start, len));
    }
    Ok((start, end))
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
    /// Synchronous — never call this directly; it's only reachable through
    /// the crate-level async `run_classify_blocking` wrapper below, which
    /// runs it inside tokio::task::spawn_blocking (items.id=479).
    /// Holds the PrivacyFilter Mutex for the duration of the C call.
    ///
    /// Pass threshold=0.0 to receive all spans regardless of confidence.
    /// Gate3 applies its own tier routing on top of the scores (D6-362).
    ///
    /// Returns Ok(vec) on success; vec may be empty if no spans found.
    /// Returns Err(reason) if the filter is unavailable or the call fails.
    pub(super) fn run_classify_blocking_sync(
        text: &str,
        threshold: f32,
    ) -> Result<Vec<PfEntityDecoded>, String> {
        let mutex = get_or_init().ok_or_else(|| "Privacy Filter unavailable".to_owned())?;

        let guard = mutex
            .lock()
            .map_err(|e| format!("Privacy Filter mutex poisoned: {e}"))?;

        // pf_classify does not require null termination (takes explicit len),
        // but CString is still used to satisfy *const c_char type. The byte
        // slice length is passed separately. NULs are swapped for spaces
        // first (same byte length, so span offsets into `text` stay valid);
        // CString::new cannot fail after that, but stays an Err, not an
        // unwrap, so it could only ever fail closed.
        let sanitized = super::sanitize_for_pf(text);
        let c_text = CString::new(sanitized.as_ref())
            .map_err(|e| format!("text contains interior null byte: {e}"))?;
        let text_len = sanitized.len();

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
        //
        // A span validate_span rejects makes the WHOLE call an Err (never a
        // skipped span: a flagged-and-dropped span is a missed finding). The
        // Result is held, not returned, until the C buffer is freed below.
        let decoded: Result<Vec<PfEntityDecoded>, String> = if out_ptr.is_null() {
            if n == 0 {
                Ok(Vec::new())
            } else {
                Err(format!("pf_classify returned a null array with n={n}"))
            }
        } else if n == 0 {
            Ok(Vec::new())
        } else {
            // Safety: out_ptr is a valid array of n PfEntity values allocated
            // by pf_classify. Read before pf_entities_free.
            let slice = unsafe { std::slice::from_raw_parts(out_ptr, n) };
            // span_text is cut from the ORIGINAL text, not the sanitised copy:
            // offsets are identical, and span_text is documented (and consumed
            // downstream) as the user's original text.
            let text_bytes = text.as_bytes();

            slice
                .iter()
                .map(|e| -> Result<PfEntityDecoded, String> {
                    let (start, end) = super::validate_span(e.start, e.end, text_bytes.len())?;

                    let label = unsafe {
                        if e.label.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(e.label).to_string_lossy().into_owned()
                        }
                    };

                    let span_text = String::from_utf8_lossy(&text_bytes[start..end]).into_owned();

                    Ok(PfEntityDecoded {
                        start_byte: start,
                        end_byte: end,
                        score: e.score,
                        label,
                        span_text,
                    })
                })
                .collect()
        };

        // Free the C-allocated array on every path, including a rejected
        // span. pf_entities_free requires both pointer and count (matches
        // pf.h signature).
        if !out_ptr.is_null() {
            unsafe {
                pf_entities_free(out_ptr, n);
            }
        }

        decoded
    }
} // end mod inner

// Re-export live implementations when compiled in.
#[cfg(privacy_filter_available)]
use inner::run_classify_blocking_sync;
#[cfg(privacy_filter_available)]
pub use inner::{is_available, set_backend_dir};

// ---------------------------------------------------------------------------
// Stub path — when PRIVACY_FILTER_LIB_DIR was not set at build time
// ---------------------------------------------------------------------------

/// Returns false when the Privacy Filter library was not compiled in.
#[cfg(not(privacy_filter_available))]
pub fn is_available() -> bool {
    false
}

/// Returns Err when the Privacy Filter library was not compiled in.
/// Gate3 calls this (via the async run_classify_blocking wrapper below) and
/// falls back to pre-filter sensitivity block.
#[cfg(not(privacy_filter_available))]
fn run_classify_blocking_sync(
    _text: &str,
    _threshold: f32,
) -> Result<Vec<PfEntityDecoded>, String> {
    Err("Privacy Filter not compiled in \
         (set PRIVACY_FILTER_LIB_DIR at build time)"
        .to_owned())
}

/// No-op when the Privacy Filter library was not compiled in — lets callers
/// (main.rs's setup hook) call this unconditionally without cfg-gating.
#[cfg(not(privacy_filter_available))]
pub fn set_backend_dir(_dir: std::path::PathBuf) {}

// ---------------------------------------------------------------------------
// Public entry point — always compiled, live or stub (items.id=479)
// ---------------------------------------------------------------------------

/// Run the Privacy Filter classifier on `text`, off the async executor.
///
/// spawn_blocking is baked in here rather than left as a doc-comment
/// convention a caller has to remember: previously `run_classify_blocking`
/// was itself the synchronous FFI call, and both existing call sites
/// (gate3.rs, output_scan.rs) happened to wrap it in
/// `tokio::task::spawn_blocking` correctly by hand — this was never
/// type-enforced, so a future caller could invoke it directly from an async
/// context and block the executor with no compiler warning. Now the
/// blocking call is unreachable except through this wrapper.
///
/// The `Result<Result<_, String>, JoinError>` nesting is deliberate, not
/// left over: it mirrors exactly what `spawn_blocking(...).await` produced
/// at the two call sites before this change, so their existing
/// panic-vs-classifier-error handling (a panic is treated as a harder
/// failure than an ordinary classifier error) needed no changes.
pub async fn run_classify_blocking(
    text: String,
    threshold: f32,
) -> Result<Result<Vec<PfEntityDecoded>, String>, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || run_classify_blocking_sync(&text, threshold)).await
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
    #[tokio::test]
    async fn stub_run_classify_returns_err() {
        let result = run_classify_blocking("Hello, my name is Alice.".to_owned(), 0.5).await;
        let classify_result = result.expect("spawn_blocking should not panic");
        assert!(classify_result.is_err());
        let msg = classify_result.unwrap_err();
        assert!(msg.contains("not compiled in"), "unexpected error: {msg}");
    }

    fn ent() -> PfEntityDecoded {
        PfEntityDecoded {
            start_byte: 0,
            end_byte: 1,
            score: 0.9,
            label: "private_person".to_owned(),
            span_text: "A".to_owned(),
        }
    }

    // -- sanitize_for_pf (items.id=613) --------------------------------------

    #[test]
    fn sanitize_for_pf_replaces_nul_and_preserves_length() {
        let text = "a\0b\0";
        let out = sanitize_for_pf(text);
        assert_eq!(out, "a b ");
        assert_eq!(out.len(), text.len());
    }

    #[test]
    fn sanitize_for_pf_keeps_char_boundaries_with_multibyte_text() {
        let text = "é\0日\0x";
        let out = sanitize_for_pf(text);
        assert_eq!(out.len(), text.len());
        for i in 0..=text.len() {
            assert_eq!(out.is_char_boundary(i), text.is_char_boundary(i), "at {i}");
        }
        assert!(!out.contains('\0'));
    }

    #[test]
    fn sanitize_for_pf_borrows_when_there_is_no_nul() {
        assert!(matches!(sanitize_for_pf("clean"), Cow::Borrowed("clean")));
    }

    // -- validate_span (items.id=613) ----------------------------------------

    #[test]
    fn validate_span_normal() {
        assert_eq!(validate_span(2, 5, 10), Ok((2, 5)));
        assert_eq!(validate_span(0, 10, 10), Ok((0, 10)));
    }

    #[test]
    fn validate_span_empty_and_at_end_are_valid() {
        assert_eq!(validate_span(4, 4, 10), Ok((4, 4)));
        assert_eq!(validate_span(10, 10, 10), Ok((10, 10)));
    }

    #[test]
    fn validate_span_negative_offsets_are_errors() {
        assert!(validate_span(-1, 5, 10).is_err());
        assert!(validate_span(2, -1, 10).is_err());
        assert!(validate_span(i32::MIN, i32::MIN, 10).is_err());
    }

    #[test]
    fn validate_span_end_before_start_is_an_error() {
        assert!(validate_span(5, 4, 10).is_err());
    }

    #[test]
    fn validate_span_start_past_text_is_an_error() {
        assert!(validate_span(11, 12, 10).is_err());
        assert!(validate_span(1, 5, 0).is_err() && validate_span(1, 1, 0).is_err());
    }

    #[test]
    fn validate_span_end_past_text_clamps_to_len() {
        assert_eq!(validate_span(7, 99, 10), Ok((7, 10)));
        assert_eq!(validate_span(10, 99, 10), Ok((10, 10)));
    }

    #[test]
    fn validate_span_error_text_carries_offsets_only() {
        let msg = validate_span(5, 4, 10).unwrap_err();
        assert!(msg.contains('5') && msg.contains('4'));
    }

    // -- classify_outcome (items.id=613) -------------------------------------

    #[test]
    fn classify_outcome_success_is_entities_not_failed() {
        assert!(matches!(
            classify_outcome(Ok(Ok(vec![]))),
            PfOutcome::Entities(v) if v.is_empty()
        ));
        assert!(matches!(
            classify_outcome(Ok(Ok(vec![ent()]))),
            PfOutcome::Entities(v) if v.len() == 1
        ));
    }

    #[test]
    fn classify_outcome_classifier_error_is_failed() {
        let out = classify_outcome(Ok(Err("Privacy Filter unavailable".to_owned())));
        assert!(
            matches!(out, PfOutcome::Failed { ref reason } if reason == "Privacy Filter unavailable")
        );
    }

    #[test]
    fn classify_outcome_poisoned_mutex_message_is_failed() {
        let out = classify_outcome(Ok(Err(
            "Privacy Filter mutex poisoned: poisoned lock: another task failed inside".to_owned(),
        )));
        assert!(matches!(out, PfOutcome::Failed { .. }));
    }

    #[test]
    fn classify_outcome_nonzero_rc_message_is_failed() {
        let out = classify_outcome(Ok(Err("pf_classify failed (rc=-1): bad model".to_owned())));
        assert!(matches!(out, PfOutcome::Failed { .. }));
    }

    #[tokio::test]
    async fn classify_outcome_join_error_from_a_panicking_task_is_failed() {
        let joined = tokio::task::spawn_blocking(|| -> Result<Vec<PfEntityDecoded>, String> {
            panic!("simulated Privacy Filter panic")
        })
        .await;
        assert!(joined.is_err(), "the task must really have panicked");
        match classify_outcome(joined) {
            PfOutcome::Failed { reason } => {
                // The panic message itself is not carried into the reason.
                assert!(!reason.contains("simulated"), "reason leaked: {reason}");
            }
            PfOutcome::Entities(_) => panic!("a panic must map to Failed"),
        }
    }

    #[tokio::test]
    async fn classify_outcome_cancelled_task_is_failed() {
        let handle =
            tokio::task::spawn(std::future::pending::<Result<Vec<PfEntityDecoded>, String>>());
        handle.abort();
        let joined = handle.await;
        assert!(joined.is_err());
        assert!(matches!(classify_outcome(joined), PfOutcome::Failed { .. }));
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
