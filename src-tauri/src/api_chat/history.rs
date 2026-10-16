// src-tauri/src/api_chat/history.rs
//
// The history prefix of an API conversation (items.id=716). Same shape as the
// QR prefix (`User: ...\nAssistant: ...\n`, the last HISTORY_WINDOW messages),
// with the rules that make it safe to send to a provider:
//
//   - It is built from ONE conversation's messages (the caller passes the rows
//     of the bound context_key). A message from anywhere else can only be in
//     those rows through an explicit transfer, which is not built yet.
//   - A user turn contributes the text that was actually SENT
//     (messages.outbound_content, after the Privacy Guardian applied the
//     user's decisions), never the text as typed. A span the user kept private
//     is therefore never sent later, and never has to be asked about again.
//     A user turn with no outbound text is left out and counted: it was never
//     reviewed for sending.
//   - A reply Gate 2 flagged is left out when the destination is a different
//     provider from the one that produced it, or when the producer is not
//     recorded (decisions.id=883, Chat-PM ruling on items.id=716). The count
//     is returned so the omission is never silent.
//   - A reply that was cancelled, cut or truncated stays out of later history
//     (it is kept for the user to read, not to feed back to a model).
//   - Empty placeholders and failure messages are skipped, as in the QR prefix.

use crate::api_chat::BoundDestination;
use crate::persistence::message_store::{MessageRecord, ReplyStatus};

/// How many recent messages are folded into a send. The same window as the QR
/// prefix (commands::messages::HISTORY_WINDOW).
pub const HISTORY_WINDOW: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiHistory {
    pub prompt: String,
    /// Flagged replies left out because the destination differs from (or the
    /// message does not record) the provider that produced them.
    pub omitted_flagged: usize,
    /// User turns left out because they carry no reviewed outbound text.
    pub omitted_unreviewed: usize,
}

