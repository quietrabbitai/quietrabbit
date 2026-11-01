// src-tauri/tests/build_gate.rs
//
// items.id=618: the release gate and rpath decisions build.rs makes, run as
// plain unit tests. build.rs itself has no test target, so the decisions live
// in build_gate.rs, which build.rs includes with `mod` and this file with
// `#[path]`. What build.rs does with the answers (panic, warning, link arg)
// is checked by the manual matrix in the item's handoff, not here.

#[path = "../build_gate.rs"]
mod build_gate;

use build_gate::{
    blocked_message, is_release_profile, opt_out_warning, release_gate, rpath_link_arg, Gate,
};

const LIB: Option<&str> = Some("/opt/privacy-filter.cpp/build/release-portable");

#[test]
fn lib_dir_set_is_live_in_every_profile() {
    for profile in [Some("release"), Some("debug"), None] {
        for opt_out in [None, Some("1"), Some("0")] {
            assert_eq!(release_gate(profile, LIB, opt_out), Gate::Live);
        }
    }
}

#[test]
fn debug_without_the_filter_only_warns() {
    for lib in [None, Some("")] {
        for opt_out in [None, Some("1")] {
            assert_eq!(
                release_gate(Some("debug"), lib, opt_out),
                Gate::DebugWithoutPrivacyFilter
            );
        }
    }
}

#[test]
fn unset_profile_is_not_a_release() {
    assert_eq!(
        release_gate(None, None, None),
        Gate::DebugWithoutPrivacyFilter
    );
    assert!(!is_release_profile(None));
    assert!(!is_release_profile(Some("debug")));
    assert!(is_release_profile(Some("release")));
}

#[test]
fn release_without_the_filter_is_blocked() {
    assert_eq!(
        release_gate(Some("release"), None, None),
        Gate::ReleaseBlocked
    );
}

#[test]
fn an_empty_lib_dir_counts_as_unset() {
    assert_eq!(
        release_gate(Some("release"), Some(""), None),
        Gate::ReleaseBlocked
    );
    assert_eq!(
        release_gate(Some("release"), Some(""), Some("1")),
        Gate::ReleaseOptOut
    );
}

#[test]
fn only_the_exact_string_1_opts_out() {
    assert_eq!(
        release_gate(Some("release"), None, Some("1")),
        Gate::ReleaseOptOut
    );
    for other in ["0", "true", "yes", "", " 1", "1 ", "11"] {
        assert_eq!(
            release_gate(Some("release"), None, Some(other)),
            Gate::ReleaseBlocked,
            "opt-out value {other:?} must not opt out"
        );
    }
}

#[test]
fn release_rpath_is_only_the_installed_relative_path() {
    let arg = rpath_link_arg(true, "Quiet Rabbit", "/home/dev/pf/ggml/src");
    assert_eq!(
        arg,
        "cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/Quiet Rabbit/ggml-backends"
    );
    assert!(!arg.contains("/home/dev"));
    // No second, colon-joined rpath entry after the "cargo:" prefix.
    assert!(!arg["cargo:".len()..].contains(':'), "{arg}");
}

#[test]
fn debug_rpath_keeps_the_absolute_build_path() {
    let arg = rpath_link_arg(false, "Quiet Rabbit", "/home/dev/pf/ggml/src");
    assert_eq!(
        arg,
        "cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/Quiet Rabbit/ggml-backends:/home/dev/pf/ggml/src"
    );
}

#[test]
fn the_messages_name_what_the_builder_needs() {
    let blocked = blocked_message();
    assert!(blocked.contains("PRIVACY_FILTER_LIB_DIR"));
    assert!(blocked.contains("QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER=1"));
    assert!(blocked.contains("cmake --preset release-portable"));
    assert!(opt_out_warning().starts_with("cargo:warning="));
    assert!(opt_out_warning().contains("NO Privacy Filter"));
}
