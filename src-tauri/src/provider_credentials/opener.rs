// src-tauri/src/provider_credentials/opener.rs
//
// The one place that asks the operating system to open a URL (items.id=714).
// Uses the `open` crate, called from Rust only: no Tauri opener plugin and no
// capability permission exists, so the webview cannot open anything itself
// (a source-scan test in provider_credentials/tests.rs pins both).
//
// Callers pass a URL that url_policy::resolve already accepted. The seam is a
// trait so tests record the URL instead of launching a browser.

/// Opens a URL in the user's default browser.
pub trait UrlOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// The real opener.
pub struct SystemOpener;

impl UrlOpener for SystemOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        // Detached: the browser process outlives this call and its output is
        // not ours. The error text carries no URL.
        open::that_detached(url).map_err(|e| format!("could not open the browser: {}", e.kind()))
    }
}
