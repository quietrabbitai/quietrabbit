// src-tauri/src/commands/api_chat.rs
//
// Group 30 -- API Chat (items.id=716; decisions.id=874, 875, 883, 884, 885).
// Commands: create_api_chat, send_api_chat_message.
//
// An API conversation is its own chat. It is bound at creation to one provider
// and model (persistence::chat_binding_store, immutable), runs under the
// dedicated `api-chat` Focus, and sends only through send_api_chat_message.
// send_message refuses such a conversation and this command refuses a QR chat,
// so external use is always apparent.
//
// What a send does, in order (each stop is a stable code, never prose):
//   1. the conversation must be an API chat, with a readable binding
//   2. the Persona's `api-chat` ceiling must permit external use
//      (the row is created lazily at local_only and only the user raises it,
//      through the existing friction gate)
//   3. the bound provider must still be an active hosted provider that the
//      ceiling permits
//   4. the Privacy Filter must be available (decisions.id=864)
//   5. the NEW user turn is reviewed by the Privacy Guardian (gate3); the
//      user's answers are applied to the text here, on the server, and what
//      was actually sent is stored (api_chat::review)
//   6. the history prefix is built from this conversation's own messages
//      (api_chat::history), then the run starts bound to the destination
//   7. from there it is the existing path, unchanged: consent, price, cap,
//      limits, reservation, Gate 1, Gate 2, Gate 3, streaming
//
// No live provider call is made anywhere in this file's tests.

use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;
use tauri::State;

use crate::api_chat::history::build_api_history;
use crate::api_chat::review::{
    outbound_for_clean, verdict_of, HeldReview, ReviewRegistry, TakeError, Verdict,
};
use crate::api_chat::{BoundDestination, API_CHAT_FOCUS_ID};
use crate::auth::registry::{key_hex, KeyRegistry};
use crate::chat_stream::ChatStreamRegistry;
use crate::commands::execution::{self, SubmitFocusRunRequest};
use crate::commands::messages::{start_reply, MessageInfo};
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::model_selection::resolve_bound_model;
use crate::conductor::privacy::types::Gate3Result;
use crate::conductor::privacy::{privacy_filter, PrivacyGateway};
use crate::conductor::provider_selection::{resolve_bound_provider, ExternalProviderResolution};
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::chat_binding_store::{self, ChatKind};
use crate::persistence::disclosure_log_store::SqliteDisclosureLogger;
use crate::persistence::integration_keys_store::{self, QR_HOSTED_KEY_TYPE};
use crate::persistence::{focus_settings_store, message_store, output_store, persona_store};
use crate::spend::codes::Code;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Type)]
pub struct CreateApiChatRequest {
    pub user_id: String,
    pub persona_id: String,
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Serialize, Type)]
pub struct CreateApiChatResponse {
    /// The conversation's key ("chat-{uuid}"). Its binding is immutable.
    pub context_key: String,
}

#[derive(Debug, Deserialize, Type)]
pub struct SendApiChatRequest {
    pub user_id: String,
    pub persona_id: String,
    pub context_key: String,
    /// The user's turn, exactly as typed.
    pub content: String,
    /// entity_facts.id values the user already confirmed this session
    /// (decisions.id=815); same meaning as in send_message.
    pub confirmed_cross_persona_fact_ids: Vec<String>,
    /// The review key from an earlier `review_required` outcome, sent back
    /// once the user has answered the Privacy Guardian modal (their answers
    /// are recorded under the key with submit_chat_copy_consent_decision).
    /// None starts a new review.
    pub review_key: Option<String>,
}

#[derive(Debug, Serialize, Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SendApiChatOutcome {
    /// The message was sent. `messages` is the transcript with the reserved
    /// reply row; `message-delta` streams the text and `message-content-ready`
    /// is the completion signal. The omitted counts are history messages left
    /// out of what was sent (a flagged reply that must not go to this
    /// provider, a user turn that was never reviewed), so omission is never
    /// silent.
    Sent {
        messages: Vec<MessageInfo>,
        omitted_flagged: u32,
        omitted_unreviewed: u32,
    },
    /// The Privacy Guardian has a question. A `consent_request` event with
    /// this key as its `focus_run_id` was emitted. Nothing was saved or sent.
    /// Record the user's answers under the key, then send again with it.
    ReviewRequired { review_key: String },
}

// ---------------------------------------------------------------------------
// Pure pieces (tested without an AppHandle)
// ---------------------------------------------------------------------------

fn stop(code: Code) -> String {
    code.as_str().to_owned()
}

