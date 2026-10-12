// src-tauri/src/provider_credentials/url_policy.rs
//
// Which URL, if any, the connect dialog may open for a provider (items.id=714,
// Chat-PM ruling: https, no userinfo, host matches the stored host).
//
// The frontend never supplies a URL. It names a provider and a `Page`; this
// module reads the URL from the provider row and decides whether it is safe to
// hand to the operating system.
//
// "THE STORED HOST". The signed catalog already refuses to change a URL's host
// against the stored row (provider_catalog/apply.rs), so the host of a URL in
// the live row can only differ from the one this release shipped if the
// plaintext shared.db was edited by hand. The anchor is therefore the host of
// the same page in the compiled-in baseline catalog
// (provider_catalog::BASELINE_CATALOG_JSON). A page the baseline has no URL
// for is refused rather than opened unanchored.

use std::sync::OnceLock;

use serde_json::Value;

use crate::persistence::provider_store::Provider;
use crate::provider_catalog::BASELINE_CATALOG_JSON;

/// Longest URL accepted, in bytes (the catalog uses the same bound).
pub const MAX_URL_LEN: usize = 512;

/// A page the connect dialog can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Keys,
    Spend,
}

/// Why no page opens. Both map to stable codes at the IPC layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRefusal {
    /// The row has no URL for this page.
    NoLink,
    /// The row has a URL and it failed the policy.
    Refused,
}

