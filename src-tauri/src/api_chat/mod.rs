// src-tauri/src/api_chat/mod.rs
//
// items.id=716 (decisions.id=874, 875): API Chat, the backend. An API
// conversation is its own chat, bound at creation to one provider and model
// (persistence::chat_binding_store), run under the dedicated `api-chat` Focus.
//
// This module holds what is neither a Tauri command (commands::api_chat) nor
// storage:
//   - the destination a run is bound to and the guards on it (here);
//   - the history prefix builder and its flagged-reply rule (history.rs);
//   - applying the user's Privacy Guardian decisions to the outbound text
//     (redact.rs);
//   - the held state of a pending outbound review (review.rs).
//
// Everything a bound run does after this is the existing path, unchanged:
// consent, price, cap, limits, reservation (spend::gate), Gate 1, Gate 2,
// Gate 3, the disclosure log.

pub mod history;
pub mod redact;
pub mod review;

use crate::conductor::tokens::ExternalAccess;
use crate::spend::codes::Code;

/// The Focus every API conversation runs. Created lazily per Persona at the
/// first bind (not seeded, items.id=687) and never `is_quick_ask`.
pub const API_CHAT_FOCUS_ID: &str = "api-chat";

/// The provider and model a run is bound to. Carried on `FocusRun` the way
/// `chat_stream` is: None by default, so every run that is not an API Chat
/// behaves exactly as before. Bound by ids so a local provider row could join
/// later without a rewrite (it would run through `OllamaClient`, not
/// `HostedClient`; see the item's plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundDestination {
    pub provider_id: String,
    pub model_id: String,
}

/// The destination a run of the api-chat Focus is bound to, found through its
/// assistant message's conversation. Used when a run is rehydrated (resumed or
/// swept), where the in-memory binding is gone. Any failure to read it is a
/// stop (`ApiChatBindingMissing`): a resumed API Chat run never falls back to a
/// preference-resolved provider.
pub async fn destination_for_run(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    run_id: &str,
) -> Result<BoundDestination, Code> {
    use crate::persistence::chat_binding_store::{get_binding, ChatKind};
    use crate::persistence::message_store::find_assistant_message_by_focus_run_id;

    let message = find_assistant_message_by_focus_run_id(user_id, persona_id, key_hex, run_id)
        .await
        .ok()
        .flatten()
        .ok_or(Code::ApiChatBindingMissing)?;
    let binding = get_binding(user_id, persona_id, key_hex, &message.context_key)
        .await
        .ok()
        .flatten()
        .filter(|b| b.kind == ChatKind::Api)
        .ok_or(Code::ApiChatBindingMissing)?;
    let target = binding.primary().ok_or(Code::ApiChatBindingMissing)?;
    Ok(BoundDestination {
        provider_id: target.provider_id.clone(),
        model_id: target.model_id.clone(),
    })
}

/// Whether a bound run may proceed to the hosted call, as the code that stops
/// it when it may not. Pure, so the fail-closed rules are tested without a
/// run. `resolved_provider` is what provider resolution returned for the run.
///
/// 1. A bound run whose effective ceiling is LocalOnly stops. The executor's
///    LocalOnly branch calls the local model; an "API" conversation answered
///    by the local model, labelled external, would be a lie about where the
///    text went. It never reaches that branch.
/// 2. A bound run whose provider did not resolve to exactly the bound provider
///    stops. No other provider is substituted.
pub fn bound_run_stop(
    bound: &BoundDestination,
    effective_access: ExternalAccess,
    resolved_provider: Option<&str>,
) -> Option<Code> {
    if effective_access == ExternalAccess::LocalOnly {
        return Some(Code::ApiChatCeilingBlocked);
    }
    match resolved_provider {
        Some(p) if p == bound.provider_id => None,
        _ => Some(Code::ApiChatProviderUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound() -> BoundDestination {
        BoundDestination {
            provider_id: "prov-x".to_owned(),
            model_id: "model-1".to_owned(),
        }
    }

    #[test]
    fn a_bound_run_at_a_local_only_ceiling_stops_and_never_runs_locally() {
        assert_eq!(
            bound_run_stop(&bound(), ExternalAccess::LocalOnly, Some("prov-x")),
            Some(Code::ApiChatCeilingBlocked)
        );
        assert_eq!(
            bound_run_stop(&bound(), ExternalAccess::LocalOnly, None),
            Some(Code::ApiChatCeilingBlocked)
        );
    }

    #[test]
    fn a_bound_run_proceeds_only_for_exactly_the_bound_provider() {
        for access in [
            ExternalAccess::AnonymousRequired,
            ExternalAccess::AnonymousPreferred,
            ExternalAccess::Unrestricted,
        ] {
            assert_eq!(bound_run_stop(&bound(), access, Some("prov-x")), None);
            assert_eq!(
                bound_run_stop(&bound(), access, Some("prov-y")),
                Some(Code::ApiChatProviderUnavailable)
            );
            assert_eq!(
                bound_run_stop(&bound(), access, None),
                Some(Code::ApiChatProviderUnavailable)
            );
        }
    }
}
