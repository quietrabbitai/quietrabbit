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
// Model path: crate::privacy_model::loadable_model_path (items.id=678). In a
// release build that is <data_root>/models/privacy-filter-q8.gguf, offered only
// while its verified marker is valid, so a missing, partial or unverified file
// never reaches pf_load. Debug builds also honour QR_PRIVACY_FILTER_MODEL and,
// when no data root is set, the older ~/.local/share/quietrabbit/models/ path
// (see privacy_model/paths.rs).
//
// Loading: the first call loads the model; a failed pf_load is latched so it is
// not retried on every scan, and reinitialize() clears that latch (after a
// verified install) and tries again without a restart.
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
        Err(join_err) => PfOutcome::Failed {
            reason: join_error_reason(&join_err).to_owned(),
        },
    }
}

/// The one fixed text for a JoinError, for the Failed reason and for logs.
/// JoinError's Display can carry a panic message (which could carry text), so
/// nothing from it is ever interpolated; say only which kind.
pub(crate) fn join_error_reason(join_err: &tokio::task::JoinError) -> &'static str {
    if join_err.is_panic() {
        "Privacy Filter task panicked"
    } else {
        "Privacy Filter task was cancelled"
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
// Loading helpers (always compiled, pure -- items.id=678)
// ---------------------------------------------------------------------------

/// What to do with the result of `pf_load`. privacy-filter.cpp returns NULL
/// only for a NULL path; for a missing, unreadable or corrupt model it returns
/// a NON-NULL context whose `pf_last_error` is set and whose `pf_classify`
/// then fails (pf.cpp, pf_load). Treating only NULL as failure made a missing
/// model look available. Verified in the library source; the PF-enabled test
/// below confirms it against the linked library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) enum LoadDecision {
    /// A usable context.
    Use,
    /// NULL: nothing to free.
    Fail,
    /// A context carrying an error: free it, then fail.
    FailAndFree,
}

/// `last_error` is the text `pf_last_error` returned, None for NULL. An empty
/// text counts as no error.
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) fn load_decision(ctx_is_null: bool, last_error: Option<&str>) -> LoadDecision {
    if ctx_is_null {
        LoadDecision::Fail
    } else if last_error.is_some_and(|e| !e.is_empty()) {
        LoadDecision::FailAndFree
    } else {
        LoadDecision::Use
    }
}

/// How one attempt to fill a `ReloadableSlot` ended.
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) enum Attempt<T> {
    Loaded(T),
    /// Not loadable yet (no verified model): cheap to re-evaluate, so it is NOT
    /// latched. The next call tries again.
    NotYet,
    /// The load itself failed (ABI mismatch, pf_load error): latched until
    /// `clear_failure`, so a corrupt-but-right-size file is not re-read on
    /// every scan.
    Failed,
}

/// The filter instance. Once `loaded` it is never replaced or dropped, so a
/// context cannot be freed while a classify uses it; the only transition after
/// that is none -> loaded. The mutex serialises first loads (concurrent callers
/// wait for one load, as a OnceLock did) and holds the failure latch.
#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
pub(crate) struct ReloadableSlot<T> {
    loaded: std::sync::OnceLock<T>,
    failed: std::sync::Mutex<bool>,
}

#[cfg_attr(not(privacy_filter_available), allow(dead_code))]
impl<T> ReloadableSlot<T> {
    pub(crate) const fn new() -> Self {
        Self {
            loaded: std::sync::OnceLock::new(),
            failed: std::sync::Mutex::new(false),
        }
    }

    pub(crate) fn get_or_try(&self, load: impl FnOnce() -> Attempt<T>) -> Option<&T> {
        if let Some(v) = self.loaded.get() {
            return Some(v);
        }
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = self.loaded.get() {
            return Some(v); // loaded by another caller while this one waited
        }
        if *failed {
            return None;
        }
        match load() {
            Attempt::Loaded(v) => {
                let _ = self.loaded.set(v);
                self.loaded.get()
            }
            Attempt::NotYet => None,
            Attempt::Failed => {
                *failed = true;
                None
            }
        }
    }

    /// Forget a latched failure so the next `get_or_try` loads again.
    pub(crate) fn clear_failure(&self) {
        *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }
}

