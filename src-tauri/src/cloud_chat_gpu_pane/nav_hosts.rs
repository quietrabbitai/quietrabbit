// src-tauri/src/cloud_chat_gpu_pane/nav_hosts.rs
//
// items.id=873, decisions.id=934 addendum 2: where a Cloud Chat pane may navigate.
//
// A providers row may declare LOGIN HOSTS (providers.login_hosts, shared_037.sql).
// For a row that declares them, a main-frame navigation of the pane to a host
// outside the list is cancelled and the row's launch_url loads instead: no
// modal, no dead end. A row that declares none is unrestricted (its login set is
// open: Google, Apple, phone, enterprise SSO). The launch host is always in the
// list of a restricted row. No provider or host is named in code (decisions.id=872,
// 875): everything here comes from the row.
//
// This module is pure: no CEF types, no I/O. The CEF handlers (render.rs) hand it
// the URL CEF is about to load and act on the decision.
//
// MATCHING is exact host only, no wildcard syntax. Hosts are compared lowercase,
// as IDNA ASCII (the url crate does that), with one trailing dot stripped. A
// subdomain, a suffix or a look-alike ("a.test.evil.test", "evila.test") never
// matches "a.test". Country domains are not matched by pattern: the row lists each
// one as an exact host, so a user sent to a country domain the row does not list
// bounces home (the cost decisions.id=934 addendum 2 accepts).
//
//   scheme   http and https are judged by host. Any other scheme (mailto:, blob:,
//            data:, about:) has no host to judge and is allowed here: it is not a
//            navigation to a web host, and the pane's other handlers deal with it.
//   port     only the scheme's default port (the url crate drops an explicit :443),
//            plus the launch URL's own port on the launch host.
//   userinfo, unparsable, hostless or IP-literal targets are outside the list: they
//            bounce, each with its own reason code in the log.
//   length   there is no length rule. A long URL to a listed host is allowed.
//
// DELIBERATE FAIL-OPEN, three places, each so an unlisted login host or country
// domain cannot lock a user out:
//   1. A row whose login_hosts is not a JSON array, or holds no valid entry, is
//      unrestricted, not "launch host only".
//   2. A restricted row whose launch_url is not an https DNS host is unrestricted
//      (there is nothing to bounce to).
//   3. BounceGuard: a second bounce for the same pane inside BOUNCE_WINDOW is
//      allowed and logged. The case it opens: a user can land ONCE on an off-list
//      page (the first bounce takes them home, the next navigation inside the
//      window is let through).

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Url;
use serde_json::Value;

/// Longest declared host, in bytes (the DNS limit).
const MAX_HOST_BYTES: usize = 253;

/// A second bounce for one pane inside this window is allowed (fail-open).
pub const BOUNCE_WINDOW: Duration = Duration::from_secs(5);

/// One trailing dot stripped; None for an empty host or one with an empty label.
fn canonical_domain(domain: &str) -> Option<String> {
    let d = domain.strip_suffix('.').unwrap_or(domain);
    if d.is_empty() || d.len() > MAX_HOST_BYTES || d.split('.').any(str::is_empty) {
        return None;
    }
    Some(d.to_owned())
}

/// A declared host entry in canonical form, or None when it is not a plain host
/// name (empty, a wildcard, a scheme, a path, a port, userinfo, an IP literal, an
/// empty label, whitespace, brackets, percent-escapes).
pub fn normalize_host(raw: &str) -> Option<String> {
    if raw.is_empty()
        || !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || !c.is_ascii())
    {
        return None;
    }
    let url = Url::parse(&format!("https://{raw}/")).ok()?;
    if url.port().is_some() || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    // `domain()` is None for an IP literal.
    canonical_domain(url.domain()?)
}

/// The hosts one pane may navigate to, built once when the pane opens (the CEF
/// handlers never read the database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSet {
    restricted: bool,
    hosts: BTreeSet<String>,
    launch_host: String,
    launch_port: Option<u16>,
    launch_url: String,
}

impl HostSet {
    /// An unrestricted set: nothing is ever bounced.
    pub fn unrestricted() -> HostSet {
        HostSet {
            restricted: false,
            hosts: BTreeSet::new(),
            launch_host: String::new(),
            launch_port: None,
            launch_url: String::new(),
        }
    }