/// Why a send cannot route to its bound provider, given the Persona's ceiling
/// for the api-chat Focus and what provider resolution said.
fn route_stop(ceiling: ExternalAccess, resolution: &ExternalProviderResolution) -> Option<Code> {
    if ceiling == ExternalAccess::LocalOnly {
        return Some(Code::ApiChatCeilingBlocked);
    }
    match resolution {
        ExternalProviderResolution::Selected(_) => None,
        ExternalProviderResolution::ExcludedByCeiling { .. } => Some(Code::ApiChatCeilingBlocked),
        ExternalProviderResolution::NotConfigured => Some(Code::ApiChatProviderUnavailable),
    }
}

fn new_review_key(persona_id: &str) -> String {
    format!(
        "{}{}-{}-{}",
        output_store::API_SEND_REVIEW_RUN_ID_PREFIX,
        persona_id,
        crate::providers::utils::now(),
        uuid::Uuid::new_v4()
    )
}

/// What a send does after gate3 has spoken about the new turn.
#[derive(Debug, PartialEq, Eq)]
enum ReviewStep {
    /// Send exactly this text.
    Send(String),
    /// Ask the user; this review key goes back to the caller.
    AskUser(String),
    Stop(Code),
}

#[allow(clippy::too_many_arguments)]
fn after_gate3(
    result: &Gate3Result,
    review_key: &str,
    registry: &ReviewRegistry,
    user_id: &str,
    persona_id: &str,
    context_key: &str,
    content: &str,
) -> ReviewStep {
    match verdict_of(result) {
        Verdict::Blocked => ReviewStep::Stop(Code::ApiChatReviewBlocked),
        Verdict::Clean { auto } => match outbound_for_clean(content, auto) {
            Some(text) => ReviewStep::Send(text),
            None => ReviewStep::Stop(Code::ApiChatReviewBlocked),
        },
        Verdict::Pending { spans, auto } => {
            registry.hold(
                review_key,
                HeldReview::new(user_id, persona_id, context_key, content, spans, auto),
            );
            ReviewStep::AskUser(review_key.to_owned())
        }
    }
}

/// The send that comes back with a review key: spend the held review and
/// apply the user's recorded answers.
fn after_answers(
    registry: &ReviewRegistry,
    review_key: &str,
    user_id: &str,
    persona_id: &str,
    context_key: &str,
    content: &str,
    answers: &[output_store::RecordedSpanDecision],
) -> ReviewStep {
    if !review_key.starts_with(output_store::API_SEND_REVIEW_RUN_ID_PREFIX) {
        return ReviewStep::Stop(Code::ApiChatReviewBlocked);
    }
    match registry.take(review_key, user_id, persona_id, context_key, content) {
        Ok(held) => match held.outbound(content, answers) {
            Some(text) => ReviewStep::Send(text),
            None => ReviewStep::Stop(Code::ApiChatReviewBlocked),
        },
        Err(TakeError::NotFound) | Err(TakeError::TextChanged) => {
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        }
    }
}

// ---------------------------------------------------------------------------
// create_api_chat
// ---------------------------------------------------------------------------

