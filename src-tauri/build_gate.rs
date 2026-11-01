// src-tauri/build_gate.rs
//
// items.id=618: the build-script decisions about the Privacy Filter, kept as
// pure functions so tests/build_gate.rs can run them under plain `cargo test`
// (a build script has no test target of its own). build.rs pulls this file in
// with `mod build_gate;` and the integration test with `#[path]`; keep it free
// of anything that needs the build-script environment.

/// What build.rs does about the Privacy Filter for this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// PRIVACY_FILTER_LIB_DIR is set: link the filter.
    Live,
    /// No filter in a debug or test build: warn and carry on.
    DebugWithoutPrivacyFilter,
    /// No filter in a release build, with the explicit opt-out: warn loudly.
    ReleaseOptOut,
    /// No filter in a release build and no opt-out: the build must fail.
    ReleaseBlocked,
}

/// `profile` is cargo's PROFILE for the target ("release", or "debug" for
/// every profile that inherits dev). `lib_dir` is PRIVACY_FILTER_LIB_DIR, with
/// an empty value counted as unset. `opt_out` is
/// QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER; only the exact string "1" opts out.
pub fn release_gate(profile: Option<&str>, lib_dir: Option<&str>, opt_out: Option<&str>) -> Gate {
    if lib_dir.is_some_and(|d| !d.is_empty()) {
        return Gate::Live;
    }
    if !is_release_profile(profile) {
        return Gate::DebugWithoutPrivacyFilter;
    }
    if opt_out == Some("1") {
        Gate::ReleaseOptOut
    } else {
        Gate::ReleaseBlocked
    }
}

/// Only cargo's literal "release" counts; PROFILE unset (not run by cargo) is
/// not a release build.
pub fn is_release_profile(profile: Option<&str>) -> bool {
    profile == Some("release")
}

/// The linker rpath line for the ggml libs. A release binary gets only the
/// path relative to the installed executable; the absolute build-machine
/// `ggml_src_dir` is for debug and test binaries, which run from target/.
pub fn rpath_link_arg(release: bool, product_name: &str, ggml_src_dir: &str) -> String {
    let installed = format!("$ORIGIN/../lib/{product_name}/ggml-backends");
    if release {
        format!("cargo:rustc-link-arg=-Wl,-rpath,{installed}")
    } else {
        format!("cargo:rustc-link-arg=-Wl,-rpath,{installed}:{ggml_src_dir}")
    }
}

/// The panic text for Gate::ReleaseBlocked.
pub fn blocked_message() -> String {
    "refusing to build a release without the Privacy Filter (items.id=618).\n\
     \n\
     PRIVACY_FILTER_LIB_DIR is not set (or is empty), so the Privacy Filter would be\n\
     compiled out and the shipped app would run its Focus-run privacy review without it.\n\
     \n\
     To fix, build privacy-filter.cpp and point PRIVACY_FILTER_LIB_DIR at the build root:\n\
     \x20 git clone https://github.com/localai-org/privacy-filter.cpp\n\
     \x20 cd privacy-filter.cpp\n\
     \x20 cmake --preset release-portable\n\
     \x20 cmake --build --preset release-portable -j\n\
     \x20 export PRIVACY_FILTER_LIB_DIR=/path/to/privacy-filter.cpp/build/release-portable\n\
     (libpf.a, ggml/src/libggml-base.so.0, ggml/src/libggml.so.0 and bin/libggml-cpu-*.so\n\
     are expected under it; PRIVACY_FILTER_LIB_NAME overrides the library name.)\n\
     \n\
     For a local release build that deliberately has no Privacy Filter (never for a shipped\n\
     artifact), set QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER=1. The app then reports the\n\
     privacy check as unavailable."
        .to_owned()
}

/// The cargo:warning text for Gate::ReleaseOptOut.
pub fn opt_out_warning() -> &'static str {
    "cargo:warning=QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER=1: this RELEASE build has NO Privacy \
     Filter. Do not ship it. Set PRIVACY_FILTER_LIB_DIR for a shippable build."
}