pub fn build_api_history(history: &[MessageRecord], destination: &BoundDestination) -> ApiHistory {
    let start = history.len().saturating_sub(HISTORY_WINDOW);
    let mut prompt = String::new();
    let mut omitted_flagged = 0;
    let mut omitted_unreviewed = 0;

    for m in &history[start..] {
        if m.content.is_empty() || m.is_error || m.reply_status != ReplyStatus::Complete {
            continue;
        }
        let (label, text) = if m.sender == "user" {
            match m.outbound_content.as_deref() {
                Some(sent) if !sent.is_empty() => ("User", sent),
                _ => {
                    omitted_unreviewed += 1;
                    continue;
                }
            }
        } else {
            if m.personal_flagged
                && m.provider_id.as_deref() != Some(destination.provider_id.as_str())
            {
                omitted_flagged += 1;
                continue;
            }
            ("Assistant", m.content.as_str())
        };
        prompt.push_str(label);
        prompt.push_str(": ");
        prompt.push_str(text);
        prompt.push('\n');
    }

    ApiHistory {
        prompt,
        omitted_flagged,
        omitted_unreviewed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest() -> BoundDestination {
        BoundDestination {
            provider_id: "prov-x".to_owned(),
            model_id: "model-1".to_owned(),
        }
    }

    fn base(sender: &str, content: &str) -> MessageRecord {
        MessageRecord {
            id: uuid::Uuid::new_v4().to_string(),
            context_key: "chat-1".to_owned(),
            sender: sender.to_owned(),
            content: content.to_owned(),
            focus_run_id: None,
            created_at: "2026-10-12T00:00:00Z".to_owned(),
            is_error: false,
            personal_flagged: false,
            provider_id: None,
            model_id: None,
            reply_status: ReplyStatus::Complete,
            outbound_content: None,
        }
    }

    fn user(typed: &str, sent: &str) -> MessageRecord {
        MessageRecord {
            outbound_content: Some(sent.to_owned()),
            ..base("user", typed)
        }
    }

    fn reply(content: &str, provider: Option<&str>) -> MessageRecord {
        MessageRecord {
            provider_id: provider.map(str::to_owned),
            model_id: provider.map(|_| "model-1".to_owned()),
            ..base("assistant", content)
        }
    }

    #[test]
    fn it_formats_like_the_qr_prefix_and_uses_the_sent_text_for_user_turns() {
        let h = vec![
            user("my number is 555-0100", "my number is [removed]"),
            reply("ok", Some("prov-x")),
        ];
        let out = build_api_history(&h, &dest());
        assert_eq!(out.prompt, "User: my number is [removed]\nAssistant: ok\n");
        assert!(
            !out.prompt.contains("555-0100"),
            "the typed text must never be re-sent"
        );
        assert_eq!((out.omitted_flagged, out.omitted_unreviewed), (0, 0));
    }

    #[test]
    fn a_user_turn_with_no_reviewed_text_is_left_out_and_counted() {
        let h = vec![base("user", "never reviewed"), user("hi", "hi")];
        let out = build_api_history(&h, &dest());
        assert_eq!(out.prompt, "User: hi\n");
        assert_eq!(out.omitted_unreviewed, 1);
    }

    #[test]
    fn a_flagged_reply_is_kept_for_its_own_provider_only() {
        let mut flagged = reply("it is 555-0100", Some("prov-x"));
        flagged.personal_flagged = true;
        let h = vec![user("hi", "hi"), flagged.clone()];

        let same = build_api_history(&h, &dest());
        assert!(same.prompt.contains("Assistant: it is 555-0100"));
        assert_eq!(same.omitted_flagged, 0);

        let other = build_api_history(
            &h,
            &BoundDestination {
                provider_id: "prov-y".to_owned(),
                model_id: "model-9".to_owned(),
            },
        );
        assert_eq!(other.prompt, "User: hi\n");
        assert_eq!(other.omitted_flagged, 1);
    }

    #[test]
    fn a_flagged_reply_with_no_recorded_producer_is_left_out_for_everyone() {
        let mut flagged = reply("secret-ish", None);
        flagged.personal_flagged = true;
        let out = build_api_history(&[flagged], &dest());
        assert_eq!(out.prompt, "");
        assert_eq!(out.omitted_flagged, 1);
    }

    #[test]
    fn an_unflagged_reply_from_another_provider_is_still_kept() {
        let h = vec![reply("hello", Some("prov-y"))];
        let out = build_api_history(&h, &dest());
        assert_eq!(out.prompt, "Assistant: hello\n");
        assert_eq!(out.omitted_flagged, 0);
    }

    #[test]
    fn partial_error_and_empty_rows_stay_out_without_being_counted() {
        let mut cancelled = reply("half", Some("prov-x"));
        cancelled.reply_status = ReplyStatus::Cancelled;
        let mut cut = reply("half", Some("prov-x"));
        cut.reply_status = ReplyStatus::Cut;
        let mut truncated = reply("long", Some("prov-x"));
        truncated.reply_status = ReplyStatus::Truncated;
        let mut error = reply("a failure", Some("prov-x"));
        error.is_error = true;
        let empty = reply("", Some("prov-x"));
        let h = vec![cancelled, cut, truncated, error, empty, user("hi", "hi")];
        let out = build_api_history(&h, &dest());
        assert_eq!(out.prompt, "User: hi\n");
        assert_eq!((out.omitted_flagged, out.omitted_unreviewed), (0, 0));
    }

    #[test]
    fn it_keeps_only_the_last_ten_messages() {
        let h: Vec<MessageRecord> = (0..15)
            .map(|i| user(&format!("m{i}"), &format!("m{i}")))
            .collect();
        let out = build_api_history(&h, &dest());
        assert_eq!(out.prompt.lines().count(), 10);
        assert!(out.prompt.starts_with("User: m5\n"));
        assert!(out.prompt.ends_with("User: m14\n"));
    }

    #[test]
    fn an_empty_history_is_an_empty_prompt() {
        assert_eq!(build_api_history(&[], &dest()).prompt, "");
    }
}