    /// Builds the set for one row. Never fails: every problem fails OPEN to an
    /// unrestricted set (see the module header). `provider_id` is only for the log.
    pub fn from_row(provider_id: &str, launch_url: &str, login_hosts_json: &str) -> HostSet {
        let mut declared = BTreeSet::new();
        match serde_json::from_str::<Value>(login_hosts_json) {
            Ok(Value::Array(entries)) => {
                for entry in entries {
                    match entry.as_str().and_then(normalize_host) {
                        Some(h) => {
                            declared.insert(h);
                        }
                        None => log::warn!(
                            "cloud_chat_gpu_pane::nav_hosts: provider '{provider_id}' has a \
                             login_hosts entry that is not a plain host name, ignored"
                        ),
                    }
                }
            }
            _ => log::warn!(
                "cloud_chat_gpu_pane::nav_hosts: provider '{provider_id}' login_hosts is not a \
                 JSON array, the pane is unrestricted"
            ),
        }
        if declared.is_empty() {
            return HostSet::unrestricted();
        }

        let launch = Url::parse(launch_url)
            .ok()
            .filter(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none());
        let Some(launch) = launch else {
            log::warn!(
                "cloud_chat_gpu_pane::nav_hosts: provider '{provider_id}' declares login hosts \
                 but its launch_url is not an https URL, the pane is unrestricted"
            );
            return HostSet::unrestricted();
        };
        let Some(launch_host) = launch.domain().and_then(canonical_domain) else {
            log::warn!(
                "cloud_chat_gpu_pane::nav_hosts: provider '{provider_id}' declares login hosts \
                 but its launch_url has no DNS host, the pane is unrestricted"
            );
            return HostSet::unrestricted();
        };

        declared.insert(launch_host.clone());
        HostSet {
            restricted: true,
            hosts: declared,
            launch_host,
            launch_port: launch.port(),
            launch_url: launch.as_str().to_owned(),
        }
    }

    /// Whether this row declares login hosts (a pane for an unrestricted row has
    /// no navigation handler work to do).
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    /// The URL a bounced navigation loads instead. Empty for an unrestricted set.
    pub fn launch_url(&self) -> &str {
        &self.launch_url
    }

    /// The canonical hosts in the set, for logs and tests.
    pub fn hosts(&self) -> impl Iterator<Item = &str> {
        self.hosts.iter().map(String::as_str)
    }

    /// Whether `host` (canonical) on `port` (None = the scheme default) is listed.
    fn allows(&self, host: &str, port: Option<u16>) -> bool {
        if !self.hosts.contains(host) {
            return false;
        }
        match port {
            None => true,
            Some(p) => host == self.launch_host && self.launch_port == Some(p),
        }
    }
}

/// Why a target is outside the list. The log carries this and the host only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BounceReason {
    /// A well-formed http(s) host that the row does not list.
    OutsideList,
    /// The URL did not parse.
    Unparsable,
    /// The URL carries a username or password.
    Userinfo,
    /// The host is an IP literal or not a plain DNS name.
    HostForm,
}

impl BounceReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BounceReason::OutsideList => "outside_list",
            BounceReason::Unparsable => "unparsable",
            BounceReason::Userinfo => "userinfo",
            BounceReason::HostForm => "host_form",
        }
    }
}

/// What to do with one main-frame target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavDecision {
    /// Loads as today.
    Allow,
    /// Cancel and load the row's launch_url. `host` is the canonical host for the
    /// log ("-" when there is none).
    Bounce { reason: BounceReason, host: String },
}

/// Decides one main-frame target. `raw` is the URL CEF is about to load.
pub fn classify(set: &HostSet, raw: &str) -> NavDecision {
    if !set.restricted || raw.is_empty() {
        return NavDecision::Allow;
    }
    let Ok(url) = Url::parse(raw) else {
        return NavDecision::Bounce {
            reason: BounceReason::Unparsable,
            host: "-".to_owned(),
        };
    };
    // No host to judge: not a navigation to a web host.
    if !matches!(url.scheme(), "http" | "https") {
        return NavDecision::Allow;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return NavDecision::Bounce {
            reason: BounceReason::Userinfo,
            host: url
                .domain()
                .and_then(canonical_domain)
                .unwrap_or_else(|| "-".to_owned()),
        };
    }
    match url.domain() {
        Some(d) => match canonical_domain(d) {
            Some(host) => {
                if set.allows(&host, url.port()) {
                    NavDecision::Allow
                } else {
                    NavDecision::Bounce {
                        reason: BounceReason::OutsideList,
                        host,
                    }
                }
            }
            None => NavDecision::Bounce {
                reason: BounceReason::HostForm,
                host: "-".to_owned(),
            },
        },
        // An IP literal never matches. Shown canonically.
        None => NavDecision::Bounce {
            reason: BounceReason::HostForm,
            host: url.host_str().unwrap_or("-").to_owned(),
        },
    }
}

