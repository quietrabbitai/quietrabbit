// src-tauri/src/provider_credentials/mod.rs
//
// Credential acquisition, part (a) of items.id=714 (decisions.id=875 point 7,
// spec Part 8g, AUTH section 16 point 3): the pieces the connect dialog
// needs to put a provider key in the store properly.
//
//   url_policy   which provider page may be opened (https, no userinfo, host
//                anchored to the shipped baseline); the frontend never sends
//                a URL
//   clipboard    the one place that reads the clipboard (Rust side, on a click)
//   opener       the one place that asks the OS to open a URL (`open` crate)
//   key_pattern  key syntax and the row-data clipboard pattern
//   session      the in-memory "connect dialog is open" state
//   flow         check a candidate key, then store it with its user variables
//   oauth        part (b), items.id=734: the OpenRouter PKCE connect (pkce,
//                the loopback callback listener, the code exchange, the flow
//                that ties them to the session)
//
// What is deliberately NOT here: any Ollama sign-in (post-R1; decisions.id=840's sidecar is untouched), any scraping of
// a provider page (decisions.id=665), anything that runs on its own.
//
// SECRET HYGIENE. A key, clipboard text or variable value is never logged,
// never put in an error code, never returned over IPC (only stable codes and
// lengths are), and every type that can hold one has a redacting Debug.

pub mod clipboard;
pub mod flow;
pub mod key_pattern;
pub mod oauth;
pub mod opener;
pub mod session;
pub mod url_policy;

#[cfg(test)]
mod oauth_tests;
#[cfg(test)]
mod tests;

use serde_json::Value;

use crate::persistence::provider_store::Provider;

/// The fields of `providers.credential_acquisition` this code reads. Rows
/// with a missing or malformed value read as all-None (fail closed).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Acquisition {
    pub method: Option<String>,
    pub key_pattern: Option<String>,
    /// Where the provider asks the user to approve (`oauth_pkce` rows).
    pub auth_url: Option<String>,
    /// Where the approval code is exchanged for a key (`oauth_pkce` rows).
    pub exchange_url: Option<String>,
    /// A path under the provider's API base that needs the key and nothing
    /// else, so a wrong key fails it. When absent the model list is the check.
    pub key_check_path: Option<String>,
}

pub fn acquisition(provider: &Provider) -> Acquisition {
    let text = |k: &str| {
        provider
            .credential_acquisition
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Acquisition {
        method: text("method"),
        key_pattern: text("key_pattern"),
        auth_url: text("auth_url"),
        exchange_url: text("exchange_url"),
        key_check_path: text("key_check_path"),
    }
}

/// A hosted provider row: the only kind this module connects.
pub fn is_hosted(p: &Provider) -> bool {
    p.provider_type == "cloud_inference_api" && p.api_dialect.is_some()
}

/// The variable names the row declares (`user_variables`), in row order.
/// None when the list is malformed.
pub fn declared_variables(provider: &Provider) -> Option<Vec<String>> {
    match &provider.user_variables {
        None => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(_) => None,
    }
}