/// The row's URL for `page`, unvalidated.
pub fn row_url(provider: &Provider, page: Page) -> Option<String> {
    match page {
        Page::Keys => provider
            .credential_acquisition
            .as_ref()
            .and_then(|v| v.get("keys_page_url"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        Page::Spend => provider.spend_limit_url.clone(),
    }
}

struct BaselineUrls {
    provider_id: String,
    keys_page_url: Option<String>,
    spend_limit_url: Option<String>,
}

fn baseline() -> &'static [BaselineUrls] {
    static B: OnceLock<Vec<BaselineUrls>> = OnceLock::new();
    B.get_or_init(|| {
        let doc: Value = serde_json::from_str(BASELINE_CATALOG_JSON).unwrap_or(Value::Null);
        let text = |p: &Value, k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        doc.get("providers")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|p| {
                        Some(BaselineUrls {
                            provider_id: text(p, "id")?,
                            keys_page_url: text(p, "keys_page_url"),
                            spend_limit_url: text(p, "spend_limit_url"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// The host this release shipped for `page` of `provider_id`.
pub fn baseline_host(provider_id: &str, page: Page) -> Option<String> {
    let row = baseline().iter().find(|b| b.provider_id == provider_id)?;
    let url = match page {
        Page::Keys => row.keys_page_url.as_deref(),
        Page::Spend => row.spend_limit_url.as_deref(),
    }?;
    policy_host(url)
}

/// The host of a URL that passes the structural policy, else None.
///
/// Structural policy: at most `MAX_URL_LEN` bytes; parses; scheme `https`; no
/// userinfo; a DNS name (no IP literal, not `localhost`); the default port.
fn policy_host(raw: &str) -> Option<String> {
    parse_policy_url(raw).map(|u| u.domain().unwrap_or_default().to_owned())
}

fn parse_policy_url(raw: &str) -> Option<reqwest::Url> {
    if raw.len() > MAX_URL_LEN {
        return None;
    }
    let u = reqwest::Url::parse(raw).ok()?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    // `domain()` is None for an IP literal.
    let domain = u.domain()?;
    if domain == "localhost" || domain.ends_with(".localhost") {
        return None;
    }
    // The url crate drops an explicit default port, so any port left is not 443.
    if u.port().is_some() {
        return None;
    }
    Some(u)
}

/// Check `raw` against the structural policy and `anchor` (the shipped host).
/// Returns the normalised URL text to open.
pub fn checked_url(raw: &str, anchor: Option<&str>) -> Result<String, UrlRefusal> {
    let u = parse_policy_url(raw).ok_or(UrlRefusal::Refused)?;
    let anchor = anchor.ok_or(UrlRefusal::Refused)?;
    if !u.domain().unwrap_or_default().eq_ignore_ascii_case(anchor) {
        return Err(UrlRefusal::Refused);
    }
    Ok(u.as_str().to_owned())
}

/// The URL to open for `page`, or why none.
pub fn resolve(provider: &Provider, page: Page) -> Result<String, UrlRefusal> {
    let raw = row_url(provider, page).ok_or(UrlRefusal::NoLink)?;
    checked_url(&raw, baseline_host(&provider.id, page).as_deref())
}

/// One of the two addresses an OAuth connect uses (items.id=734).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OauthUrl {
    /// Where the user approves (opened in the browser).
    Auth,
    /// Where the approval code is exchanged for a key (called by QR).
    Exchange,
}

/// The OAuth address for `which`, or why none.
///
/// Anchor: the compiled baseline carries no authorization or exchange URL (the
/// signed catalog format has no field for either), so both are held to the
/// host of the same provider's baseline KEYS page, the provider's own host.
/// Both must also carry no query and no fragment: QR appends the flow's
/// parameters itself and a stored value must not pre-load any.
pub fn resolve_oauth(provider: &Provider, which: OauthUrl) -> Result<String, UrlRefusal> {
    let acq = super::acquisition(provider);
    let raw = match which {
        OauthUrl::Auth => acq.auth_url,
        OauthUrl::Exchange => acq.exchange_url,
    }
    .ok_or(UrlRefusal::NoLink)?;
    let checked = checked_url(&raw, baseline_host(&provider.id, Page::Keys).as_deref())?;
    let u = reqwest::Url::parse(&checked).map_err(|_| UrlRefusal::Refused)?;
    if u.query().is_some() || u.fragment().is_some() {
        return Err(UrlRefusal::Refused);
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHOR: &str = "console.example.test";

    #[test]
    fn a_matching_https_url_is_accepted_and_normalised() {
        let out = checked_url("https://console.example.test/keys?a=1&b=2", Some(ANCHOR)).unwrap();
        assert_eq!(out, "https://console.example.test/keys?a=1&b=2");
        let out = checked_url("HTTPS://Console.Example.Test:443/keys", Some(ANCHOR)).unwrap();
        assert_eq!(out, "https://console.example.test/keys");
    }

    #[test]
    fn everything_unsafe_or_unanchored_is_refused() {
        let refused = [
            "http://console.example.test/keys",
            "https://user@console.example.test/keys",
            "https://user:pw@console.example.test/keys",
            "https://127.0.0.1/keys",
            "https://[::1]/keys",
            "https://localhost/keys",
            "https://app.localhost/keys",
            "https://console.example.test:8443/keys",
            "https://other.example.test/keys",
            "https://console.example.test.evil.test/keys",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "not a url",
            "",
        ];
        for raw in refused {
            assert_eq!(
                checked_url(raw, Some(ANCHOR)),
                Err(UrlRefusal::Refused),
                "{raw}"
            );
        }
        let long = format!("https://console.example.test/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(checked_url(&long, Some(ANCHOR)), Err(UrlRefusal::Refused));
    }

    #[test]
    fn a_page_with_no_shipped_host_is_never_opened() {
        assert_eq!(
            checked_url("https://console.example.test/keys", None),
            Err(UrlRefusal::Refused)
        );
    }

    #[test]
    fn the_baseline_anchors_every_row_url_this_release_ships() {
        // Every URL the baseline itself carries must pass its own policy, or a
        // shipped link would be unopenable.
        assert!(!baseline().is_empty(), "the baseline lists providers");
        for b in baseline() {
            for (page, url) in [
                (Page::Keys, &b.keys_page_url),
                (Page::Spend, &b.spend_limit_url),
            ] {
                if let Some(url) = url {
                    let host = baseline_host(&b.provider_id, page)
                        .unwrap_or_else(|| panic!("{} {page:?} fails the policy", b.provider_id));
                    assert!(checked_url(url, Some(&host)).is_ok(), "{url}");
                }
            }
        }
    }
}
