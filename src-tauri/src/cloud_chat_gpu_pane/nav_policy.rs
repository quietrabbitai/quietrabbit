// src-tauri/src/cloud_chat_gpu_pane/nav_policy.rs
//
// items.id=873, decisions.id=934: which top-level targets a Cloud Chat pane may
// load. The rule is by TARGET HOST, from hosts declared as data on the provider
// row (providers.navigate_hosts) plus the host of the row's own launch_url.
// No provider or host is named here (decisions.id=872, 875).
//
// This module is pure: no CEF types, no I/O. The CEF handlers (render.rs) hand
// it the URL CEF is about to load and act on the decision.
//
// Treatment of each case (pinned by the tests below):
//   case          hosts are compared lowercase.
//   trailing dot  one trailing dot is stripped on both sides ("a.test." is "a.test").
//   subdomain     exact host only, no wildcard: "x.a.test" is not "a.test", and
//                 "a.test.evil.test" and "evila.test" are not "a.test" either.
//   userinfo      any URL with a username or password is BlockMalformed, even when
//                 the host matches (url_policy.rs precedent): never loaded, never
//                 offered to the system browser.
//   IP literals   never match a declared host (a declared IP literal is itself
//                 invalid). The url crate canonicalises 0x7f.1 and decimal forms to
//                 dotted IPv4, so those are IP literals too.
//   punycode      targets and declared entries are compared as IDNA ASCII. A
//                 unicode lookalike becomes xn--..., does not match, and the confirm
//                 shows the ASCII form.
//   port          only the scheme's default port (the url crate drops an explicit
//                 :443). The launch URL's own port is also allowed on the launch
//                 host, so a launch URL on a non-default port can open itself.
//   scheme        an in-pane match needs https. http to a declared host takes the
//                 confirm path. Other schemes are BlockScheme and never passed on.
//                 about:blank and about:srcdoc are inert and allowed. An empty
//                 target is allowed (a blank window.open, whose later navigation is
//                 checked by the popup's own handler).
//   length        a URL over MAX_URL_BYTES is BlockMalformed.
//
// FAIL CLOSED. A launch_url that is not an https DNS host refuses the pane
// (PolicyError). A navigate_hosts value that is not a JSON array declares
// nothing. A non-string element, or an entry that is not a plain host name (empty,
// a wildcard, a scheme, a path, a port, userinfo, an IP literal, an empty label),
// declares nothing; the other entries and the launch host still count.

use std::collections::BTreeSet;

use reqwest::Url;
use serde_json::Value;

/// Longest URL the policy looks at, in bytes. Longer targets are refused rather
/// than held for a confirm.
pub const MAX_URL_BYTES: usize = 2048;

/// Longest declared host, in bytes (the DNS limit).
const MAX_HOST_BYTES: usize = 253;

/// Longest scheme text carried to a notice.
const MAX_SCHEME_CHARS: usize = 16;

/// Why a pane cannot open for a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyError {
    /// The row's launch_url is not an https URL with a DNS host name.
    LaunchUrlUnusable,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::LaunchUrlUnusable => {
                write!(f, "the provider's launch address is not usable")
            }
        }
    }
}

/// The hosts one pane may load, built once when the pane opens (the CEF handlers
/// never read the database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSet {
    hosts: BTreeSet<String>,
    launch_host: String,
    launch_port: Option<u16>,
}

/// What to do with one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavDecision {
    /// Behaves as today: loads in the pane (or the existing closable popup).
    Allow,
    /// Never loads in the pane. Ask the user: `host` is the canonical ASCII host
    /// to show, `url` the canonical URL held for the system browser.
    Confirm { host: String, url: String },
    /// A scheme that is never loaded and never handed on. Notice only.
    BlockScheme { scheme: String },
    /// Unparsable, userinfo, over-long or hostless. Notice only.
    BlockMalformed,
}

/// One trailing dot stripped; None for an empty host or one with an empty label.
fn canonical_domain(domain: &str) -> Option<String> {
    let d = domain.strip_suffix('.').unwrap_or(domain);
    if d.is_empty() || d.len() > MAX_HOST_BYTES || d.split('.').any(str::is_empty) {
        return None;
    }
    Some(d.to_owned())
}