/// The per-pane loop guard. `permit` answers whether a bounce may happen now: the
/// first one does, a second inside BOUNCE_WINDOW does not (fail-open, see the
/// module header). A permitted bounce starts a new window.
#[derive(Debug, Default)]
pub struct BounceGuard {
    last: Mutex<Option<Instant>>,
}

impl BounceGuard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn permit(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(t) if now.saturating_duration_since(t) < BOUNCE_WINDOW => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// How CEF described a new window (kept free of CEF types so it is testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowKind {
    /// A new tab, foreground or background (a `target=_blank` link, a bare
    /// `window.open`, a ctrl or middle click).
    Tab,
    /// A sized popup (`window.open` with features) or a new window.
    Popup,
    /// Anything else (current tab, save-as, unknown, ...).
    Other,
}

impl WindowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowKind::Tab => "tab",
            WindowKind::Popup => "popup",
            WindowKind::Other => "other",
        }
    }
}

/// What to do with a window the page asked to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewWindowDecision {
    /// Behaves as today (the existing closable pane popup, or CEF's default).
    StayInPane,
    /// Hand `url` to the system browser. No modal. `host` is for the log.
    SystemBrowser { url: String, host: String },
}

/// An http(s) URL without userinfo and with a DNS host: the canonical URL and the
/// host, or None. Only this ever reaches the system browser.
pub fn openable(raw: &str) -> Option<(String, String)> {
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.domain().and_then(canonical_domain)?;
    Some((url.as_str().to_owned(), host))
}

/// Rule 3 (decisions.id=934 addendum 2): a link that opens a new TAB from a user
/// click goes to the system browser. A sized popup stays in the pane. A tab with
/// no user gesture, or with a target that is not an openable http(s) URL (a blank
/// `window.open` that navigates later, `blob:`, `mailto:`), stays as it is today.
pub fn classify_new_window(
    kind: WindowKind,
    user_gesture: bool,
    target: &str,
) -> NewWindowDecision {
    if kind != WindowKind::Tab || !user_gesture {
        return NewWindowDecision::StayInPane;
    }
    match openable(target) {
        Some((url, host)) => NewWindowDecision::SystemBrowser { url, host },
        None => NewWindowDecision::StayInPane,
    }
}