// ---------------------------------------------------------------------------
// Live path — compiled only when PRIVACY_FILTER_LIB_DIR was set at build time
// ---------------------------------------------------------------------------

#[cfg(privacy_filter_available)]
mod inner {
    use std::ffi::{CStr, CString};
    use std::marker::{PhantomData, PhantomPinned};
    use std::path::PathBuf;
    use std::sync::{Mutex, Once, OnceLock};

    use super::{load_decision, Attempt, LoadDecision, PfEntityDecoded, ReloadableSlot};

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

    // -- Global instance -----------------------------------------------------
    //
    // A ReloadableSlot (see above): loaded once, never replaced; a failed
    // pf_load is latched until reinitialize() clears it (items.id=678). Before
    // that item this was an OnceLock<Option<..>> that latched a failure for the
    // life of the process, so a model installed after startup stayed unused.
    static PF_INSTANCE: ReloadableSlot<Mutex<PrivacyFilter>> = ReloadableSlot::new();

    /// ggml's backend search directory is applied once, before the first
    /// pf_load, and never again (the loader runs once per process).
    static BACKEND_DIR_APPLIED: Once = Once::new();

    /// The model path as a C string, or None while no loadable model exists
    /// (privacy_model::loadable_model_path: a verified installed file in a
    /// release build). Non-UTF-8 paths are kept byte for byte on unix.
    fn model_path_cstring() -> Option<CString> {
        let path = crate::privacy_model::loadable_model_path()?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            CString::new(path.as_os_str().as_bytes()).ok()
        }
        #[cfg(not(unix))]
        {
            CString::new(path.to_str()?).ok()
        }
    }

    fn try_load() -> Attempt<Mutex<PrivacyFilter>> {
        // ABI check — fail before model load if library version changed.
        // Safety: pf_abi_version() has no preconditions.
        let abi = unsafe { pf_abi_version() } as u32;
        if abi != EXPECTED_PF_ABI_VERSION {
            log::error!(
                "privacy_filter: ABI mismatch — expected {EXPECTED_PF_ABI_VERSION}, \
                 got {abi}. Update EXPECTED_PF_ABI_VERSION in privacy_filter.rs \
                 and audit FFI declarations before using this library version."
            );
            return Attempt::Failed;
        }

        // Must happen before the first pf_load — see BACKEND_DIR doc comment.
        // BACKEND_DIR is populated by set_backend_dir() (called from
        // main.rs's setup() hook with Tauri's resolved resource dir)
        // if this process has a Tauri AppHandle; otherwise fall back
        // to the build-time-baked dev bin/ dir (calibration harness,
        // `cargo test`, etc).
        BACKEND_DIR_APPLIED.call_once(|| {
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
        });

        let Some(model_path) = model_path_cstring() else {
            log::info!("privacy_filter: no verified model to load yet");
            return Attempt::NotYet;
        };

        // Safety: model_path is a valid null-terminated C string.
        let ctx = unsafe {
            pf_load(
                model_path.as_ptr(),
                std::ptr::null(), // device: NULL → library default (cpu)
                0,                // n_threads: 0 → library default
            )
        };

        // pf_load returns a non-NULL context carrying an error for a missing or
        // corrupt model (see LoadDecision), so the error text decides.
        let last_error = if ctx.is_null() {
            None
        } else {
            // Safety: ctx is a live context from pf_load; the returned pointer
            // is valid until the next call on it and is copied here.
            let p = unsafe { pf_last_error(ctx as *const PfCtx) };
            if p.is_null() {
                None
            } else {
                Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            }
        };
        match load_decision(ctx.is_null(), last_error.as_deref()) {
            LoadDecision::Use => {
                log::info!("privacy_filter: initialised successfully");
                Attempt::Loaded(Mutex::new(PrivacyFilter { ctx }))
            }
            LoadDecision::Fail => {
                log::warn!("privacy_filter: pf_load returned null");
                Attempt::Failed
            }
            LoadDecision::FailAndFree => {
                log::warn!(
                    "privacy_filter: pf_load failed: {}",
                    last_error.as_deref().unwrap_or_default()
                );
                // Safety: ctx came from pf_load and has not been freed or shared.
                unsafe { pf_free(ctx) };
                Attempt::Failed
            }
        }
    }

    fn get_or_init() -> Option<&'static Mutex<PrivacyFilter>> {
        PF_INSTANCE.get_or_try(try_load)
    }

    // -- Public API ----------------------------------------------------------

    /// Returns true if the Privacy Filter library was compiled in and the
    /// GGUF model loaded successfully on first call.
    pub fn is_available() -> bool {
        get_or_init().is_some()
    }

    /// Clear a latched load failure and try to load again (items.id=678): called
    /// after a verified install, so the filter turns on without a restart.
    /// Blocking (it can load 1.6 GB): never call it from an async context
    /// directly. True when a model is loaded afterwards. A model that is already
    /// loaded is left as it is.
    pub fn reinitialize() -> bool {
        PF_INSTANCE.clear_failure();
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
pub use inner::{is_available, reinitialize, set_backend_dir};

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

/// False when the Privacy Filter library was not compiled in: there is nothing
/// to load.
#[cfg(not(privacy_filter_available))]
pub fn reinitialize() -> bool {
    false
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
// Status for the UI (always compiled, live or stub -- items.id=618)
// ---------------------------------------------------------------------------

/// Whether the outbound privacy check can run, as the frontend sees it.
/// `Downloading` and `Verifying` come from the model setup task
/// (items.id=678, crate::privacy_model). Retry is a UI affordance derived from
/// the variant (not retryable for `NotCompiledIn`), so the type carries no retry
/// field.
#[derive(Debug, Clone, PartialEq, serde::Serialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrivacyFilterStatus {
    Ready,
    /// The pinned total size is known from the start, so `percent` is None only
    /// if a caller has no figure.
    Downloading {
        percent: Option<u8>,
    },
    Verifying,
    Unavailable {
        reason: PrivacyFilterUnavailableReason,
    },
}

/// Why the check is unavailable. Additive: items.id=678 added the last three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyFilterUnavailableReason {
    /// The library was not linked at build time (PRIVACY_FILTER_LIB_DIR unset).
    NotCompiledIn,
    /// Compiled in, but the model is missing or failed to load.
    ModelUnavailable,
    /// No source delivered the model (offline, refused, a bad status).
    DownloadFailed,
    /// Not enough free disk space for the model.
    LowDisk,
    /// Bytes arrived that do not match the pinned size and hash.
    VerificationFailed,
}