/// A declared host entry in canonical form, or None when it is not a plain host
/// name. Used for the declared list only.
pub fn normalize_host(raw: &str) -> Option<String> {
    if raw.is_empty()
        || !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || !c.is_ascii())
    {
        return None;
    }
    // Anything the parser would read as more than a host (a port, a path,
    // userinfo, percent-escapes, whitespace, brackets, a wildcard) was refused
    // above, because none of those characters is allowed.
    let url = Url::parse(&format!("https://{raw}/")).ok()?;
    if url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    // `domain()` is None for an IP literal.
    canonical_domain(url.domain()?)
}

impl HostSet {
    /// Builds the set for one row. `provider_id` is only for the log.
    pub fn from_row(
        provider_id: &str,
        launch_url: &str,
        navigate_hosts_json: &str,
    ) -> Result<HostSet, PolicyError> {
        let launch = Url::parse(launch_url)
            .ok()
            .filter(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none())
            .ok_or(PolicyError::LaunchUrlUnusable)?;
        let launch_host = launch
            .domain()
            .and_then(canonical_domain)
            .ok_or(PolicyError::LaunchUrlUnusable)?;

        let mut hosts = BTreeSet::new();
        hosts.insert(launch_host.clone());

        match serde_json::from_str::<Value>(navigate_hosts_json) {
            Ok(Value::Array(entries)) => {
                for entry in entries {
                    match entry.as_str().and_then(normalize_host) {
                        Some(h) => {
                            hosts.insert(h);
                        }
                        None => log::warn!(
                            "cloud_chat_gpu_pane::nav_policy: provider '{provider_id}' has a \
                             navigate_hosts entry that is not a plain host name, ignored"
                        ),
                    }
                }
            }
            _ => log::warn!(
                "cloud_chat_gpu_pane::nav_policy: provider '{provider_id}' navigate_hosts is \
                 not a JSON array, declaring nothing (launch host only)"
            ),
        }

        Ok(HostSet {
            hosts,
            launch_host,
            launch_port: launch.port(),
        })
    }

    /// Whether `host` (canonical) on `port` (None = the scheme default) is allowed.
    fn allows(&self, host: &str, port: Option<u16>) -> bool {
        if !self.hosts.contains(host) {
            return false;
        }
        match port {
            None => true,
            Some(p) => host == self.launch_host && self.launch_port == Some(p),
        }
    }

    /// The canonical hosts in the set, for logs and tests.
    pub fn hosts(&self) -> impl Iterator<Item = &str> {
        self.hosts.iter().map(String::as_str)
    }
}

fn sanitize_scheme(scheme: &str) -> String {
    scheme
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .take(MAX_SCHEME_CHARS)
        .collect()
}

