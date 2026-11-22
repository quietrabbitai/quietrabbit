// src-tauri/src/persistence/document_session_store_tests.rs
//
// items.id=903 unit B: the document session store against a real migrated outputs.db.

use super::*;
use crate::persistence::session_store::latch_write_suppressed;
use crate::persistence::session_store::test_db::mem_outputs_conn;

const CHAT: &str = "chat-1";
const FOCUS: &str = "writing-assistant";

#[test]
fn every_flag_round_trips_and_only_three_restrict() {
    for f in DocumentPrivacyFlag::ALL {
        assert_eq!(DocumentPrivacyFlag::parse(f.as_str()), Some(f));
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, format!("\"{}\"", f.as_str()));
    }
    assert_eq!(DocumentPrivacyFlag::parse("secret"), None);
    let restricting: Vec<&str> = DocumentPrivacyFlag::ALL
        .iter()
        .filter(|f| f.restricts_to_local())
        .map(|f| f.as_str())
        .collect();
    assert_eq!(restricting, ["medical", "financial", "legal"]);
}

#[tokio::test]
async fn a_flag_is_created_then_replaced_with_the_version_moving_up() {
    let mut c = mem_outputs_conn().await;
    assert!(get(&mut c, CHAT, FOCUS).await.unwrap().is_none());
    let s = set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Personal,
    )
    .await
    .unwrap();
    assert_eq!(s.version, 1);
    assert_eq!(s.document_privacy_flag, Some(DocumentPrivacyFlag::Personal));
    assert_eq!(s.raised_ceiling, None);
    let s = set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Medical,
    )
    .await
    .unwrap();
    assert_eq!(s.version, 2);
    assert_eq!(get(&mut c, CHAT, FOCUS).await.unwrap(), Some(s));
    // Another chat and another Focus are separate rows.
    assert!(get(&mut c, "chat-2", FOCUS).await.unwrap().is_none());
    assert!(get(&mut c, CHAT, "quick-ask").await.unwrap().is_none());
}

#[tokio::test]
async fn a_changed_document_type_voids_a_raise_and_keeps_the_new_flag() {
    let mut c = mem_outputs_conn().await;
    set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Legal,
    )
    .await
    .unwrap();
    // A raise as unit D will write it (no writer exists in unit B).
    sqlx::query(
        "UPDATE document_sessions SET raised_ceiling = 'anonymous_required',
         consent_audit_id = 'audit-1' WHERE context_key = ?",
    )
    .bind(CHAT)
    .execute(&mut c)
    .await
    .unwrap();
    // The same type keeps it.
    let s = set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Legal,
    )
    .await
    .unwrap();
    assert_eq!(s.raised_ceiling, Some(ExternalAccess::AnonymousRequired));
    assert_eq!(s.consent_audit_id.as_deref(), Some("audit-1"));
    // Another type voids it.
    let s = set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "rma_letter",
        DocumentPrivacyFlag::Medical,
    )
    .await
    .unwrap();
    assert_eq!(s.document_type, "rma_letter");
    assert_eq!(s.raised_ceiling, None);
    assert_eq!(s.consent_audit_id, None);
    assert_eq!(s.document_privacy_flag, Some(DocumentPrivacyFlag::Medical));
}

#[tokio::test]
async fn a_one_time_chat_refuses_the_flag_and_writes_nothing() {
    let mut c = mem_outputs_conn().await;
    latch_write_suppressed(&mut c, CHAT, FOCUS, "run-1")
        .await
        .unwrap();
    let e = set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Medical,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(e, DocumentSessionError::Latch(LatchError::Suppressed)),
        "{e:?}"
    );
    assert!(get(&mut c, CHAT, FOCUS).await.unwrap().is_none());
}

#[tokio::test]
async fn a_corrupt_value_is_an_error_never_skipped() {
    let mut c = mem_outputs_conn().await;
    set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::General,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE document_sessions SET document_privacy_flag = 'secret'")
        .execute(&mut c)
        .await
        .unwrap();
    assert!(matches!(
        get(&mut c, CHAT, FOCUS).await,
        Err(DocumentSessionError::Corrupt("privacy flag"))
    ));
    sqlx::query(
        "UPDATE document_sessions SET document_privacy_flag = NULL, raised_ceiling = 'everything'",
    )
    .execute(&mut c)
    .await
    .unwrap();
    assert!(matches!(
        get(&mut c, CHAT, FOCUS).await,
        Err(DocumentSessionError::Corrupt("raised ceiling"))
    ));
}

