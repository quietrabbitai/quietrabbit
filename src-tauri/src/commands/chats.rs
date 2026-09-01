// src-tauri/src/commands/chats.rs
//
// Group 22 — Persona-scoped chat history. Commands: create_chat,
// list_chats, archive_chat.
//
// items.id=384 slice 6 (decisions.id=739/740): backs the persona-scoped
// chat-history list/switcher UI (slice 7) that ChatPane.tsx's Tier3
// Access / merged-workspace usage gets, per the reference mockup's
// chat-history icon. Backed by persistence/chat_store.rs, itself living
// in the same messages.db message_store.rs already owns.
//
// user_id/persona_id via IPC, key_hex derived server-side from
// KeyRegistry: same Release-1-no-auth-layer-yet convention
// commands/library.rs's own module header documents.

use serde::Serialize;
use specta::Type;
use tauri::State;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::persistence::chat_store;

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
}

fn to_chat_info(r: chat_store::ChatRecord) -> ChatInfo {
    ChatInfo {
        id: r.id,
        persona_id: r.persona_id,
        context_key: r.context_key,
        title: r.title,
        archived_at: r.archived_at,
        created_at: r.created_at,
        last_message_at: r.last_message_at,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Starts a new chat for a Persona. decisions.id=739: the previously
/// current chat is not touched by this call at all -- its messages stay
/// exactly where they were saved (message_store::save_message persists
/// per-message, not per-chat-on-close), so "new chat auto-saves to
/// history" is satisfied by this command simply handing back a fresh
/// chat_id/context_key, not by any explicit save step here.
#[tauri::command]
#[specta::specta]
pub async fn create_chat(
    user_id: String,
    persona_id: String,
    key_registry: State<'_, KeyRegistry>,
) -> Result<ChatInfo, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.master_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    let record = chat_store::create_chat(&user_id, &persona_id, &key_hex_str)
        .await
        .map_err(|e| e.to_string())?;
    Ok(to_chat_info(record))
}

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
    Ok(records.into_iter().map(to_chat_info).collect())
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