/// Host only, for a log line ("-" when the target has none).
pub fn log_host(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "-".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAUNCH: &str = "https://launch.example.test/app";

    fn set(login: &str) -> HostSet {
        HostSet::from_row("fixture", LAUNCH, login)
    }

    fn allowed(set: &HostSet, url: &str) -> bool {
        classify(set, url) == NavDecision::Allow
    }

    fn bounce(set: &HostSet, url: &str) -> Option<(BounceReason, String)> {
        match classify(set, url) {
            NavDecision::Bounce { reason, host } => Some((reason, host)),
            NavDecision::Allow => None,
        }
    }

    #[test]
    fn a_row_that_declares_nothing_is_unrestricted() {
        for json in [
            "[]",
            "null",
            "not json",
            r#"{"a":1}"#,
            r#"[1, "", "*.x.test"]"#,
        ] {
            let s = set(json);
            assert!(!s.is_restricted(), "{json}");
            assert!(allowed(&s, "https://anything.example.test/"), "{json}");
            assert!(
                allowed(&s, "https://user:pw@anything.example.test/"),
                "{json}"
            );
            assert!(allowed(&s, "not a url"), "{json}");
        }
    }

    #[test]
    fn the_launch_host_is_always_listed_and_declared_hosts_add_to_it() {
        let s = set(r#"["signin.example.test", "idp.example.test"]"#);
        assert!(s.is_restricted());
        assert!(allowed(&s, "https://launch.example.test/"));
        assert!(allowed(&s, "https://launch.example.test/a/b?c=d#e"));
        assert!(allowed(&s, "https://signin.example.test/x"));
        assert!(allowed(&s, "https://idp.example.test/"));
        assert_eq!(
            bounce(&s, "https://other.example.test/x"),
            Some((BounceReason::OutsideList, "other.example.test".to_owned()))
        );
        assert_eq!(s.launch_url(), "https://launch.example.test/app");
    }

    #[test]
    fn matching_is_exact_with_no_subdomain_or_suffix_matching() {
        let s = set(r#"["a.test"]"#);
        assert!(allowed(&s, "https://a.test/"));
        for url in [
            "https://x.a.test/",
            "https://a.test.evil.test/",
            "https://evila.test/",
            "https://a.testx/",
            "https://test/",
        ] {
            assert_eq!(
                bounce(&s, url).map(|b| b.0),
                Some(BounceReason::OutsideList),
                "{url}"
            );
        }
    }

    #[test]
    fn case_and_one_trailing_dot_are_normalised_but_not_two() {
        let s = set(r#"["Signin.Example.TEST"]"#);
        assert!(allowed(&s, "https://SIGNIN.example.test/"));
        assert!(allowed(&s, "https://signin.example.test./"));
        assert_eq!(
            bounce(&s, "https://signin.example.test../").map(|b| b.0),
            Some(BounceReason::HostForm)
        );
    }

    #[test]
    fn userinfo_bounces_even_on_a_listed_host() {
        let s = set(r#"["a.test"]"#);
        assert_eq!(
            bounce(&s, "https://user@a.test/"),
            Some((BounceReason::Userinfo, "a.test".to_owned()))
        );
        assert_eq!(
            bounce(&s, "https://user:pw@launch.example.test/").map(|b| b.0),
            Some(BounceReason::Userinfo)
        );
    }

    #[test]
    fn ip_literals_never_match_and_are_shown_canonically() {
        let s = set(r#"["a.test", "127.0.0.1", "[::1]"]"#);
        assert_eq!(
            s.hosts().count(),
            2,
            "IP literals declare nothing: {:?}",
            s.hosts().collect::<Vec<_>>()
        );
        for (url, shown) in [
            ("https://127.0.0.1/", "127.0.0.1"),
            ("https://0x7f.1/", "127.0.0.1"),
            ("https://2130706433/", "127.0.0.1"),
            ("https://[::1]/", "[::1]"),
        ] {
            assert_eq!(
                bounce(&s, url),
                Some((BounceReason::HostForm, shown.to_owned())),
                "{url}"
            );
        }
    }

    #[test]
    fn punycode_and_unicode_lookalikes() {
        let s = set(r#"["bücher.test"]"#);
        assert!(allowed(&s, "https://xn--bcher-kva.test/"));
        assert!(allowed(&s, "https://b\u{fc}cher.test/"));
        // A Cyrillic look-alike of "a" is a different host.
        let s = set(r#"["a.test"]"#);
        assert_eq!(
            bounce(&s, "https://\u{430}.test/").map(|b| b.0),
            Some(BounceReason::OutsideList)
        );
    }

    #[test]
    fn ports_default_only_except_the_launch_port_on_the_launch_host() {
        let s = HostSet::from_row(
            "fixture",
            "https://launch.example.test:8443/",
            r#"["a.test"]"#,
        );
        assert!(allowed(&s, "https://a.test:443/"));
        assert!(allowed(&s, "https://launch.example.test:8443/"));
        assert!(!allowed(&s, "https://a.test:8443/"));
        assert!(!allowed(&s, "https://launch.example.test:9000/"));
        // The default port is allowed on any listed host, the launch host included.
        assert!(allowed(&s, "https://launch.example.test/"));
    }

    #[test]
    fn http_to_a_listed_host_is_judged_by_host() {
        let s = set(r#"["a.test"]"#);
        assert!(allowed(&s, "http://a.test/"));
        assert!(!allowed(&s, "http://b.test/"));
    }

    #[test]
    fn schemes_with_no_host_are_not_judged() {
        let s = set(r#"["a.test"]"#);
        for url in [
            "about:blank",
            "about:srcdoc",
            "mailto:x@y.test",
            "blob:https://b.test/id",
            "data:text/plain,hi",
            "",
        ] {
            assert!(allowed(&s, url), "{url}");
        }
    }

    #[test]
    fn unparsable_and_hostless_http_targets_bounce_with_a_reason() {
        let s = set(r#"["a.test"]"#);
        assert_eq!(
            bounce(&s, "not a url").map(|b| b.0),
            Some(BounceReason::Unparsable)
        );
        assert_eq!(
            bounce(&s, "/relative/path").map(|b| b.0),
            Some(BounceReason::Unparsable)
        );
        assert_eq!(
            bounce(&s, "https://").map(|b| b.0),
            Some(BounceReason::Unparsable)
        );
    }

    #[test]
    fn a_long_url_to_a_listed_host_is_allowed() {
        let s = set(r#"["a.test"]"#);
        let long = format!("https://a.test/{}", "a".repeat(8000));
        assert!(allowed(&s, &long));
        let long_other = format!("https://other.test/{}", "a".repeat(8000));
        assert_eq!(
            bounce(&s, &long_other).map(|b| b.0),
            Some(BounceReason::OutsideList)
        );
    }

    #[test]
    fn a_bad_value_or_entry_declares_nothing_and_fails_open() {
        // Every entry invalid: the row is unrestricted, not "launch host only".
        let s = set(
            r#"["*.a.test", "https://a.test", "a.test/x", "a.test:8443", " a.test", "a..test", "a b.test"]"#,
        );
        assert!(!s.is_restricted());
        // One valid entry among bad ones: the valid one counts, the bad ones do not.
        let s = set(r#"["*.a.test", "ok.test", 7, null]"#);
        assert!(s.is_restricted());
        assert!(allowed(&s, "https://ok.test/"));
        assert!(!allowed(&s, "https://x.a.test/"));
    }

    #[test]
    fn an_unusable_launch_url_fails_open() {
        for launch in [
            "http://launch.example.test/",
            "not a url",
            "https://127.0.0.1/",
            "https://u:p@launch.example.test/",
        ] {
            let s = HostSet::from_row("fixture", launch, r#"["a.test"]"#);
            assert!(!s.is_restricted(), "{launch}");
            assert!(allowed(&s, "https://anywhere.test/"), "{launch}");
        }
    }

    #[test]
    fn declared_entries_are_deduplicated_with_the_launch_host() {
        let s = set(r#"["launch.example.test", "A.test", "a.test", "a.test."]"#);
        assert_eq!(
            s.hosts().collect::<Vec<_>>(),
            vec!["a.test", "launch.example.test"]
        );
    }

    #[test]
    fn normalize_host_cases() {
        assert_eq!(normalize_host("A.Test").as_deref(), Some("a.test"));
        assert_eq!(normalize_host("a.test.").as_deref(), Some("a.test"));
        assert_eq!(
            normalize_host("accounts.google.co.jp").as_deref(),
            Some("accounts.google.co.jp")
        );
        for bad in [
            "",
            "*",
            "*.a.test",
            "a.test:1",
            "a.test/x",
            "http://a.test",
            "1.2.3.4",
            "[::1]",
            "a..test",
            ".a.test",
            "a b",
            "a%2eb.test",
            "u@a.test",
        ] {
            assert_eq!(normalize_host(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_guard_permits_the_first_bounce_and_not_a_second_inside_the_window() {
        let g = BounceGuard::new();
        let t0 = Instant::now();
        assert!(g.permit(t0));
        assert!(!g.permit(t0 + Duration::from_secs(1)));
        assert!(!g.permit(t0 + BOUNCE_WINDOW - Duration::from_millis(1)));
        // The refused attempt did not extend the window.
        assert!(g.permit(t0 + BOUNCE_WINDOW));
        // A permitted bounce starts a new window.
        assert!(!g.permit(t0 + BOUNCE_WINDOW + Duration::from_secs(1)));
    }

    #[test]
    fn a_tab_from_a_user_click_goes_to_the_system_browser() {
        let d = classify_new_window(WindowKind::Tab, true, "https://Other.Example.test/p?q=1#f");
        assert_eq!(
            d,
            NewWindowDecision::SystemBrowser {
                url: "https://other.example.test/p?q=1#f".to_owned(),
                host: "other.example.test".to_owned(),
            }
        );
        assert!(matches!(
            classify_new_window(WindowKind::Tab, true, "http://other.example.test/"),
            NewWindowDecision::SystemBrowser { .. }
        ));
    }

    #[test]
    fn a_sized_popup_a_scripted_tab_or_a_non_web_target_stays_as_it_is() {
        let url = "https://accounts.example.test/signin";
        assert_eq!(
            classify_new_window(WindowKind::Popup, true, url),
            NewWindowDecision::StayInPane
        );
        assert_eq!(
            classify_new_window(WindowKind::Popup, false, url),
            NewWindowDecision::StayInPane
        );
        assert_eq!(
            classify_new_window(WindowKind::Other, true, url),
            NewWindowDecision::StayInPane
        );
        assert_eq!(
            classify_new_window(WindowKind::Tab, false, url),
            NewWindowDecision::StayInPane
        );
        for target in [
            "",
            "about:blank",
            "blob:https://x.test/id",
            "mailto:a@b.test",
            "javascript:void(0)",
            "https://u:p@x.test/",
            "https://127.0.0.1/",
            "not a url",
        ] {
            assert_eq!(
                classify_new_window(WindowKind::Tab, true, target),
                NewWindowDecision::StayInPane,
                "{target:?}"
            );
        }
    }

    #[test]
    fn log_host_carries_the_host_and_nothing_else() {
        assert_eq!(log_host("https://a.test/p?q=secret#f"), "a.test");
        assert_eq!(log_host("https://u:p@a.test:8443/x"), "a.test");
        assert_eq!(log_host("not a url"), "-");
        assert_eq!(log_host(""), "-");
    }
}