#[test]
fn the_ceiling_rule_restricts_only_on_a_sensitive_flag() {
    let mut s = DocumentSession {
        context_key: CHAT.to_owned(),
        focus_id: FOCUS.to_owned(),
        document_type: "cover_letter".to_owned(),
        document_privacy_flag: None,
        raised_ceiling: None,
        consent_audit_id: None,
        version: 1,
        created_at: String::new(),
        updated_at: String::new(),
    };
    assert_eq!(restricted_ceiling(None, "cover_letter"), None);
    assert_eq!(restricted_ceiling(Some(&s), "cover_letter"), None);
    for f in DocumentPrivacyFlag::ALL {
        s.document_privacy_flag = Some(f);
        let want = f.restricts_to_local().then_some(ExternalAccess::LocalOnly);
        assert_eq!(restricted_ceiling(Some(&s), "cover_letter"), want, "{f:?}");
        // The flag applies whatever type the run selected.
        assert_eq!(restricted_ceiling(Some(&s), "rma_letter"), want, "{f:?}");
    }
}

async fn run_and_output(c: &mut SqliteConnection, run: &str, chat: &str, focus: &str, out: &str) {
    sqlx::query(
        "INSERT INTO focus_runs (id, focus_id, status, started_at, context_key)
         VALUES (?, ?, 'complete', datetime('now'), ?)",
    )
    .bind(run)
    .bind(focus)
    .bind(chat)
    .execute(&mut *c)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO outputs (id, focus_run_id, output_type, content, created_at, updated_at)
         VALUES (?, ?, 'document', 'x', datetime('now'), datetime('now'))",
    )
    .bind(out)
    .bind(run)
    .execute(&mut *c)
    .await
    .unwrap();
}

async fn output_flag(c: &mut SqliteConnection, out: &str) -> Option<String> {
    sqlx::query_scalar("SELECT document_privacy_flag FROM outputs WHERE id = ?")
        .bind(out)
        .fetch_one(&mut *c)
        .await
        .unwrap()
}

#[tokio::test]
async fn only_an_output_of_the_same_chat_and_focus_is_stamped() {
    let mut c = mem_outputs_conn().await;
    run_and_output(&mut c, "r1", CHAT, FOCUS, "o1").await;
    run_and_output(&mut c, "r2", "chat-2", FOCUS, "o2").await;
    run_and_output(&mut c, "r3", CHAT, "quick-ask", "o3").await;
    assert!(
        stamp_output_flag(&mut c, "o1", CHAT, FOCUS, DocumentPrivacyFlag::Medical)
            .await
            .unwrap()
    );
    assert_eq!(output_flag(&mut c, "o1").await.as_deref(), Some("medical"));
    for other in ["o2", "o3", "nope"] {
        assert!(
            !stamp_output_flag(&mut c, other, CHAT, FOCUS, DocumentPrivacyFlag::Legal)
                .await
                .unwrap()
        );
    }
    assert_eq!(output_flag(&mut c, "o2").await, None);
    assert_eq!(output_flag(&mut c, "o3").await, None);
}

#[tokio::test]
async fn the_archive_clear_and_the_sweep_remove_rows() {
    let mut c = mem_outputs_conn().await;
    set_privacy_flag(
        &mut c,
        CHAT,
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Legal,
    )
    .await
    .unwrap();
    set_privacy_flag(
        &mut c,
        "chat-2",
        FOCUS,
        "cover_letter",
        DocumentPrivacyFlag::Legal,
    )
    .await
    .unwrap();
    assert_eq!(clear_chat_in_tx(&mut c, CHAT).await.unwrap(), 1);
    assert!(get(&mut c, CHAT, FOCUS).await.unwrap().is_none());
    assert!(get(&mut c, "chat-2", FOCUS).await.unwrap().is_some());
    // The sweep compares updated_at with the cutoff text.
    assert_eq!(
        sweep_older_than_in_tx(&mut c, "1970-01-01T00:00:00+00:00")
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sweep_older_than_in_tx(&mut c, "9999-01-01T00:00:00+00:00")
            .await
            .unwrap(),
        1
    );
}

#[test]
fn the_table_has_no_text_column_and_the_store_logs_nothing() {
    let sql = include_str!("../../schema/outputs_020.sql");
    let table = sql
        .split("CREATE TABLE IF NOT EXISTS document_sessions (")
        .nth(1)
        .unwrap()
        .split(");")
        .next()
        .unwrap();
    let columns: Vec<String> = table
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        columns,
        [
            "context_key",
            "focus_id",
            "document_type",
            "document_privacy_flag",
            "raised_ceiling",
            "consent_audit_id",
            "version",
            "created_at",
            "updated_at"
        ]
    );
    let src = include_str!("document_session_store.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    for banned in ["log::", "println!", "eprintln!", "tracing::"] {
        assert!(!prod.contains(banned), "the store uses {banned}");
    }
}
