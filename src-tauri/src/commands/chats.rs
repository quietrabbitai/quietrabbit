// src-tauri/src/commands/chats.rs
//
// Group 22 — Persona-scoped chat history. Commands: list_chats,
// archive_chat.
//
// items.id=384 slice 6 (decisions.id=739/740): backs the persona-scoped
// chat-history list/switcher UI (slice 7) that ChatPane.tsx's Cloud Chat
// / merged-workspace usage gets, per the reference mockup's
// chat-history icon. Backed by persistence/chat_store.rs, itself living
// in the same messages.db message_store.rs already owns.
//
// items.id=546: there used to be a create_chat command here too, called
// eagerly the instant a persona was picked in the UI, before any message
// existed. Removed -- chat creation is now lazy, folded into
// commands/messages.rs::send_message's own call to
// chat_store::ensure_chat_and_bump_activity, which creates the row on the
// first message actually sent under a given context_key. See that
// function's doc comment for why.
//
// user_id/persona_id via IPC, key_hex derived server-side from
// KeyRegistry: same Release-1-no-auth-layer-yet convention
// commands/library.rs's own module header documents.

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::{chat_binding_store, chat_store};

// ---------------------------------------------------------------------------
// Response DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct ChatInfo {
    pub id: String,
    pub persona_id: String,
    pub context_key: String,
    pub title: Option<String>,
    pub archived_at: Option<String>,
    pub created_at: String,
    pub last_message_at: String,
    /// items.id=716: "qr" for a QR chat (no binding row) or "api" for an API
    /// Chat, so History can keep the two surfaces apart.
    pub kind: String,
    /// The provider and model an API Chat is bound to; None for a QR chat.
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
}

fn to_chat_info(
    r: chat_store::ChatRecord,
    binding: Option<&chat_binding_store::ChatBinding>,
) -> ChatInfo {
    let target = binding.and_then(|b| b.primary());
    ChatInfo {
        id: r.id,
        persona_id: r.persona_id,
        context_key: r.context_key,
        title: r.title,
        archived_at: r.archived_at,
        created_at: r.created_at,
        last_message_at: r.last_message_at,
        kind: binding
            .map(|b| b.kind)
            .unwrap_or(chat_binding_store::ChatKind::Qr)
            .as_str()
            .to_owned(),
        provider_id: target.map(|t| t.provider_id.clone()),
        model_id: target.map(|t| t.model_id.clone()),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Lists a Persona's non-archived chats, most-recent-first.
#[tauri::command]
#[specta::specta]
pub async fn list_chats(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<Vec<ChatInfo>, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let records = chat_store::list_chats(&user_id, &persona_id, &key_hex_str)
        .await
        .map_err(|e| e.to_string())?;
    // An unreadable or unknown binding fails the list rather than showing an
    // external conversation as a QR chat.
    let bindings = chat_binding_store::list_bindings(&user_id, &persona_id, &key_hex_str)
        .await
        .map_err(|e| e.to_string())?;
    Ok(records
        .into_iter()
        .map(|r| {
            let binding = bindings.get(&r.context_key);
            to_chat_info(r, binding)
        })
        .collect())
}

/// Archives a chat. decisions.id=739: explicit action only, never
/// implicit or bundled with create_chat -- see that command's own doc.
#[tauri::command]
#[specta::specta]
pub async fn archive_chat(
    user_id: String,
    persona_id: String,
    chat_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    chat_store::archive_chat(&user_id, &persona_id, &key_hex_str, &chat_id)
        .await
        .map_err(|e| e.to_string())
}