/// Validate and bind a new API conversation. Consent, price and limits are NOT
/// checked here: they are enforced at send by the unchanged spend gate, which
/// is the only place they can be enforced for real.
pub(crate) async fn create_api_chat_inner(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
    key_hex_str: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<String, String> {
    // The Persona must belong to this user.
    match persona_store::get_persona_for_user(pool, user_id, persona_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err("persona_not_found".to_owned()),
        Err(e) => return Err(e.to_string()),
    }

    // The provider must be an active hosted provider. The ceiling is not
    // consulted here (the user raises it separately); an unknown provider is
    // refused now rather than at the first send.
    let hosted =
        crate::persistence::provider_store::list_providers_by_type(pool, "cloud_inference_api")
            .await
            .map_err(|e| e.to_string())?;
    if !hosted.iter().any(|p| p.id == provider_id) {
        return Err(stop(Code::ApiChatProviderUnavailable));
    }

    // The model must be runnable (curated, or the user's own live list with
    // an output limit). A retired or unknown one is its code, never a swap.
    resolve_bound_model(pool, user_id, key_hex_str, provider_id, model_id)
        .await
        .map_err(|e| e.block().0.as_str().to_owned())?;

    // A saved key for the provider.
    let key = integration_keys_store::get_active_key(
        user_id,
        key_hex_str,
        provider_id,
        QR_HOSTED_KEY_TYPE,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    if key.is_none() {
        return Err("api_chat.key_missing".to_owned());
    }

    // The Persona's api-chat settings row, local_only until the user raises it.
    focus_settings_store::ensure_focus_settings_default(
        pool,
        persona_id,
        API_CHAT_FOCUS_ID,
        ExternalAccess::LocalOnly,
    )
    .await
    .map_err(|e| e.to_string())?;

    let context_key = format!("chat-{}", uuid::Uuid::new_v4());
    chat_binding_store::create_binding(
        user_id,
        persona_id,
        key_hex_str,
        &context_key,
        ChatKind::Api,
        provider_id,
        model_id,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(context_key)
}

#[tauri::command]
#[specta::specta]
pub async fn create_api_chat(
    pool: State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    request: CreateApiChatRequest,
) -> Result<CreateApiChatResponse, String> {
    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    let context_key = create_api_chat_inner(
        &pool,
        &request.user_id,
        &request.persona_id,
        &key_hex_str,
        &request.provider_id,
        &request.model_id,
    )
    .await?;
    Ok(CreateApiChatResponse { context_key })
}

// ---------------------------------------------------------------------------
// send_api_chat_message
// ---------------------------------------------------------------------------

/// What a send is cleared to do: where it goes and the Persona's real ceiling
/// for the api-chat Focus. The ceiling is handed on to the outbound review so
/// the audit record carries the Persona's actual setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedSend {
    pub destination: BoundDestination,
    pub ceiling: ExternalAccess,
}

/// Everything a send needs before any text is touched: the binding, the
/// ceiling, and the proof the bound provider still routes. Each failure is a
/// stable code.
pub(crate) async fn prepare_send(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    persona_id: &str,
    key_hex_str: &str,
    context_key: &str,
) -> Result<PreparedSend, String> {
    let binding = chat_binding_store::get_binding(user_id, persona_id, key_hex_str, context_key)
        .await
        .map_err(|_| stop(Code::ApiChatWrongChatKind))?
        .filter(|b| b.kind == ChatKind::Api)
        .ok_or_else(|| stop(Code::ApiChatWrongChatKind))?;
    let target = binding
        .primary()
        .ok_or_else(|| stop(Code::ApiChatBindingMissing))?;
    let destination = BoundDestination {
        provider_id: target.provider_id.clone(),
        model_id: target.model_id.clone(),
    };

    // The row is guaranteed on every send (the lesson of items.id=318): a
    // Persona with no row would otherwise hit a hard AUTHORIZE failure. It is
    // created at local_only and never changed here.
    focus_settings_store::ensure_focus_settings_default(
        pool,
        persona_id,
        API_CHAT_FOCUS_ID,
        ExternalAccess::LocalOnly,
    )
    .await
    .map_err(|e| e.to_string())?;
    let settings = focus_settings_store::get_focus_settings(pool, persona_id, API_CHAT_FOCUS_ID)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| stop(Code::ApiChatCeilingBlocked))?;
    // The Focus's own ceiling is Unrestricted (max_routing_tier 3), so the
    // Persona's setting is the effective ceiling.
    let ceiling = settings.max_permitted_tier;

    let resolution = resolve_bound_provider(pool, &destination.provider_id, ceiling).await;
    if let Some(code) = route_stop(ceiling, &resolution) {
        return Err(stop(code));
    }
    Ok(PreparedSend {
        destination,
        ceiling,
    })
}

/// The Privacy Guardian review of the new turn: one gate3 call, the same
/// parameters as the copy review except the destination (external, so gate3's
/// own ceiling check also holds) and the ceiling, which is the Persona's real
/// one. gate3 echoes the ceiling into its result and the audit record, so a
/// placeholder here would put a wrong setting on the record.
#[allow(clippy::too_many_arguments)]
async fn review_new_turn<L: crate::conductor::privacy::logger::DisclosureLogger>(
    gateway: &PrivacyGateway<L>,
    app_handle: Option<&tauri::AppHandle>,
    review_key: &str,
    content: &str,
    ceiling: ExternalAccess,
    user_id: &str,
    persona_id: &str,
    key_hex_str: &str,
) -> Result<Gate3Result, crate::conductor::privacy::errors::DisclosureLogWriteError> {
    gateway
        .gate3(
            "api-chat-send",
            review_key,
            "API Chat",
            review_key,
            content,
            // Placeholder severity: there is no PersonalTrack here, so the copy
            // review's convention applies and only content the Privacy Filter
            // actually flags reaches the modal.
            1,
            // External target, so gate3's own ceiling check also holds.
            3,
            ceiling,
            3,
            app_handle,
            // Strict constant, as the copy review: no providers.risk_rating or
            // model-override lookup (items.id=729 is separate and open).
            Some(3),
            false,
            user_id,
            persona_id,
            key_hex_str,
        )
        .await
}

#[tauri::command]
#[specta::specta]
pub async fn send_api_chat_message(
    app_handle: tauri::AppHandle,
    scheduler: tauri::State<'_, Arc<ConductorScheduler>>,
    pool: tauri::State<'_, sqlx::SqlitePool>,
    key_registry: State<'_, KeyRegistry>,
    chat_registry: State<'_, ChatStreamRegistry>,
    review_registry: State<'_, ReviewRegistry>,
    request: SendApiChatRequest,
) -> Result<SendApiChatOutcome, String> {
    let SendApiChatRequest {
        user_id,
        persona_id,
        context_key,
        content,
        confirmed_cross_persona_fact_ids,
        review_key,
    } = request;

    let key_hex_str = key_registry
        .with_key(|k| key_hex(&k.data_key))
        .await
        .ok_or_else(|| "not logged in".to_owned())?;

    // 1-3. Binding, ceiling, provider.
    let PreparedSend {
        destination,
        ceiling,
    } = prepare_send(&pool, &user_id, &persona_id, &key_hex_str, &context_key).await?;

    // 4. No Privacy Filter, no outbound review, so no send (decisions.id=864).
    // Without it gate3 would approve everything below severity 3 without
    // scanning, which is not a review.
    if !privacy_filter::is_available() {
        return Err(stop(Code::ApiChatPrivacyFilterUnavailable));
    }

    // 5. The outbound review of the new turn.
    let step = match review_key.as_deref() {
        Some(key) => {
            let answers = output_store::list_element_decisions_for_run(
                &user_id,
                &persona_id,
                &key_hex_str,
                key,
            )
            .await
            .map_err(|e| e.to_string())?;
            after_answers(
                &review_registry,
                key,
                &user_id,
                &persona_id,
                &context_key,
                &content,
                &answers,
            )
        }
        None => {
            let key = new_review_key(&persona_id);
            let gateway = PrivacyGateway::new(SqliteDisclosureLogger::new(
                &user_id,
                &persona_id,
                &key_hex_str,
            ));
            let result = review_new_turn(
                &gateway,
                Some(&app_handle),
                &key,
                &content,
                ceiling,
                &user_id,
                &persona_id,
                &key_hex_str,
            )
            .await;
            match result {
                // An audit-log failure is fatal to the review (fail closed).
                Err(_) => ReviewStep::Stop(Code::ApiChatReviewBlocked),
                Ok(r) => after_gate3(
                    &r,
                    &key,
                    &review_registry,
                    &user_id,
                    &persona_id,
                    &context_key,
                    &content,
                ),
            }
        }
    };
    let outbound = match step {
        ReviewStep::Send(text) => text,
        ReviewStep::AskUser(review_key) => {
            return Ok(SendApiChatOutcome::ReviewRequired { review_key })
        }
        ReviewStep::Stop(code) => return Err(stop(code)),
    };

    // R1 crisis-handling floor (decisions.id=607): on the raw fresh turn, as
    // send_message does.
    let crisis_detected = crate::conductor::crisis::detect(&content);

    // 6. Save the turn (as typed, with what was sent beside it), then build the
    // history from this conversation's own messages.
    message_store::save_message_with_provenance(
        &user_id,
        &persona_id,
        &key_hex_str,
        &context_key,
        "user",
        &content,
        None,
        &message_store::MessageProvenance {
            outbound_content: Some(&outbound),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let history = message_store::list_messages(&user_id, &persona_id, &key_hex_str, &context_key)
        .await
        .map_err(|e| e.to_string())?;
    let api_history = build_api_history(&history, &destination);

    let run_request = SubmitFocusRunRequest {
        focus_id: API_CHAT_FOCUS_ID.to_owned(),
        user_input: api_history.prompt.clone(),
        user_id: user_id.clone(),
        persona_id: persona_id.clone(),
        topic_id: None,
        confirmed_cross_persona_fact_ids,
    };
    let mut run = execution::load_and_authorize_run(
        app_handle,
        scheduler,
        pool,
        key_hex_str.clone(),
        crisis_detected,
        Some(destination.clone()),
        run_request,
    )
    .await?;
    let run_id = run
        .focus_run_id
        .clone()
        .ok_or_else(|| "run_id not set after authorize".to_string())?;
    // Streamed, with a cancel token the caller reaches through
    // cancel_chat_stream (items.id=715).
    run.chat_stream = Some(chat_registry.register(&user_id, &run_id));

    let messages = start_reply(
        run,
        &user_id,
        &persona_id,
        &key_hex_str,
        &context_key,
        &message_store::MessageProvenance {
            provider_id: Some(&destination.provider_id),
            model_id: Some(&destination.model_id),
            ..Default::default()
        },
    )
    .await?;

    Ok(SendApiChatOutcome::Sent {
        messages,
        omitted_flagged: api_history.omitted_flagged as u32,
        omitted_unreviewed: api_history.omitted_unreviewed as u32,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// The two commands take Tauri State (and send_api_chat_message an AppHandle),
// which has no construction path in a plain #[tokio::test] anywhere in this
// codebase (see commands/consent.rs for the same note). Everything they decide
// is in the functions above and in api_chat::*, and is tested here and there
// against real shared.db and messages.db files. Not tested: the gate3 call
// itself with a live Privacy Filter and the real send, which need an
// AppHandle and a provider (items.id=726).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_chat::redact::{Decision, SpanDecision};
    use crate::api_chat::review::SpanRef;
    use crate::conductor::privacy::types::AutoResolvedSpan;
    use crate::persistence::provider_models_store::{
        record_refresh_conn, set_user_output_limit_conn,
    };
    use crate::providers::hosted_client::LiveModel;
    use crate::test_support::ENV_MUTEX;

    const USER: &str = "api-user";
    const PERSONA: &str = "api-persona";
    const OTHER_PERSONA: &str = "api-persona-2";
    const KEY: &str = "0505050505050505050505050505050505050505050505050505050505050505";

    struct Rig {
        pool: sqlx::SqlitePool,
        provider: String,
        model: String,
        _dir: tempfile::TempDir,
    }

    async fn with_rig<F, Fut>(f: F)
    where
        F: FnOnce(Rig) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = ENV_MUTEX.lock().await;
        let saved = std::env::var("QR_DATA_ROOT").ok();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", dir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("migrate shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        sqlx::query(
            "INSERT INTO users (id, display_name, role, is_primary, auth_enabled, created_at) \
             VALUES (?, 'Fixture', 'user', 1, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(USER)
        .execute(&pool)
        .await
        .expect("user");
        for persona in [PERSONA, OTHER_PERSONA] {
            persona_store::create_persona(&pool, persona, "Fixture", "personal", USER, None)
                .await
                .expect("persona");
        }
        let (provider, model): (String, String) = sqlx::query_as(
            "SELECT m.provider_id, m.model_id FROM provider_models m \
             JOIN providers p ON p.id = m.provider_id \
             WHERE m.is_default = 1 AND p.provider_type = 'cloud_inference_api' \
             ORDER BY m.provider_id LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .expect("a seeded hosted provider with a default model");
        f(Rig {
            pool,
            provider,
            model,
            _dir: dir,
        })
        .await;
        match saved {
            Some(v) => std::env::set_var("QR_DATA_ROOT", v),
            None => std::env::remove_var("QR_DATA_ROOT"),
        }
    }

    impl Rig {
        async fn save_key(&self) {
            integration_keys_store::save_credential(
                USER,
                KEY,
                &integration_keys_store::NewCredential {
                    provider: &self.provider,
                    key_type: QR_HOSTED_KEY_TYPE,
                    credential: "sk-fixture-not-a-real-key",
                    persona_id: None,
                    auth_type: Some("api_key"),
                    expires_at: None,
                    last_verified_at: None,
                    extra_metadata: None,
                },
            )
            .await
            .expect("save key");
        }

        async fn create(&self) -> Result<String, String> {
            create_api_chat_inner(&self.pool, USER, PERSONA, KEY, &self.provider, &self.model).await
        }

        async fn ceiling(&self, persona: &str) -> ExternalAccess {
            focus_settings_store::get_focus_settings(&self.pool, persona, API_CHAT_FOCUS_ID)
                .await
                .unwrap()
                .unwrap()
                .max_permitted_tier
        }
    }

    // -- create ---------------------------------------------------------------

    #[tokio::test]
    async fn creating_binds_the_provider_and_model_and_makes_a_local_only_settings_row() {
        with_rig(|rig| async move {
            rig.save_key().await;
            assert!(focus_settings_store::get_focus_settings(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID
            )
            .await
            .unwrap()
            .is_none());

            let key = rig.create().await.expect("create");
            assert!(key.starts_with("chat-"));

            let b = chat_binding_store::get_binding(USER, PERSONA, KEY, &key)
                .await
                .unwrap()
                .expect("bound");
            assert_eq!(b.kind, ChatKind::Api);
            let t = b.primary().unwrap();
            assert_eq!(
                (t.provider_id.as_str(), t.model_id.as_str()),
                (rig.provider.as_str(), rig.model.as_str())
            );

            // The row exists at the most restrictive ceiling, and the other
            // Persona has none (created lazily, only where API Chat is used).
            assert_eq!(rig.ceiling(PERSONA).await, ExternalAccess::LocalOnly);
            assert!(focus_settings_store::get_focus_settings(
                &rig.pool,
                OTHER_PERSONA,
                API_CHAT_FOCUS_ID
            )
            .await
            .unwrap()
            .is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn creating_refuses_an_unknown_provider_model_persona_or_missing_key() {
        with_rig(|rig| async move {
            rig.save_key().await;
            let bad_provider =
                create_api_chat_inner(&rig.pool, USER, PERSONA, KEY, "no-such", &rig.model).await;
            assert_eq!(bad_provider.unwrap_err(), "api_chat.provider_unavailable");
            let bad_model =
                create_api_chat_inner(&rig.pool, USER, PERSONA, KEY, &rig.provider, "ghost").await;
            assert_eq!(bad_model.unwrap_err(), "spend_block.model_unavailable");
            let bad_persona =
                create_api_chat_inner(&rig.pool, USER, "nobody", KEY, &rig.provider, &rig.model)
                    .await;
            assert_eq!(bad_persona.unwrap_err(), "persona_not_found");
            // Nothing was created by the refusals.
            assert!(focus_settings_store::get_focus_settings(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID
            )
            .await
            .unwrap()
            .is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn creating_without_a_saved_key_is_refused() {
        with_rig(|rig| async move {
            assert_eq!(rig.create().await.unwrap_err(), "api_chat.key_missing");
        })
        .await;
    }

    #[tokio::test]
    async fn creating_with_an_uncurated_model_needs_the_users_output_limit() {
        with_rig(|rig| async move {
            rig.save_key().await;
            let mut conn = crate::persistence::provider_spend_store::open(USER, KEY)
                .await
                .unwrap();
            record_refresh_conn(
                &mut conn,
                &rig.provider,
                &[LiveModel {
                    id: "live-x".to_owned(),
                    context_tokens: Some(8000),
                    retires_at: None,
                }],
                chrono::Utc::now(),
            )
            .await
            .unwrap();
            let without =
                create_api_chat_inner(&rig.pool, USER, PERSONA, KEY, &rig.provider, "live-x").await;
            assert_eq!(
                without.unwrap_err(),
                "spend_block.model_output_limit_required"
            );
            set_user_output_limit_conn(&mut conn, &rig.provider, "live-x", Some(200))
                .await
                .unwrap();
            assert!(
                create_api_chat_inner(&rig.pool, USER, PERSONA, KEY, &rig.provider, "live-x")
                    .await
                    .is_ok()
            );
        })
        .await;
    }

    // -- prepare_send: the ceiling and the kinds ------------------------------

    #[tokio::test]
    async fn a_default_row_is_still_blocked_from_a_hosted_send_until_the_user_raises_it() {
        with_rig(|rig| async move {
            rig.save_key().await;
            let key = rig.create().await.unwrap();

            let blocked = prepare_send(&rig.pool, USER, PERSONA, KEY, &key).await;
            assert_eq!(blocked.unwrap_err(), "api_chat.ceiling_blocked");
            assert_eq!(
                rig.ceiling(PERSONA).await,
                ExternalAccess::LocalOnly,
                "a send never raises the ceiling"
            );

            // The user raises it (the friction gate is the command layer's
            // job, commands::persona::update_focus_settings; this is its store).
            focus_settings_store::update_focus_settings(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID,
                None,
                None,
                None,
                Some(ExternalAccess::Unrestricted),
                None,
                None,
            )
            .await
            .expect("raise");
            let ok = prepare_send(&rig.pool, USER, PERSONA, KEY, &key)
                .await
                .unwrap();
            assert_eq!(ok.destination.provider_id, rig.provider);
            assert_eq!(ok.destination.model_id, rig.model);
            assert_eq!(
                ok.ceiling,
                ExternalAccess::Unrestricted,
                "the Persona's real ceiling comes back, for the review"
            );

            // A raised value survives every later ensure.
            focus_settings_store::ensure_focus_settings_default(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID,
                ExternalAccess::LocalOnly,
            )
            .await
            .unwrap();
            assert_eq!(rig.ceiling(PERSONA).await, ExternalAccess::Unrestricted);
        })
        .await;
    }

    /// The ceiling the send hands to the review is the Persona's real one,
    /// whatever it is: not a constant.
    #[tokio::test]
    async fn prepare_send_returns_the_personas_real_ceiling() {
        with_rig(|rig| async move {
            rig.save_key().await;
            let key = rig.create().await.unwrap();
            focus_settings_store::update_focus_settings(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID,
                None,
                None,
                None,
                Some(ExternalAccess::AnonymousPreferred),
                None,
                None,
            )
            .await
            .unwrap();
            let prepared = prepare_send(&rig.pool, USER, PERSONA, KEY, &key)
                .await
                .unwrap();
            assert_eq!(prepared.ceiling, ExternalAccess::AnonymousPreferred);
            assert_ne!(
                prepared.ceiling,
                ExternalAccess::Unrestricted,
                "the review must not be given a stronger setting than the Persona has"
            );
        })
        .await;
    }

    /// The review gives gate3 the ceiling it was handed. gate3 echoes it into
    /// its result (and so the audit record) on its ceiling check, so a local
    /// only ceiling shows up there as itself.
    #[tokio::test]
    async fn the_review_carries_the_ceiling_it_was_given_into_gate3() {
        use crate::conductor::privacy::logger::TestLogger;
        let gateway = PrivacyGateway::new(TestLogger::new());
        let blocked = review_new_turn(
            &gateway,
            None,
            "api-chat-send-k",
            "hello there",
            ExternalAccess::LocalOnly,
            USER,
            PERSONA,
            "",
        )
        .await
        .unwrap();
        assert!(
            blocked.blocked,
            "an external target at a local-only ceiling is blocked"
        );
        assert_eq!(
            blocked.space_max_permitted_tier,
            Some(ExternalAccess::LocalOnly),
            "gate3 saw the ceiling the review was handed"
        );
        assert_eq!(blocked.target_tier, Some(3));

        // At an external ceiling it is not stopped by the ceiling check.
        let ok = review_new_turn(
            &gateway,
            None,
            "api-chat-send-k2",
            "hello there",
            ExternalAccess::AnonymousPreferred,
            USER,
            PERSONA,
            "",
        )
        .await
        .unwrap();
        assert!(!ok.blocked);
        assert_eq!(ok.space_max_permitted_tier, None);
    }

    #[tokio::test]
    async fn an_anonymous_only_ceiling_excludes_a_non_anonymous_hosted_provider() {
        with_rig(|rig| async move {
            rig.save_key().await;
            let key = rig.create().await.unwrap();
            focus_settings_store::update_focus_settings(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID,
                None,
                None,
                None,
                Some(ExternalAccess::AnonymousRequired),
                None,
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                prepare_send(&rig.pool, USER, PERSONA, KEY, &key)
                    .await
                    .unwrap_err(),
                "api_chat.ceiling_blocked"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn only_an_api_chat_can_use_the_api_send_and_the_row_is_made_on_demand() {
        with_rig(|rig| async move {
            // A QR chat (no binding row) is refused.
            assert_eq!(
                prepare_send(&rig.pool, USER, PERSONA, KEY, "chat-qr")
                    .await
                    .unwrap_err(),
                "api_chat.wrong_chat_kind"
            );
            // A bound chat on a Persona whose settings row is missing gets it
            // made, at local_only, not an AUTHORIZE failure.
            chat_binding_store::create_binding(
                USER,
                PERSONA,
                KEY,
                "chat-late",
                ChatKind::Api,
                &rig.provider,
                &rig.model,
            )
            .await
            .unwrap();
            assert_eq!(
                prepare_send(&rig.pool, USER, PERSONA, KEY, "chat-late")
                    .await
                    .unwrap_err(),
                "api_chat.ceiling_blocked"
            );
            assert_eq!(rig.ceiling(PERSONA).await, ExternalAccess::LocalOnly);
        })
        .await;
    }

    #[tokio::test]
    async fn a_bound_provider_that_is_gone_is_unavailable_never_replaced() {
        with_rig(|rig| async move {
            chat_binding_store::create_binding(
                USER,
                PERSONA,
                KEY,
                "chat-gone",
                ChatKind::Api,
                "removed-provider",
                "m",
            )
            .await
            .unwrap();
            focus_settings_store::ensure_focus_settings_default(
                &rig.pool,
                PERSONA,
                API_CHAT_FOCUS_ID,
                ExternalAccess::Unrestricted,
            )
            .await
            .unwrap();
            assert_eq!(
                prepare_send(&rig.pool, USER, PERSONA, KEY, "chat-gone")
                    .await
                    .unwrap_err(),
                "api_chat.provider_unavailable"
            );
        })
        .await;
    }

    #[test]
    fn routing_stops_are_decided_by_the_ceiling_first() {
        use ExternalProviderResolution as R;
        let sel = R::Selected("p".into());
        assert_eq!(
            route_stop(ExternalAccess::LocalOnly, &sel),
            Some(Code::ApiChatCeilingBlocked)
        );
        assert_eq!(route_stop(ExternalAccess::Unrestricted, &sel), None);
        assert_eq!(
            route_stop(
                ExternalAccess::AnonymousRequired,
                &R::ExcludedByCeiling {
                    provider_name: "P".into()
                }
            ),
            Some(Code::ApiChatCeilingBlocked)
        );
        assert_eq!(
            route_stop(ExternalAccess::Unrestricted, &R::NotConfigured),
            Some(Code::ApiChatProviderUnavailable)
        );
    }

    // -- the outbound review -------------------------------------------------

    fn pending_result() -> Gate3Result {
        Gate3Result {
            pending_consent: true,
            review_spans: vec![crate::conductor::privacy::types::ConsentSpanItem {
                span_id: "s1".into(),
                category: "private_person".into(),
                user_label: "Person name".into(),
                original_text: "Ann".into(),
                suggestion: None,
                start_byte: 5,
                end_byte: 8,
                score: 0.99,
                review_tier: crate::conductor::privacy::types::ReviewTier::High,
                fact_key: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_pending_review_is_held_and_the_answers_decide_what_leaves() {
        let reg = ReviewRegistry::default();
        let step = after_gate3(
            &pending_result(),
            "api-chat-send-k1",
            &reg,
            USER,
            PERSONA,
            "chat-1",
            "call Ann now",
        );
        assert_eq!(step, ReviewStep::AskUser("api-chat-send-k1".into()));
        assert_eq!(reg.held_count(), 1);

        let answers = vec![output_store::RecordedSpanDecision {
            span_id: "s1".into(),
            decision: "keep_private".into(),
            suggestion_text: None,
            user_modified_text: None,
        }];
        let sent = after_answers(
            &reg,
            "api-chat-send-k1",
            USER,
            PERSONA,
            "chat-1",
            "call Ann now",
            &answers,
        );
        assert_eq!(sent, ReviewStep::Send("call [removed] now".into()));

        // Single use: the same key again is refused.
        let again = after_answers(
            &reg,
            "api-chat-send-k1",
            USER,
            PERSONA,
            "chat-1",
            "call Ann now",
            &answers,
        );
        assert_eq!(again, ReviewStep::Stop(Code::ApiChatReviewBlocked));
    }

    #[test]
    fn answers_with_edited_text_a_foreign_key_or_the_wrong_prefix_send_nothing() {
        let reg = ReviewRegistry::default();
        after_gate3(
            &pending_result(),
            "api-chat-send-k2",
            &reg,
            USER,
            PERSONA,
            "chat-1",
            "call Ann now",
        );
        // The user edited the draft between the question and the answer.
        assert_eq!(
            after_answers(
                &reg,
                "api-chat-send-k2",
                USER,
                PERSONA,
                "chat-1",
                "call Bob now",
                &[]
            ),
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        );
        // Not a key this feature issued.
        assert_eq!(
            after_answers(
                &reg,
                "clipboard-copy-chat-x",
                USER,
                PERSONA,
                "chat-1",
                "call Ann now",
                &[]
            ),
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        );
        // Another user.
        after_gate3(
            &pending_result(),
            "api-chat-send-k3",
            &reg,
            USER,
            PERSONA,
            "chat-1",
            "call Ann now",
        );
        assert_eq!(
            after_answers(
                &reg,
                "api-chat-send-k3",
                "intruder",
                PERSONA,
                "chat-1",
                "call Ann now",
                &[]
            ),
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        );
    }

    #[test]
    fn a_blocked_or_unclear_review_stops_and_a_clean_one_applies_standing_decisions() {
        let reg = ReviewRegistry::default();
        let blocked = Gate3Result {
            blocked: true,
            ..Default::default()
        };
        assert_eq!(
            after_gate3(&blocked, "k", &reg, USER, PERSONA, "c", "text"),
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        );
        assert_eq!(
            after_gate3(
                &Gate3Result::default(),
                "k",
                &reg,
                USER,
                PERSONA,
                "c",
                "text"
            ),
            ReviewStep::Stop(Code::ApiChatReviewBlocked),
            "neither approved nor pending is not a green light"
        );
        let clean = Gate3Result {
            approved: true,
            ..Default::default()
        };
        assert_eq!(
            after_gate3(&clean, "k", &reg, USER, PERSONA, "c", "plain text"),
            ReviewStep::Send("plain text".into())
        );
        let with_standing = Gate3Result {
            approved: true,
            auto_resolved: vec![AutoResolvedSpan {
                start_byte: 4,
                end_byte: 7,
                decision: "keep_private".into(),
                suggestion_text: None,
                user_modified_text: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            after_gate3(&with_standing, "k", &reg, USER, PERSONA, "c", "ask Ann"),
            ReviewStep::Send("ask [removed]".into())
        );
        // A standing decision that cannot be applied safely stops the send.
        let unappliable = Gate3Result {
            approved: true,
            auto_resolved: vec![AutoResolvedSpan {
                start_byte: 4,
                end_byte: 400,
                decision: "keep_private".into(),
                suggestion_text: None,
                user_modified_text: None,
            }],
            ..Default::default()
        };
        assert_eq!(
            after_gate3(&unappliable, "k", &reg, USER, PERSONA, "c", "ask Ann"),
            ReviewStep::Stop(Code::ApiChatReviewBlocked)
        );
        assert_eq!(reg.held_count(), 0, "only a pending review is held");
    }

    #[test]
    fn a_review_key_has_the_api_send_prefix_and_is_unique() {
        let a = new_review_key("p1");
        let b = new_review_key("p1");
        assert!(a.starts_with(output_store::API_SEND_REVIEW_RUN_ID_PREFIX));
        assert_ne!(a, b);
    }

    // Keep the redaction types referenced so a refactor of either breaks here.
    #[test]
    fn redaction_types_line_up_with_the_review_types() {
        let _ = (
            SpanRef {
                span_id: "x".into(),
                start_byte: 0,
                end_byte: 0,
            },
            SpanDecision::unresolved(0, 0),
            Decision::KeepPrivate,
        );
    }
}