impl From<crate::privacy_model::install::FailReason> for PrivacyFilterUnavailableReason {
    fn from(reason: crate::privacy_model::install::FailReason) -> Self {
        use crate::privacy_model::install::FailReason;
        match reason {
            FailReason::DownloadFailed => Self::DownloadFailed,
            FailReason::LowDisk => Self::LowDisk,
            FailReason::VerificationFailed => Self::VerificationFailed,
            FailReason::ModelUnavailable => Self::ModelUnavailable,
        }
    }
}

/// The status `get_health` and the retry command report. Loading the model
/// (is_available, first call) runs on the blocking pool, never on the async
/// executor.
pub async fn status() -> PrivacyFilterStatus {
    let install = crate::privacy_model::runtime::current_state();
    let available = tokio::task::spawn_blocking(is_available)
        .await
        .unwrap_or(false);
    status_from(cfg!(privacy_filter_available), available, install)
}

/// Whole percent of `current` in `total`, 0 to 100.
fn percent_of(current: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    u8::try_from((u128::from(current) * 100 / u128::from(total)).min(100)).unwrap_or(100)
}

/// Pure mapping from "compiled in", "loaded" and the setup task's state to a
/// status. A loaded filter is `Ready` whatever the task says.
fn status_from(
    compiled_in: bool,
    available: bool,
    install: crate::privacy_model::install::InstallState,
) -> PrivacyFilterStatus {
    use crate::privacy_model::install::InstallState;
    if available {
        return PrivacyFilterStatus::Ready;
    }
    match install {
        // A run is alive and nothing is loaded: the check is being set up.
        InstallState::Checking | InstallState::Verifying => PrivacyFilterStatus::Verifying,
        InstallState::Downloading { current } => PrivacyFilterStatus::Downloading {
            percent: Some(percent_of(current, crate::privacy_model::pin::SIZE)),
        },
        InstallState::Failed(reason) => PrivacyFilterStatus::Unavailable {
            reason: reason.into(),
        },
        InstallState::Idle | InstallState::Installed => PrivacyFilterStatus::Unavailable {
            reason: if compiled_in {
                PrivacyFilterUnavailableReason::ModelUnavailable
            } else {
                PrivacyFilterUnavailableReason::NotCompiledIn
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Fail closed when unavailable (always compiled, pure -- items.id=618)
// ---------------------------------------------------------------------------

/// What an outbound gate does, given whether the Privacy Filter can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnavailableAction {
    /// The filter is available: run it.
    UsePf,
    /// Debug and test builds: the pre-618 severity-only fallback.
    Legacy,
    /// Release builds: nothing external passes without the review.
    Block,
}

/// The one decision shared by gate3 and output_scan. `fail_closed` is a
/// parameter so both values are testable; production passes
/// `fail_closed_when_unavailable()`.
pub(crate) fn unavailable_action(available: bool, fail_closed: bool) -> UnavailableAction {
    match (available, fail_closed) {
        (true, _) => UnavailableAction::UsePf,
        (false, true) => UnavailableAction::Block,
        (false, false) => UnavailableAction::Legacy,
    }
}

/// True in release builds (debug_assertions off, the repo's debug-vs-release
/// idiom, as for the single-instance guard in main.rs). Debug and test builds
/// keep the legacy fallback so the golden vectors and unit tests, which run
/// without the filter, stay valid. A release build only exists with the filter
/// linked or the explicit build-time opt-out (build.rs), so this is the
/// runtime half of the same gate.
pub(crate) fn fail_closed_when_unavailable() -> bool {
    !cfg!(debug_assertions)
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

    // -- PrivacyFilterStatus (items.id=618) ----------------------------------

    #[test]
    fn status_from_maps_every_combination() {
        use crate::privacy_model::install::InstallState;
        use PrivacyFilterUnavailableReason::*;
        let idle = InstallState::Idle;
        assert_eq!(status_from(true, true, idle), PrivacyFilterStatus::Ready);
        assert_eq!(status_from(false, true, idle), PrivacyFilterStatus::Ready);
        assert_eq!(
            status_from(true, false, idle),
            PrivacyFilterStatus::Unavailable {
                reason: ModelUnavailable
            }
        );
        assert_eq!(
            status_from(false, false, idle),
            PrivacyFilterStatus::Unavailable {
                reason: NotCompiledIn
            }
        );
        assert_eq!(
            status_from(true, false, InstallState::Installed),
            PrivacyFilterStatus::Unavailable {
                reason: ModelUnavailable
            },
            "installed but not loaded is still unavailable"
        );
    }

    #[test]
    fn a_running_setup_task_is_reported_until_the_filter_is_loaded() {
        use crate::privacy_model::install::InstallState;
        use crate::privacy_model::pin::SIZE;
        for state in [InstallState::Checking, InstallState::Verifying] {
            assert_eq!(
                status_from(true, false, state),
                PrivacyFilterStatus::Verifying
            );
        }
        assert_eq!(
            status_from(true, false, InstallState::Downloading { current: 0 }),
            PrivacyFilterStatus::Downloading { percent: Some(0) }
        );
        assert_eq!(
            status_from(true, false, InstallState::Downloading { current: SIZE / 2 }),
            PrivacyFilterStatus::Downloading { percent: Some(50) }
        );
        assert_eq!(
            status_from(true, false, InstallState::Downloading { current: SIZE }),
            PrivacyFilterStatus::Downloading { percent: Some(100) }
        );
        // Loaded wins over whatever the task says.
        for state in [
            InstallState::Checking,
            InstallState::Verifying,
            InstallState::Downloading { current: 5 },
        ] {
            assert_eq!(status_from(true, true, state), PrivacyFilterStatus::Ready);
        }
    }

    #[test]
    fn a_failed_setup_task_reports_its_reason() {
        use crate::privacy_model::install::{FailReason, InstallState};
        use PrivacyFilterUnavailableReason as R;
        for (fail, reason) in [
            (FailReason::DownloadFailed, R::DownloadFailed),
            (FailReason::LowDisk, R::LowDisk),
            (FailReason::VerificationFailed, R::VerificationFailed),
            (FailReason::ModelUnavailable, R::ModelUnavailable),
        ] {
            assert_eq!(
                status_from(true, false, InstallState::Failed(fail)),
                PrivacyFilterStatus::Unavailable { reason }
            );
        }
    }

    #[test]
    fn percent_of_is_clamped_and_total_safe() {
        assert_eq!(percent_of(0, 0), 0);
        assert_eq!(percent_of(5, 0), 0);
        assert_eq!(percent_of(1, 3), 33);
        assert_eq!(percent_of(2, 3), 66);
        assert_eq!(percent_of(10, 10), 100);
        assert_eq!(percent_of(u64::MAX, 1), 100);
        assert_eq!(percent_of(u64::MAX, u64::MAX), 100);
    }

    #[test]
    fn the_new_reasons_serialize_to_the_bindings_contract() {
        let json = |r: PrivacyFilterUnavailableReason| {
            serde_json::to_string(&PrivacyFilterStatus::Unavailable { reason: r }).unwrap()
        };
        assert_eq!(
            json(PrivacyFilterUnavailableReason::DownloadFailed),
            r#"{"kind":"unavailable","reason":"download_failed"}"#
        );
        assert_eq!(
            json(PrivacyFilterUnavailableReason::LowDisk),
            r#"{"kind":"unavailable","reason":"low_disk"}"#
        );
        assert_eq!(
            json(PrivacyFilterUnavailableReason::VerificationFailed),
            r#"{"kind":"unavailable","reason":"verification_failed"}"#
        );
    }

    // -- load decision and the reloadable slot (items.id=678) -----------------

    #[test]
    fn load_decision_covers_null_error_and_clean_contexts() {
        // NULL: fail, nothing to free.
        assert_eq!(load_decision(true, None), LoadDecision::Fail);
        assert_eq!(load_decision(true, Some("ignored")), LoadDecision::Fail);
        // Non-NULL with an error set (what pf_load returns for a missing or
        // corrupt model): fail, and the context must be freed.
        assert_eq!(
            load_decision(false, Some("cannot open model")),
            LoadDecision::FailAndFree
        );
        // Non-NULL, no error: use it.
        assert_eq!(load_decision(false, None), LoadDecision::Use);
        // An empty error text is no error.
        assert_eq!(load_decision(false, Some("")), LoadDecision::Use);
    }

    #[test]
    fn a_slot_loads_once_and_never_replaces_what_it_holds() {
        let slot: ReloadableSlot<u32> = ReloadableSlot::new();
        assert_eq!(slot.get_or_try(|| Attempt::Loaded(7)), Some(&7));
        let mut called = false;
        assert_eq!(
            slot.get_or_try(|| {
                called = true;
                Attempt::Loaded(8)
            }),
            Some(&7)
        );
        assert!(!called, "a loaded slot does not load again");
        slot.clear_failure();
        assert_eq!(slot.get_or_try(|| Attempt::Loaded(9)), Some(&7));
    }

    #[test]
    fn a_failed_load_is_latched_until_it_is_cleared() {
        let slot: ReloadableSlot<u32> = ReloadableSlot::new();
        let loads = std::cell::Cell::new(0);
        let fail = || {
            loads.set(loads.get() + 1);
            Attempt::<u32>::Failed
        };
        assert_eq!(slot.get_or_try(fail), None);
        assert_eq!(slot.get_or_try(fail), None);
        assert_eq!(slot.get_or_try(fail), None);
        assert_eq!(loads.get(), 1, "no retry storm on a corrupt file");

        slot.clear_failure();
        assert_eq!(slot.get_or_try(|| Attempt::Loaded(3)), Some(&3));
    }

    #[test]
    fn not_yet_is_not_latched() {
        let slot: ReloadableSlot<u32> = ReloadableSlot::new();
        let tries = std::cell::Cell::new(0);
        for _ in 0..3 {
            assert_eq!(
                slot.get_or_try(|| {
                    tries.set(tries.get() + 1);
                    Attempt::NotYet
                }),
                None
            );
        }
        assert_eq!(tries.get(), 3, "each call looks again");
        // Once a model becomes loadable, the next call picks it up with no
        // reinitialize at all.
        assert_eq!(slot.get_or_try(|| Attempt::Loaded(1)), Some(&1));
    }

    #[test]
    fn concurrent_first_callers_load_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let slot: ReloadableSlot<usize> = ReloadableSlot::new();
        let loads = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let got = slot.get_or_try(|| {
                        loads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(30));
                        Attempt::Loaded(42)
                    });
                    assert_eq!(got, Some(&42));
                });
            }
        });
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn availability_flips_after_a_cleared_failure_and_never_flips_back() {
        // The same sequence is_available()/reinitialize() run, with a fake
        // loader: unavailable, latched, cleared, available, still available.
        let slot: ReloadableSlot<u32> = ReloadableSlot::new();
        let model_ready = std::cell::Cell::new(false);
        let load = || {
            if model_ready.get() {
                Attempt::Loaded(1)
            } else {
                Attempt::Failed
            }
        };
        assert!(slot.get_or_try(load).is_none());
        model_ready.set(true);
        assert!(
            slot.get_or_try(load).is_none(),
            "still latched: the verified install alone does not clear it"
        );
        slot.clear_failure(); // what reinitialize() does first
        assert!(slot.get_or_try(load).is_some());
        model_ready.set(false);
        assert!(slot.get_or_try(load).is_some(), "loaded stays loaded");
    }

    #[test]
    fn status_serializes_to_the_bindings_contract() {
        let json = |s: &PrivacyFilterStatus| serde_json::to_string(s).unwrap();
        assert_eq!(json(&PrivacyFilterStatus::Ready), r#"{"kind":"ready"}"#);
        assert_eq!(
            json(&PrivacyFilterStatus::Downloading { percent: Some(42) }),
            r#"{"kind":"downloading","percent":42}"#
        );
        assert_eq!(
            json(&PrivacyFilterStatus::Downloading { percent: None }),
            r#"{"kind":"downloading","percent":null}"#
        );
        assert_eq!(
            json(&PrivacyFilterStatus::Verifying),
            r#"{"kind":"verifying"}"#
        );
        assert_eq!(
            json(&PrivacyFilterStatus::Unavailable {
                reason: PrivacyFilterUnavailableReason::NotCompiledIn
            }),
            r#"{"kind":"unavailable","reason":"not_compiled_in"}"#
        );
        assert_eq!(
            json(&PrivacyFilterStatus::Unavailable {
                reason: PrivacyFilterUnavailableReason::ModelUnavailable
            }),
            r#"{"kind":"unavailable","reason":"model_unavailable"}"#
        );
    }

    #[cfg(not(privacy_filter_available))]
    #[tokio::test]
    async fn stub_status_is_not_compiled_in_and_agrees_with_is_available() {
        assert!(!is_available());
        assert_eq!(
            status().await,
            PrivacyFilterStatus::Unavailable {
                reason: PrivacyFilterUnavailableReason::NotCompiledIn
            }
        );
    }

    #[cfg(privacy_filter_available)]
    #[tokio::test]
    async fn live_status_is_ready_iff_is_available() {
        let expected = if is_available() {
            PrivacyFilterStatus::Ready
        } else {
            PrivacyFilterStatus::Unavailable {
                reason: PrivacyFilterUnavailableReason::ModelUnavailable,
            }
        };
        assert_eq!(status().await, expected);
    }

    #[test]
    fn unavailable_action_covers_every_combination() {
        use UnavailableAction::*;
        assert_eq!(unavailable_action(true, true), UsePf);
        assert_eq!(unavailable_action(true, false), UsePf);
        assert_eq!(unavailable_action(false, true), Block);
        assert_eq!(unavailable_action(false, false), Legacy);
    }

    #[test]
    fn fail_closed_follows_debug_assertions() {
        assert_eq!(fail_closed_when_unavailable(), !cfg!(debug_assertions));
    }

    // -- The real library and the real model (items.id=678) -------------------
    //
    // Compiled only into a PF-enabled build, so CI (which builds without the
    // library) never runs it. By hand:
    //   cd src-tauri && TMPDIR=$HOME/.cache/qr-678-tmp \
    //     CARGO_TARGET_DIR=$HOME/.cache/qr-618-target-pf \
    //     PRIVACY_FILTER_LIB_DIR=$HOME/privacy-filter.cpp/build/release-portable \
    //     cargo test --lib privacy_filter::tests::pf_enabled
    // The instance is process-wide, so every step is in ONE test, and the test
    // skips (loudly) when the model file is absent or something already loaded.

    #[cfg(privacy_filter_available)]
    struct EnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

    #[cfg(privacy_filter_available)]
    impl EnvRestore {
        fn new(names: &[&'static str]) -> Self {
            Self(names.iter().map(|n| (*n, std::env::var_os(n))).collect())
        }
    }

    #[cfg(privacy_filter_available)]
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.0.drain(..) {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[cfg(privacy_filter_available)]
    #[test]
    fn pf_enabled_unverified_and_corrupt_models_never_load_and_a_verified_one_does() {
        use crate::privacy_model::{install::pin_expect, marker, paths, pin};

        let home = std::env::var("HOME").unwrap_or_default();
        let real = std::path::PathBuf::from(&home)
            .join(".local/share/quietrabbit/models")
            .join(pin::FILE_NAME);
        if !real.exists() {
            eprintln!("SKIPPED: {real:?} is not there");
            return;
        }
        let _lock = crate::test_support::ENV_MUTEX.blocking_lock();
        let _restore = EnvRestore::new(&["QR_DATA_ROOT", paths::ENV_OVERRIDE]);
        let root = tempfile::tempdir().unwrap();
        std::env::set_var("QR_DATA_ROOT", root.path());
        std::env::remove_var(paths::ENV_OVERRIDE);
        if is_available() {
            eprintln!("SKIPPED: another test already loaded the model in this process");
            return;
        }

        // (a) No model under the data root: not available, and not latched.
        assert!(!is_available());

        // (b) The right file at the right path but no marker: it is never
        // handed to pf_load.
        let model = paths::model_path_in(root.path());
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &model).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&real, &model).unwrap();
        assert!(!is_available(), "an unverified file must not load");

        // (c) pf_load on a corrupt file returns a NON-NULL context carrying an
        // error. It must count as a failed load, not as available. (Debug
        // builds only: release builds never read the override.)
        if cfg!(debug_assertions) {
            let junk = root.path().join("junk.gguf");
            std::fs::write(&junk, b"this is not a gguf file").unwrap();
            std::env::set_var(paths::ENV_OVERRIDE, &junk);
            assert!(!reinitialize(), "a corrupt model must not look available");
            assert!(!is_available());
            std::env::remove_var(paths::ENV_OVERRIDE);
        }

        // (d) Verify the model (the marker), as the installer does. A failure
        // latched in (c) holds until reinitialize() clears it.
        marker::record(&model, &paths::marker_path_for(&model), &pin_expect()).unwrap();
        if cfg!(debug_assertions) {
            assert!(!is_available(), "the latch holds until reinitialize");
        }
        assert!(reinitialize(), "a verified model loads without a restart");
        assert!(is_available());

        // (e) And it classifies.
        let spans = run_classify_blocking_sync("My name is Alice Smith.", 0.0)
            .expect("classify after reinitialize");
        assert!(!spans.is_empty(), "expected at least one span");
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
    async fn join_error_reason_never_carries_the_panic_message() {
        const DISTINCTIVE: &str = "zq-distinctive-panic-text-7731";
        let joined = tokio::task::spawn_blocking(|| -> Result<Vec<PfEntityDecoded>, String> {
            panic!("{}", DISTINCTIVE)
        })
        .await;
        let join_err = joined.expect_err("the task must really have panicked");
        // The leak this helper exists to avoid is real: Display carries it.
        assert!(
            join_err.to_string().contains(DISTINCTIVE),
            "premise: JoinError Display should carry the panic message"
        );
        let reason = join_error_reason(&join_err);
        assert_eq!(reason, "Privacy Filter task panicked");
        assert!(!reason.contains(DISTINCTIVE));
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
