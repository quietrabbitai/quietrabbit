// src-tauri/src/persistence/chat_binding_store.rs
//
// items.id=716 (decisions.id=874, 875): the provider and model an API Chat
// conversation is bound to. Lives in the same per-user, per-Persona encrypted
// messages.db as message_store.rs and chat_store.rs (schema: messages_007.sql)
// and reuses that module's opener.
//
// WHY NOT COLUMNS ON `chats`: a `chats` row is created lazily, only once a
// real reply is saved (items.id=587, decisions.id=844). A binding has to
// exist before the first run, so it has its own table, keyed by context_key.
//
// IMMUTABLE: `create_binding` is the only writer and a second insert for the
// same context_key fails. There is no update or delete path, on purpose: the
// history of a bound conversation only ever goes to the destination the user
// bound it to.
//
// A context_key with no row is a QR chat (every chat that existed before
// messages_007.sql). `kind` is validated by the `ChatKind` enum, not by a SQL
// CHECK, so a later kind needs no table rebuild.
//
// QUERY STYLE: runtime sqlx::query() only, as in message_store.rs.

use std::collections::HashMap;

use sqlx::Connection;
use sqlx::Row;
use thiserror::Error;

use crate::persistence::message_store::{self, MessageStoreError};

/// What kind of conversation this is. Extensible: a local large-model kind
/// may join later (items.id=717 design pending, not built here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatKind {
    /// A QR chat: self-contained, never sends to an external provider.
    Qr,
    /// An API Chat: bound to a hosted provider and model.
    Api,
}

impl ChatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qr => "qr",
            Self::Api => "api",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "qr" => Some(Self::Qr),
            "api" => Some(Self::Api),
            _ => None,
        }
    }
}

/// One slot's destination. Only slot 0 exists today; a later fan-out adds
/// slots 1 and 2 without a schema change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingTarget {
    pub slot: i64,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatBinding {
    pub context_key: String,
    pub kind: ChatKind,
    /// Ordered by slot. Never empty for a stored binding.
    pub targets: Vec<BindingTarget>,
}

impl ChatBinding {
    /// The single-provider destination (slot 0).
    pub fn primary(&self) -> Option<&BindingTarget> {
        self.targets.iter().find(|t| t.slot == 0)
    }
}