/// Decides one target. `raw` is the URL CEF is about to load.
pub fn classify(set: &HostSet, raw: &str) -> NavDecision {
    if raw.is_empty() {
        return NavDecision::Allow;
    }
    if raw.len() > MAX_URL_BYTES {
        return NavDecision::BlockMalformed;
    }
    let Ok(url) = Url::parse(raw) else {
        return NavDecision::BlockMalformed;
    };

    match url.scheme() {
        "http" | "https" => {}
        "about" if matches!(url.as_str(), "about:blank" | "about:srcdoc") => {
            return NavDecision::Allow
        }
        other => {
            return NavDecision::BlockScheme {
                scheme: sanitize_scheme(other),
            }
        }
    }

    if !url.username().is_empty() || url.password().is_some() {
        return NavDecision::BlockMalformed;
    }

    let (host_display, match_host) = match url.domain() {
        Some(d) => match canonical_domain(d) {
            Some(c) => (c.clone(), Some(c)),
            None => return NavDecision::BlockMalformed,
        },
        // An IP literal is shown canonically and never matches.
        None => match url.host_str() {
            Some(h) => (h.to_owned(), None),
            None => return NavDecision::BlockMalformed,
        },
    };

    if url.scheme() == "https" {
        if let Some(h) = match_host {
            if set.allows(&h, url.port()) {
                return NavDecision::Allow;
            }
        }
    }

    NavDecision::Confirm {
        host: host_display,
        url: url.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCH: &str = "https://launch.example.test";

    fn set(navigate: &str) -> HostSet {
        HostSet::from_row("fixture", LAUNCH, navigate).unwrap()
    }

    fn allowed(set: &HostSet, url: &str) -> bool {
        classify(set, url) == NavDecision::Allow
    }

    fn confirm_host(set: &HostSet, url: &str) -> Option<String> {
        match classify(set, url) {
            NavDecision::Confirm { host, .. } => Some(host),
            _ => None,
        }
    }

    #[test]
    fn launch_host_is_always_allowed_and_declared_hosts_add_to_it() {
        let s = set("[]");
        assert!(allowed(&s, "https://launch.example.test/"));
        assert!(allowed(&s, "https://launch.example.test/a/b?c=d#e"));
        assert_eq!(
            confirm_host(&s, "https://other.example.test/").unwrap(),
            "other.example.test"
        );

        let s = set(r#"["signin.example.test", "idp.example.test"]"#);
        assert!(allowed(&s, "https://signin.example.test/x"));
        assert!(allowed(&s, "https://idp.example.test/"));
        assert!(allowed(&s, "https://launch.example.test/"));
        assert!(!allowed(&s, "https://other.example.test/"));
    }

    #[test]
    fn matching_is_exact_with_no_subdomain_or_suffix_matching() {
        let s = set(r#"["a.example.test"]"#);
        assert!(allowed(&s, "https://a.example.test/"));
        for lookalike in [
            "https://x.a.example.test/",
            "https://example.test/",
            "https://evila.example.test/",
            "https://a.example.test.evil.test/",
            "https://a-example.test/",
            "https://aa.example.test/",
        ] {
            assert!(!allowed(&s, lookalike), "{lookalike} must not match");
            assert!(confirm_host(&s, lookalike).is_some(), "{lookalike}");
        }
    }

    #[test]
    fn case_and_one_trailing_dot_are_normalised_but_not_two() {
        let s = set(r#"["A.Example.TEST"]"#);
        assert!(allowed(&s, "HTTPS://A.EXAMPLE.TEST/"));
        assert!(allowed(&s, "https://a.example.test./"));
        assert!(!allowed(&s, "https://a.example.test../"));
        // A declared entry with a trailing dot is the same host.
        let s = set(r#"["a.example.test."]"#);
        assert!(allowed(&s, "https://a.example.test/"));
    }

    #[test]
    fn userinfo_is_malformed_even_on_a_declared_host() {
        let s = set(r#"["a.example.test"]"#);
        for url in [
            "https://user@a.example.test/",
            "https://user:pw@a.example.test/",
            "https://a.example.test@evil.test/",
            "https://a.example.test:pw@evil.test/",
            "http://user@evil.test/",
        ] {
            assert_eq!(classify(&s, url), NavDecision::BlockMalformed, "{url}");
        }
        // The WHATWG parser reads a backslash as a slash, so the host here is
        // the declared one, which is what the browser will load.
        assert!(allowed(&s, r"https://a.example.test\@evil.test/"));
    }

    #[test]
    fn ip_literals_never_match_and_are_shown_canonically() {
        let s = set(r#"["a.example.test"]"#);
        assert_eq!(confirm_host(&s, "https://127.0.0.1/").unwrap(), "127.0.0.1");
        assert_eq!(confirm_host(&s, "https://0x7f.1/").unwrap(), "127.0.0.1");
        assert_eq!(
            confirm_host(&s, "https://2130706433/").unwrap(),
            "127.0.0.1"
        );
        assert_eq!(confirm_host(&s, "https://[::1]/").unwrap(), "[::1]");
        assert_eq!(
            confirm_host(&s, "https://[::ffff:127.0.0.1]/").unwrap(),
            "[::ffff:7f00:1]"
        );
        assert!(confirm_host(&s, "http://localhost/").is_some());
    }

    #[test]
    fn punycode_and_unicode_lookalikes() {
        // A declared unicode entry is stored as its IDNA ASCII form.
        let s = set(r#"["bücher.example.test"]"#);
        assert!(allowed(&s, "https://xn--bcher-kva.example.test/"));
        assert!(allowed(&s, "https://bücher.example.test/"));
        // A Cyrillic lookalike of a declared Latin host does not match, and the
        // confirm names the ASCII (punycode) form.
        let s = set(r#"["apple.example.test"]"#);
        let host = confirm_host(&s, "https://аpple.example.test/").unwrap();
        assert!(host.starts_with("xn--"), "{host}");
        assert!(!allowed(&s, "https://аpple.example.test/"));
    }

    #[test]
    fn ports_default_only_except_the_launch_port_on_the_launch_host() {
        let s = set(r#"["a.example.test"]"#);
        assert!(allowed(&s, "https://a.example.test:443/"));
        assert!(!allowed(&s, "https://a.example.test:8443/"));
        assert!(confirm_host(&s, "https://a.example.test:8443/").is_some());

        let s = HostSet::from_row(
            "fixture",
            "https://launch.example.test:8443/",
            r#"["a.example.test"]"#,
        )
        .unwrap();
        assert!(allowed(&s, "https://launch.example.test:8443/x"));
        // The launch host is a declared host: its default port is allowed too.
        assert!(allowed(&s, "https://launch.example.test/x"));
        assert!(!allowed(&s, "https://launch.example.test:9443/x"));
        assert!(!allowed(&s, "https://a.example.test:8443/x"));
    }

    #[test]
    fn in_pane_match_needs_https() {
        let s = set(r#"["a.example.test"]"#);
        assert!(!allowed(&s, "http://a.example.test/"));
        assert_eq!(
            confirm_host(&s, "http://a.example.test/").unwrap(),
            "a.example.test"
        );
        assert!(!allowed(&s, "http://launch.example.test/"));
    }

    #[test]
    fn other_schemes_are_blocked_with_a_notice_and_never_confirmed() {
        let s = set("[]");
        for url in [
            "blob:https://launch.example.test/00000000-0000-0000-0000-000000000000",
            "data:text/html,hello",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "chrome://settings",
            "view-source:https://launch.example.test/",
            "mailto:a@example.test",
            "tel:+10000000",
            "ftp://launch.example.test/",
            "ws://launch.example.test/",
            "about:config",
        ] {
            assert!(
                matches!(classify(&s, url), NavDecision::BlockScheme { .. }),
                "{url}"
            );
        }
        match classify(&s, "data:text/html,hello") {
            NavDecision::BlockScheme { scheme } => assert_eq!(scheme, "data"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn inert_and_empty_targets_are_allowed() {
        let s = set("[]");
        assert!(allowed(&s, ""));
        assert!(allowed(&s, "about:blank"));
        assert!(allowed(&s, "about:srcdoc"));
        assert!(!allowed(&s, "about:blank#frag"));
    }

    #[test]
    fn malformed_and_over_long_targets_are_refused() {
        let s = set("[]");
        assert_eq!(classify(&s, "not a url"), NavDecision::BlockMalformed);
        assert_eq!(classify(&s, "https://"), NavDecision::BlockMalformed);
        assert_eq!(classify(&s, "/relative/path"), NavDecision::BlockMalformed);
        let long = format!("https://launch.example.test/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(classify(&s, &long), NavDecision::BlockMalformed);
        let ok = format!(
            "https://launch.example.test/{}",
            "a".repeat(MAX_URL_BYTES - 40)
        );
        assert!(allowed(&s, &ok));
    }

    #[test]
    fn confirm_carries_the_canonical_url() {
        let s = set("[]");
        match classify(&s, "HTTPS://Other.Example.TEST:443/p?q=1#f") {
            NavDecision::Confirm { host, url } => {
                assert_eq!(host, "other.example.test");
                assert_eq!(url, "https://other.example.test/p?q=1#f");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unusable_launch_url_refuses_the_pane() {
        for launch in [
            "",
            "not a url",
            "http://launch.example.test/",
            "https://user@launch.example.test/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "ftp://launch.example.test/",
        ] {
            assert_eq!(
                HostSet::from_row("fixture", launch, "[]"),
                Err(PolicyError::LaunchUrlUnusable),
                "{launch:?}"
            );
        }
    }

    #[test]
    fn a_bad_declared_value_declares_nothing_and_fails_closed() {
        let launch_only = |json: &str| {
            let s = set(json);
            assert_eq!(
                s.hosts().collect::<Vec<_>>(),
                vec!["launch.example.test"],
                "{json}"
            );
        };
        // Not an array, or not JSON at all.
        for json in [
            "{}",
            "\"a.example.test\"",
            "42",
            "null",
            "true",
            "",
            "not json",
            "[oops",
        ] {
            launch_only(json);
        }
        // Entries that are not plain host names declare nothing.
        for entry in [
            r#"[1]"#,
            r#"[null]"#,
            r#"[["a.example.test"]]"#,
            r#"[{"host":"a.example.test"}]"#,
            r#"[""]"#,
            r#"[" "]"#,
            r#"[" a.example.test"]"#,
            r#"["a.example.test "]"#,
            r#"["*.example.test"]"#,
            r#"["*"]"#,
            r#"["a*.example.test"]"#,
            r#"["https://a.example.test"]"#,
            r#"["a.example.test/path"]"#,
            r#"["a.example.test:443"]"#,
            r#"["user@a.example.test"]"#,
            r#"["a.example.test?x"]"#,
            r#"["a.example.test#x"]"#,
            r#"["a..example.test"]"#,
            r#"["."]"#,
            r#"[".a.example.test"]"#,
            r#"["a.example.test.."]"#,
            r#"["%61.example.test"]"#,
            r#"["127.0.0.1"]"#,
            r#"["0x7f.1"]"#,
            r#"["[::1]"]"#,
            r#"["::1"]"#,
        ] {
            launch_only(entry);
        }
        // A bad entry does not take its good neighbours with it.
        let s = set(r#"["a.example.test", 7, "*.b.example.test", "c.example.test"]"#);
        assert_eq!(
            s.hosts().collect::<Vec<_>>(),
            vec!["a.example.test", "c.example.test", "launch.example.test"]
        );
    }

    #[test]
    fn declared_entries_are_deduplicated_with_the_launch_host() {
        let s = set(r#"["launch.example.test", "A.example.test", "a.example.test"]"#);
        assert_eq!(
            s.hosts().collect::<Vec<_>>(),
            vec!["a.example.test", "launch.example.test"]
        );
    }

    #[test]
    fn normalize_host_cases() {
        assert_eq!(
            normalize_host("Example.TEST").as_deref(),
            Some("example.test")
        );
        assert_eq!(
            normalize_host("example.test.").as_deref(),
            Some("example.test")
        );
        assert_eq!(normalize_host("localhost").as_deref(), Some("localhost"));
        assert_eq!(
            normalize_host("xn--bcher-kva.test").as_deref(),
            Some("xn--bcher-kva.test")
        );
        assert_eq!(
            normalize_host("bücher.test").as_deref(),
            Some("xn--bcher-kva.test")
        );
        assert_eq!(normalize_host("*.example.test"), None);
        assert_eq!(normalize_host("1.2.3.4"), None);
        assert_eq!(normalize_host(""), None);
        let long = format!("{}.test", "a".repeat(260));
        assert_eq!(normalize_host(&long), None);
    }

    /// No provider or host is named in this module or in the handlers that use it
    /// (decisions.id=872, 875). Test hosts use the reserved example.test domain.
    #[test]
    fn this_module_names_no_provider_or_real_host() {
        let src = include_str!("nav_policy.rs");
        let code = src
            .split("#[cfg(test)]")
            .next()
            .expect("module code before its tests");
        for name in [
            "duck",
            "claude",
            "chatgpt",
            "openai",
            "gemini",
            "google",
            "anthropic",
            "mistral",
            "groq",
        ] {
            assert!(
                !code.to_ascii_lowercase().contains(name),
                "nav_policy.rs must not name '{name}'"
            );
        }
    }
}
