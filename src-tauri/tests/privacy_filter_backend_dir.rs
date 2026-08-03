// src-tauri/tests/privacy_filter_backend_dir.rs
//
// Regression test for the ggml CPU-backend-dispatch loading bug: pf_classify
// used to fail with "CPU backend init failed" (rc=-1) because ggml's backend
// loader only ever checked (compile-time GGML_BACKEND_DIR, exe dir, cwd) for
// the libggml-cpu-*.so ISA variants -- none of which resolve for a Tauri app
// running from an arbitrary working directory. Fixed by pf_set_backend_dir
// (privacy-filter.cpp) + privacy_filter::set_backend_dir (this crate).
//
// Only compiled when the live FFI path is active (PRIVACY_FILTER_LIB_DIR set
// at build time) and a real GGUF model is present -- both are opt-in/dev-only
// per privacy_filter.rs's own doc comments, so this test skips (not fails)
// when either precondition isn't met, matching golden_vectors.rs's style of
// asserting only what the fixture/build actually provides.
#![cfg(privacy_filter_available)]

use quietrabbit_lib::conductor::privacy::privacy_filter;

#[test]
fn classify_succeeds_from_arbitrary_cwd() {
    // cargo test's cwd is always CARGO_MANIFEST_DIR (src-tauri/), never the
    // privacy-filter.cpp bin/ directory that ggml's default cwd-search
    // fallback would need -- this is the "arbitrary cwd" the bug depended on.
    assert_ne!(
        std::env::current_dir().unwrap().file_name().unwrap(),
        "bin",
        "test harness cwd unexpectedly IS the backend bin/ dir -- \
         this test would pass even with the bug present"
    );

    if !privacy_filter::is_available() {
        eprintln!(
            "privacy_filter_backend_dir: skipping -- no GGUF model at \
             QR_PRIVACY_FILTER_MODEL / ~/.local/share/quietrabbit/models/ \
             (see privacy_filter.rs model_path_cstring)"
        );
        return;
    }

    if let Err(e) = privacy_filter::run_classify_blocking("My name is Alice Smith.", 0.0) {
        panic!(
            "pf_classify failed: {e} -- if this says 'CPU backend init failed', \
             the libggml-cpu-*.so variants were not found under the resolved \
             backend dir (see build.rs PRIVACY_FILTER_BACKEND_DIR_DEV and \
             privacy_filter.rs BACKEND_DIR_DEV_FALLBACK)"
        );
    }
}