#[derive(Debug, Error)]
pub enum ChatBindingError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    MessagesDb(#[from] MessageStoreError),
    #[error("Validation error: {0}")]
    Validation(String),
    /// A conversation is bound once. This context_key already has a binding.
    #[error("this conversation is already bound")]
    AlreadyBound,
    #[error("unknown chat kind '{0}'")]
    UnknownKind(String),
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// Bind `context_key` to one provider and model (slot 0). Fails with
/// `AlreadyBound` if the conversation already has a binding.
pub async fn create_binding(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
    kind: ChatKind,
    provider_id: &str,
    model_id: &str,
) -> Result<ChatBinding, ChatBindingError> {
    if context_key.is_empty() || provider_id.is_empty() || model_id.is_empty() {
        return Err(ChatBindingError::Validation(
            "a binding needs a context_key, a provider_id and a model_id".to_owned(),
        ));
    }
    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;
    let timestamp = crate::providers::utils::now();

    let mut tx = conn.begin().await?;
    let inserted =
        sqlx::query("INSERT INTO chat_bindings (context_key, kind, created_at) VALUES (?, ?, ?)")
            .bind(context_key)
            .bind(kind.as_str())
            .bind(&timestamp)
            .execute(&mut *tx)
            .await;
    if let Err(e) = inserted {
        return Err(if is_unique_violation(&e) {
            ChatBindingError::AlreadyBound
        } else {
            ChatBindingError::Database(e)
        });
    }
    sqlx::query(
        "INSERT INTO chat_binding_targets (context_key, slot, provider_id, model_id, created_at)
         VALUES (?, 0, ?, ?, ?)",
    )
    .bind(context_key)
    .bind(provider_id)
    .bind(model_id)
    .bind(&timestamp)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(ChatBinding {
        context_key: context_key.to_owned(),
        kind,
        targets: vec![BindingTarget {
            slot: 0,
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
        }],
    })
}

/// The binding of `context_key`, or None for a QR chat. A stored kind this
/// build does not know is an error, never a quiet default to QR: treating an
/// unknown (possibly external) kind as QR would hide external use.
pub async fn get_binding(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    context_key: &str,
) -> Result<Option<ChatBinding>, ChatBindingError> {
    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;

    let header = sqlx::query("SELECT kind FROM chat_bindings WHERE context_key = ?")
        .bind(context_key)
        .fetch_optional(&mut conn)
        .await?;
    let Some(header) = header else {
        return Ok(None);
    };
    let kind_text: String = header.try_get("kind")?;
    let kind = ChatKind::parse(&kind_text).ok_or(ChatBindingError::UnknownKind(kind_text))?;

    let rows = sqlx::query(
        "SELECT slot, provider_id, model_id FROM chat_binding_targets
         WHERE context_key = ? ORDER BY slot ASC",
    )
    .bind(context_key)
    .fetch_all(&mut conn)
    .await?;
    let targets: Vec<BindingTarget> = rows
        .iter()
        .map(|r| {
            Ok(BindingTarget {
                slot: r.try_get("slot")?,
                provider_id: r.try_get("provider_id")?,
                model_id: r.try_get("model_id")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()?;
    if targets.is_empty() {
        return Err(ChatBindingError::Validation(format!(
            "binding for '{context_key}' has no target"
        )));
    }
    Ok(Some(ChatBinding {
        context_key: context_key.to_owned(),
        kind,
        targets,
    }))
}

/// Every binding of a Persona by context_key, for list views. Fails on an
/// unknown kind for the same reason `get_binding` does.
pub async fn list_bindings(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
) -> Result<HashMap<String, ChatBinding>, ChatBindingError> {
    let mut conn = message_store::open_messages_db(user_id, persona_id, key_hex).await?;

    let headers = sqlx::query("SELECT context_key, kind FROM chat_bindings")
        .fetch_all(&mut conn)
        .await?;
    let mut out: HashMap<String, ChatBinding> = HashMap::with_capacity(headers.len());
    for h in &headers {
        let context_key: String = h.try_get("context_key")?;
        let kind_text: String = h.try_get("kind")?;
        let kind = ChatKind::parse(&kind_text).ok_or(ChatBindingError::UnknownKind(kind_text))?;
        out.insert(
            context_key.clone(),
            ChatBinding {
                context_key,
                kind,
                targets: Vec::new(),
            },
        );
    }
    let targets = sqlx::query(
        "SELECT context_key, slot, provider_id, model_id FROM chat_binding_targets
         ORDER BY context_key, slot ASC",
    )
    .fetch_all(&mut conn)
    .await?;
    for r in &targets {
        let context_key: String = r.try_get("context_key")?;
        if let Some(b) = out.get_mut(&context_key) {
            b.targets.push(BindingTarget {
                slot: r.try_get("slot")?,
                provider_id: r.try_get("provider_id")?,
                model_id: r.try_get("model_id")?,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_MUTEX;

    const USER_ID: &str = "user-bind-test";
    const PERSONA_ID: &str = "persona-bind-test";
    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("messages.db migration must succeed");
        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    #[tokio::test]
    async fn a_binding_round_trips_with_its_slot_zero_target() {
        let _env = setup().await;
        let made = create_binding(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-a",
            ChatKind::Api,
            "prov-x",
            "model-1",
        )
        .await
        .expect("create");
        assert_eq!(made.kind, ChatKind::Api);

        let got = get_binding(USER_ID, PERSONA_ID, KEY_HEX, "chat-a")
            .await
            .expect("get")
            .expect("bound");
        assert_eq!(got, made);
        let primary = got.primary().expect("slot 0");
        assert_eq!(primary.provider_id, "prov-x");
        assert_eq!(primary.model_id, "model-1");
    }

    #[tokio::test]
    async fn a_conversation_with_no_row_is_not_bound() {
        let _env = setup().await;
        assert!(get_binding(USER_ID, PERSONA_ID, KEY_HEX, "chat-none")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn a_binding_is_immutable_a_second_create_fails_and_changes_nothing() {
        let _env = setup().await;
        create_binding(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-b",
            ChatKind::Api,
            "prov-x",
            "model-1",
        )
        .await
        .expect("first");

        let second = create_binding(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-b",
            ChatKind::Api,
            "prov-y",
            "model-2",
        )
        .await;
        assert!(matches!(second, Err(ChatBindingError::AlreadyBound)));

        let got = get_binding(USER_ID, PERSONA_ID, KEY_HEX, "chat-b")
            .await
            .expect("get")
            .expect("bound");
        let primary = got.primary().expect("slot 0");
        assert_eq!(
            (primary.provider_id.as_str(), primary.model_id.as_str()),
            ("prov-x", "model-1"),
            "the original destination must stand"
        );
        assert_eq!(got.targets.len(), 1, "no second target was added");
    }

    #[tokio::test]
    async fn an_unknown_stored_kind_is_an_error_never_a_quiet_qr_chat() {
        let _env = setup().await;
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open");
        sqlx::query("INSERT INTO chat_bindings (context_key, kind, created_at) VALUES ('chat-k', 'local', 't')")
            .execute(&mut conn)
            .await
            .expect("seed a kind this build does not know");
        drop(conn);

        let got = get_binding(USER_ID, PERSONA_ID, KEY_HEX, "chat-k").await;
        assert!(matches!(got, Err(ChatBindingError::UnknownKind(k)) if k == "local"));
        let listed = list_bindings(USER_ID, PERSONA_ID, KEY_HEX).await;
        assert!(matches!(listed, Err(ChatBindingError::UnknownKind(_))));
    }

    #[tokio::test]
    async fn a_target_must_name_a_binding_and_slots_are_unique() {
        let _env = setup().await;
        create_binding(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-c",
            ChatKind::Api,
            "prov-x",
            "model-1",
        )
        .await
        .expect("create");
        let mut conn = message_store::open_messages_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("open");
        let dup = sqlx::query(
            "INSERT INTO chat_binding_targets (context_key, slot, provider_id, model_id, created_at)
             VALUES ('chat-c', 0, 'p', 'm', 't')",
        )
        .execute(&mut conn)
        .await;
        assert!(dup.is_err(), "slot 0 of a binding can exist only once");
        // A fan-out slot is plain data, no schema change.
        sqlx::query(
            "INSERT INTO chat_binding_targets (context_key, slot, provider_id, model_id, created_at)
             VALUES ('chat-c', 1, 'prov-y', 'model-2', 't')",
        )
        .execute(&mut conn)
        .await
        .expect("slot 1 is allowed by the schema");
        drop(conn);
        let got = get_binding(USER_ID, PERSONA_ID, KEY_HEX, "chat-c")
            .await
            .expect("get")
            .expect("bound");
        assert_eq!(got.targets.len(), 2);
        assert_eq!(got.primary().expect("slot 0").provider_id, "prov-x");
    }

    #[tokio::test]
    async fn list_bindings_returns_every_binding_by_context_key() {
        let _env = setup().await;
        for k in ["chat-1", "chat-2"] {
            create_binding(
                USER_ID,
                PERSONA_ID,
                KEY_HEX,
                k,
                ChatKind::Api,
                "prov-x",
                "m",
            )
            .await
            .expect("create");
        }
        let all = list_bindings(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("list");
        assert_eq!(all.len(), 2);
        assert!(all
            .values()
            .all(|b| b.kind == ChatKind::Api && b.targets.len() == 1));
    }

    #[test]
    fn chat_kind_round_trips_and_rejects_unknown_text() {
        for k in [ChatKind::Qr, ChatKind::Api] {
            assert_eq!(ChatKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(ChatKind::parse("local"), None);
        assert_eq!(ChatKind::parse(""), None);
    }

    #[tokio::test]
    async fn a_binding_needs_every_field() {
        let _env = setup().await;
        let r = create_binding(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            "chat-d",
            ChatKind::Api,
            "",
            "m",
        )
        .await;
        assert!(matches!(r, Err(ChatBindingError::Validation(_))));
    }
}
