// src-tauri/src/provider_credentials/clipboard.rs
//
// The one place that reads the system clipboard (items.id=714). It is called
// only from the check_clipboard_for_key command, which requires an open
// connect-dialog session and is itself the user's explicit confirm. A
// source-scan test pins `read_text` to this file.
//
// The Rust-side plugin API needs no capability permission, so the webview is
// granted nothing: capabilities/default.json is unchanged.

use zeroize::Zeroizing;

/// Reads the clipboard as text. None when it is empty, not text, or
/// unavailable.
pub trait ClipboardSource {
    fn read_text(&self) -> Option<Zeroizing<String>>;
}

/// The real clipboard, through the registered clipboard plugin.
pub struct PluginClipboard(pub tauri::AppHandle);

impl ClipboardSource for PluginClipboard {
    fn read_text(&self) -> Option<Zeroizing<String>> {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        self.0.clipboard().read_text().ok().map(Zeroizing::new)
    }
}
