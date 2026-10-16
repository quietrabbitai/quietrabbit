// src-tauri/src/providers/hosted_stream_tests.rs
//
// Tests for streamed generation (items.id=715). Everything talks to a local
// 127.0.0.1 server; nothing leaves the machine and no real provider is called.
// No provider or model name appears here (decisions.id=872): rows are neutral
// fixtures. The wire shapes in the fixtures are the documented ones for each
// dialect (Anthropic's read from its streaming reference; the OpenAI-style and
// OpenRouter shapes from their documented behaviour, UNVERIFIED live:
// items.id=726).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::persistence::provider_store::ModelStatus;
use crate::providers::hosted_call::HostedRoute;
use crate::providers::loopback_test_server::{Canned, LoopbackServer, StreamStep};

const SECRET: &str = "sk-loopback-secret-0123456789abcdef";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn chat_route(base: &str) -> HostedRoute {
    HostedRoute {
        provider_id: "fixture-host".to_owned(),
        display_name: "Fixture Host".to_owned(),
        api_dialect: Some("openai_chat".to_owned()),
        base_url_template: Some(format!("{base}/v1")),
        auth_scheme: Some("bearer".to_owned()),
        extra_headers: Some(json!({})),
        models_path: Some("/models".to_owned()),
        user_variables: Some(json!([])),
        model_id: "fixture-model".to_owned(),
        model_status: ModelStatus::Active,
        request_options: Some(json!({"max_completion_tokens": 512})),
        required_request_options: None,
    }
}

fn messages_route(base: &str) -> HostedRoute {
    HostedRoute {
        api_dialect: Some("anthropic_messages".to_owned()),
        base_url_template: Some(base.to_owned()),
        auth_scheme: Some("x-api-key".to_owned()),
        models_path: Some("/v1/models".to_owned()),
        request_options: Some(json!({"max_tokens": 256})),
        ..chat_route(base)
    }
}

fn ctx_for(route: &HostedRoute) -> CallContext {
    CallContext::build_for_loopback(route, SECRET, &json!({})).expect("fixture route must build")
}

fn client() -> HostedClient {
    HostedClient::for_loopback_tests(Duration::from_secs(5), Duration::from_secs(2), 1024 * 1024)
}

fn limits() -> StreamLimits {
    StreamLimits {
        headers_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        total_timeout: Duration::from_secs(20),
        max_event_bytes: MAX_EVENT_BYTES,
    }
}

fn request() -> HostedRequest {
    HostedRequest {
        system: None,
        prompt: "hello".to_owned(),
        temperature: Some(0.5),
        max_output_tokens: None,
    }
}

#[derive(Default)]
struct Rec(Mutex<Vec<(u32, String)>>);

impl StreamSink for Rec {
    fn delta(&self, seq: u32, text: &str) {
        self.0.lock().unwrap().push((seq, text.to_owned()));
    }
}

impl Rec {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.as_str())
            .collect()
    }
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn seqs(&self) -> Vec<u32> {
        self.0.lock().unwrap().iter().map(|(s, _)| *s).collect()
    }
}

fn data(v: Value) -> Vec<u8> {
    format!("data: {v}\n\n").into_bytes()
}
fn chat_delta(t: &str) -> Vec<u8> {
    data(json!({"choices": [{"index": 0, "delta": {"content": t}, "finish_reason": null}]}))
}
fn chat_finish(reason: &str) -> Vec<u8> {
    data(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}))
}
fn chat_usage() -> Vec<u8> {
    data(json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 7}}))
}
fn done() -> Vec<u8> {
    b"data: [DONE]\n\n".to_vec()
}
fn named(name: &str, v: Value) -> Vec<u8> {
    format!("event: {name}\ndata: {v}\n\n").into_bytes()
}
fn chunks(frames: Vec<Vec<u8>>) -> Vec<StreamStep> {
    frames.into_iter().map(StreamStep::Chunk).collect()
}
fn chat_stream_ok() -> Canned {
    Canned::sse(chunks(vec![
        chat_delta("Hel"),
        chat_delta("lo"),
        chat_finish("stop"),
        chat_usage(),
        done(),
    ]))
}

fn m_start() -> Vec<u8> {
    named(
        "message_start",
        json!({"type": "message_start", "message": {"id": "m", "type": "message",
            "role": "assistant", "content": [], "stop_reason": null,
            "usage": {"input_tokens": 25, "output_tokens": 1}}}),
    )
}
fn m_text(t: &str) -> Vec<u8> {
    named(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": t}}),
    )
}
fn m_delta(reason: &str, usage: bool) -> Vec<u8> {
    let mut v = json!({"type": "message_delta",
                       "delta": {"stop_reason": reason, "stop_sequence": null}});
    if usage {
        v["usage"] = json!({"output_tokens": 15});
    }
    named("message_delta", v)
}
fn m_stop() -> Vec<u8> {
    named("message_stop", json!({"type": "message_stop"}))
}
fn m_ping() -> Vec<u8> {
    named("ping", json!({"type": "ping"}))
}
fn messages_stream_ok() -> Canned {
    Canned::sse(chunks(vec![
        m_start(),
        m_ping(),
        named(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        m_text("Hel"),
        named(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "SECRET-THOUGHT"}}),
        ),
        m_text("lo"),
        named(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        m_delta("end_turn", true),
        m_stop(),
    ]))
}

async fn run(
    client: &HostedClient,
    route: &HostedRoute,
    rec: &Rec,
) -> crate::providers::hosted_stream::StreamAttempt {
    client
        .generate_stream_metered(&ctx_for(route), &request(), rec, &CancellationToken::new())
        .await
}

fn err_of(a: &crate::providers::hosted_stream::StreamAttempt) -> &ProviderError {
    a.attempt.result.as_ref().err().expect("expected an error")
}

// ---------------------------------------------------------------------------
// openai_chat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_stream_sends_a_stream_request_and_returns_text_and_final_usage() {
    let server = LoopbackServer::start(vec![chat_stream_ok()]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;

    let out = a.attempt.result.as_ref().unwrap();
    assert_eq!(out.response.content, "Hello");
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    assert!(!out.truncated);
    let usage = a.attempt.usage.expect("final usage");
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (11, 7));
    assert_eq!(out.response.prompt_token_count, 11);
    assert_eq!(a.received_text, "Hello");
    assert_eq!(rec.text(), "Hello");
    assert!(a.attempt.reached_provider);
    assert_eq!(a.attempt.http_status, Some(200));
    assert!(!a.cancelled);

    let sent = server.captured();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].path, "/v1/chat/completions");
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    assert_eq!(
        sent[0].header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    let body = sent[0].json();
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(!sent[0].body.contains(SECRET));
}

#[tokio::test]
async fn chat_stream_survives_any_split_of_the_bytes_including_inside_a_character() {
    let mut doc: Vec<u8> = Vec::new();
    for f in [
        chat_delta("caf\u{e9} "),
        chat_delta("\u{1F600} \u{4E16}\u{754C}"),
        chat_finish("stop"),
        chat_usage(),
        done(),
    ] {
        doc.extend(f);
    }
    // Split at every byte position of a representative few: each run is a
    // separate server, so cover boundaries inside the multi-byte characters.
    let positions: Vec<usize> = (0..doc.len()).step_by(7).collect();
    for cut in positions {
        let server = LoopbackServer::start(vec![Canned::sse(vec![
            StreamStep::Chunk(doc[..cut].to_vec()),
            StreamStep::Pause(Duration::from_millis(5)),
            StreamStep::Chunk(doc[cut..].to_vec()),
        ])]);
        let rec = Rec::default();
        let a = run(
            &client().with_stream_limits(limits()),
            &chat_route(&server.base()),
            &rec,
        )
        .await;
        let out = a
            .attempt
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("split at {cut}: {e:?}"));
        assert_eq!(
            out.response.content, "caf\u{e9} \u{1F600} \u{4E16}\u{754C}",
            "split at {cut}"
        );
    }
}

#[tokio::test]
async fn chat_stream_openrouter_shape_usage_frame_with_one_choice_and_keepalive() {
    // Documented shape: keepalive comments; a final frame with ONE choice, an
    // empty-content delta repeating finish_reason (plus native_finish_reason)
    // and a usage object; then [DONE].
    let frames = vec![
        b": OPENROUTER PROCESSING\n\n".to_vec(),
        data(json!({"id": "x", "choices": [{"index": 0,
            "delta": {"role": "assistant", "content": "Hello"}, "finish_reason": null}]})),
        b": OPENROUTER PROCESSING\n\n".to_vec(),
        data(
            json!({"choices": [{"index": 0, "delta": {"content": " world"},
            "finish_reason": null}]}),
        ),
        data(json!({"choices": [{"index": 0, "delta": {"content": ""},
            "finish_reason": "stop", "native_finish_reason": "stop"}]})),
        data(json!({"choices": [{"index": 0, "delta": {"content": ""},
            "finish_reason": "stop", "native_finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}})),
        done(),
    ];
    let server = LoopbackServer::start(vec![Canned::sse(chunks(frames))]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    let out = a.attempt.result.as_ref().unwrap();
    assert_eq!(out.response.content, "Hello world");
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    let u = a.attempt.usage.expect("the accounting frame carries usage");
    assert_eq!((u.prompt_tokens, u.completion_tokens), (11, 7));
}

#[tokio::test]
async fn chat_stream_reports_reasoning_tokens_from_the_usage_frame() {
    let frames = vec![
        chat_delta("ok"),
        chat_finish("stop"),
        data(
            json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 9,
            "completion_tokens_details": {"reasoning_tokens": 6}}}),
        ),
        done(),
    ];
    let server = LoopbackServer::start(vec![Canned::sse(chunks(frames))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert_eq!(a.attempt.usage.unwrap().reasoning_tokens, Some(6));
    assert_eq!(a.attempt.result.as_ref().unwrap().reasoning_tokens, Some(6));
}

#[tokio::test]
async fn chat_stream_without_usage_completes_with_no_usage() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_delta("Hi"),
        chat_finish("stop"),
        done(),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, "Hi");
    assert!(a.attempt.usage.is_none());
    assert!(a.attempt.reached_provider);
}

#[tokio::test]
async fn chat_stream_a_finish_reason_then_a_clean_close_is_complete_without_done() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_delta("Hi"),
        chat_finish("stop"),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, "Hi");
}

#[tokio::test]
async fn chat_stream_with_the_last_event_unterminated_still_completes() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(chat_delta("Hi")),
        StreamStep::Chunk(chat_finish("stop")),
        StreamStep::Chunk(b"data: [DONE]".to_vec()),
    ])]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, "Hi");
}

#[tokio::test]
async fn chat_stream_in_stream_error_as_the_first_and_only_event_keeps_its_class() {
    // OpenRouter-style: a top-level error object with finish_reason "error",
    // under HTTP 200, before any text.
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![data(json!({
        "error": {"code": 429, "message": "UPSTREAM-SECRET-TEXT"},
        "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]
    }))]))]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert!(matches!(err_of(&a), ProviderError::RateLimit { .. }));
    assert!(a.attempt.usage.is_none());
    assert!(a.attempt.reached_provider);
    assert_eq!(a.attempt.http_status, Some(200));
    assert_eq!(rec.count(), 0);
    assert!(!format!("{:?}", err_of(&a)).contains("UPSTREAM-SECRET-TEXT"));
}

#[tokio::test]
async fn chat_stream_error_event_with_a_string_code_is_classified_by_its_words() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![data(json!({
        "error": {"code": "server_error", "message": "x"},
        "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]
    }))]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(err_of(&a), ProviderError::ServerError { .. }));
}

#[tokio::test]
async fn chat_stream_error_after_text_is_terminal_and_keeps_the_text() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_delta("Hel"),
        data(json!({"error": {"code": 503, "message": "UPSTREAM-SECRET-TEXT"}})),
    ]))]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));
    assert_eq!(a.received_text, "Hel");
    assert_eq!(rec.text(), "Hel");
    assert!(a.attempt.usage.is_none());
    assert_eq!(a.attempt.http_status, Some(200));
    assert!(!format!("{:?}", err_of(&a)).contains("UPSTREAM-SECRET-TEXT"));
}

#[tokio::test]
async fn chat_stream_cut_after_text_is_stream_interrupted() {
    for cut in [
        StreamStep::Cut,
        StreamStep::CutMidChunk(b"data: {\"cho".to_vec()),
    ] {
        let server = LoopbackServer::start(vec![Canned::sse(vec![
            StreamStep::Chunk(chat_delta("Hel")),
            cut,
        ])]);
        let rec = Rec::default();
        let a = run(
            &client().with_stream_limits(limits()),
            &chat_route(&server.base()),
            &rec,
        )
        .await;
        assert!(
            matches!(err_of(&a), ProviderError::StreamInterrupted { .. }),
            "{:?}",
            err_of(&a)
        );
        assert_eq!(a.received_text, "Hel");
        assert!(a.attempt.reached_provider);
        assert!(a.attempt.usage.is_none());
    }
}

#[tokio::test]
async fn chat_stream_cut_before_any_text_keeps_the_retryable_class() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![StreamStep::Cut])]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(
        matches!(
            err_of(&a),
            ProviderError::Unavailable { .. } | ProviderError::Timeout { .. }
        ),
        "{:?}",
        err_of(&a)
    );
    assert!(a.attempt.reached_provider);
}

#[tokio::test]
async fn chat_stream_short_content_length_is_a_cut() {
    let body = chat_delta("Hel");
    let canned = Canned::bytes(200, body.clone())
        .with_header("Content-Type", "text/event-stream")
        .with_declared_length(body.len() + 200);
    let server = LoopbackServer::start(vec![canned]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(err_of(&a).to_string().contains("fixture-host"));
    assert!(a.attempt.result.is_err());
    assert!(a.attempt.usage.is_none());
}

#[tokio::test]
async fn chat_stream_ending_without_any_completion_signal_is_incomplete() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![chat_delta("Hel")]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));
    assert_eq!(a.received_text, "Hel");
}

#[tokio::test]
async fn chat_stream_idle_timeout_after_text_is_stream_interrupted() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(chat_delta("Hel")),
        StreamStep::Pause(Duration::from_secs(10)),
    ])]);
    let c = client().with_stream_limits(StreamLimits {
        idle_timeout: Duration::from_millis(200),
        ..limits()
    });
    let started = Instant::now();
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));
    assert_eq!(a.received_text, "Hel");
}

#[tokio::test]
async fn chat_stream_idle_timeout_before_any_text_is_a_timeout() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![StreamStep::Pause(
        Duration::from_secs(10),
    )])]);
    let c = client().with_stream_limits(StreamLimits {
        idle_timeout: Duration::from_millis(200),
        ..limits()
    });
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(matches!(err_of(&a), ProviderError::Timeout { .. }));
}

#[tokio::test]
async fn chat_stream_total_deadline_stops_a_keepalive_that_never_ends() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![StreamStep::Hold(
        Duration::from_secs(20),
    )])]);
    let c = client().with_stream_limits(StreamLimits {
        total_timeout: Duration::from_millis(400),
        ..limits()
    });
    let started = Instant::now();
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(matches!(err_of(&a), ProviderError::Timeout { .. }));
    assert!(a.attempt.reached_provider);
}

#[tokio::test]
async fn chat_stream_wire_byte_cap_stops_a_flood() {
    let flood = b": x\n".repeat(2000);
    let server = LoopbackServer::start(vec![Canned::sse(vec![StreamStep::Chunk(flood)])]);
    let c = HostedClient::for_loopback_tests(Duration::from_secs(5), Duration::from_secs(2), 2048)
        .with_stream_limits(limits());
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(matches!(
        err_of(&a),
        ProviderError::MalformedResponse { .. }
    ));
}

#[tokio::test]
async fn chat_stream_event_cap_stops_an_oversized_event() {
    let big = format!("data: {}\n\n", "x".repeat(400)).into_bytes();
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![big]))]);
    let c = client().with_stream_limits(StreamLimits {
        max_event_bytes: 64,
        ..limits()
    });
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(matches!(
        err_of(&a),
        ProviderError::MalformedResponse { .. }
    ));
}

#[tokio::test]
async fn chat_stream_accumulated_text_cap_stops_a_runaway_reply() {
    let mut frames = Vec::new();
    for _ in 0..40 {
        frames.push(chat_delta(&"y".repeat(100)));
    }
    let server = LoopbackServer::start(vec![Canned::sse(chunks(frames))]);
    let c = HostedClient::for_loopback_tests(Duration::from_secs(5), Duration::from_secs(2), 2000)
        .with_stream_limits(limits());
    let a = run(&c, &chat_route(&server.base()), &Rec::default()).await;
    assert!(a.attempt.result.is_err());
    assert!(a.received_text.len() <= 2000);
}

#[tokio::test]
async fn chat_stream_malformed_event_is_a_failure_not_skipped() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_delta("Hel"),
        b"data: {not json}\n\n".to_vec(),
        chat_delta("lo"),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));
    assert_eq!(a.received_text, "Hel");
}

#[tokio::test]
async fn chat_stream_length_with_text_is_truncated_and_without_text_is_budget_exhausted() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_delta("Hel"),
        chat_finish("length"),
        chat_usage(),
        done(),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    let out = a.attempt.result.as_ref().unwrap();
    assert!(out.truncated);
    assert_eq!(
        out.response.completion_status,
        crate::providers::types::CompletionStatus::Truncated
    );
    assert_eq!(out.response.content, "Hel");

    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        chat_finish("length"),
        chat_usage(),
        done(),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::OutputBudgetExhausted { .. }
    ));
    assert!(a.attempt.usage.is_some(), "a burned budget is still billed");
}

#[tokio::test]
async fn chat_stream_reasoning_and_refusal_fields_are_not_shown() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        data(json!({"choices": [{"index": 0,
            "delta": {"reasoning_content": "SECRET-THOUGHT", "reasoning": "SECRET-THOUGHT"},
            "finish_reason": null}]})),
        chat_delta("Hi"),
        chat_finish("stop"),
        done(),
    ]))]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert_eq!(a.received_text, "Hi");
    assert!(!rec.text().contains("SECRET-THOUGHT"));
}

#[tokio::test]
async fn a_server_that_ignores_stream_true_and_sends_json_is_handled() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({"choices": [{"message": {"role": "assistant", "content": "whole"},
                             "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2}})
        .to_string(),
    )]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, "whole");
    assert_eq!(rec.text(), "whole");
    assert_eq!(a.attempt.usage.unwrap().completion_tokens, 2);
}

#[tokio::test]
async fn non_success_before_the_stream_uses_the_ordinary_mapping_and_costs_nothing() {
    let server = LoopbackServer::start(vec![Canned::json(401, "{\"error\":\"UPSTREAM-SECRET\"}")]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert!(matches!(err_of(&a), ProviderError::InvalidApiKey { .. }));
    assert_eq!(a.attempt.http_status, Some(401));
    assert!(a.attempt.reached_provider);
    assert!(a.attempt.usage.is_none());
    assert!(!format!("{:?}", err_of(&a)).contains("UPSTREAM-SECRET"));
    assert_eq!(rec.count(), 0);
}

#[tokio::test]
async fn a_missing_token_limit_fails_closed_before_any_traffic() {
    let server = LoopbackServer::start(vec![chat_stream_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({}));
    let a = run(
        &client().with_stream_limits(limits()),
        &route,
        &Rec::default(),
    )
    .await;
    assert!(matches!(err_of(&a), ProviderError::Misconfigured { .. }));
    assert!(!a.attempt.reached_provider);
    assert_eq!(server.connections(), 0);
}

// -- stream_options ---------------------------------------------------------

#[tokio::test]
async fn a_row_can_replace_or_remove_the_default_stream_options() {
    for (opt, expect) in [
        (json!({"stream_options": null}), None),
        (
            json!({"stream_options": {"include_usage": false}}),
            Some(json!({"include_usage": false})),
        ),
    ] {
        let server = LoopbackServer::start(vec![chat_stream_ok()]);
        let mut route = chat_route(&server.base());
        let mut o = json!({"max_completion_tokens": 512});
        o.as_object_mut()
            .unwrap()
            .extend(opt.as_object().unwrap().clone());
        route.request_options = Some(o);
        let a = run(
            &client().with_stream_limits(limits()),
            &route,
            &Rec::default(),
        )
        .await;
        assert!(a.attempt.result.is_ok());
        let body = server.captured()[0].json();
        assert_eq!(body.get("stream_options").cloned(), expect);
        assert_eq!(body["stream"], true);
    }
}

#[tokio::test]
async fn a_whole_body_call_never_sends_stream_options_even_if_a_row_sets_them() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({"choices": [{"message": {"content": "x"}, "finish_reason": "stop"}]}).to_string(),
    )]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({
        "max_completion_tokens": 512,
        "stream_options": {"include_usage": true}
    }));
    client()
        .generate(&ctx_for(&route), &request())
        .await
        .unwrap();
    let body = server.captured()[0].json();
    assert_eq!(body["stream"], false);
    assert!(body.get("stream_options").is_none());
}

#[test]
fn stream_options_must_be_an_object_or_null_and_stream_stays_forbidden_in_rows() {
    let mut r = chat_route("http://127.0.0.1:1");
    r.request_options = Some(json!({"max_completion_tokens": 5, "stream_options": 3}));
    assert!(CallContext::build_for_loopback(&r, SECRET, &json!({})).is_err());
    r.request_options = Some(json!({"max_completion_tokens": 5, "stream": true}));
    assert!(CallContext::build_for_loopback(&r, SECRET, &json!({})).is_err());
    r.request_options = Some(json!({"max_completion_tokens": 5, "stream_options": null}));
    assert!(CallContext::build_for_loopback(&r, SECRET, &json!({})).is_ok());
}

// -- coalescing -------------------------------------------------------------

#[tokio::test]
async fn tiny_deltas_are_batched_but_nothing_is_lost_or_reordered() {
    let mut frames = Vec::new();
    let mut expect = String::new();
    for i in 0..300 {
        let t = format!("{i},");
        expect.push_str(&t);
        frames.push(chat_delta(&t));
    }
    frames.push(chat_finish("stop"));
    frames.push(done());
    let server = LoopbackServer::start(vec![Canned::sse(chunks(frames))]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &chat_route(&server.base()),
        &rec,
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, expect);
    assert_eq!(rec.text(), expect);
    assert!(rec.count() < 300, "{} events for 300 deltas", rec.count());
    let seqs = rec.seqs();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
}

#[tokio::test]
async fn the_last_words_go_out_even_if_the_provider_then_goes_quiet() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(chat_delta("first")),
        StreamStep::Chunk(chat_delta(" second")),
        StreamStep::Pause(Duration::from_millis(600)),
        StreamStep::Chunk(chat_finish("stop")),
        StreamStep::Chunk(done()),
    ])]);
    let rec = std::sync::Arc::new(Rec::default());
    let c = client().with_stream_limits(limits());
    let route = chat_route(&server.base());
    let r2 = rec.clone();
    let handle = tokio::spawn(async move {
        let a = c
            .generate_stream_metered(
                &ctx_for(&route),
                &request(),
                &*r2,
                &CancellationToken::new(),
            )
            .await;
        a.attempt.result.is_ok()
    });
    tokio::time::sleep(Duration::from_millis(350)).await;
    // Mid-pause: both deltas must already be on screen.
    assert_eq!(rec.text(), "first second");
    assert!(handle.await.unwrap());
}

// -- cancellation -----------------------------------------------------------

#[tokio::test]
async fn cancel_during_the_stream_stops_the_read_and_the_server_sees_the_close() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(chat_delta("Hi")),
        StreamStep::Hold(Duration::from_secs(20)),
    ])]);
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        t2.cancel();
    });
    let rec = Rec::default();
    let c = client().with_stream_limits(limits());
    let started = Instant::now();
    let a = c
        .generate_stream_metered(
            &ctx_for(&chat_route(&server.base())),
            &request(),
            &rec,
            &token,
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(matches!(err_of(&a), ProviderError::StreamCancelled { .. }));
    assert!(a.cancelled);
    assert_eq!(a.received_text, "Hi");
    assert_eq!(rec.text(), "Hi");
    // After the request was sent: never a release.
    assert!(a.attempt.reached_provider);
    assert_eq!(a.attempt.http_status, Some(200));
    assert!(a.attempt.usage.is_none());

    // The HTTP read really stopped: the server finds the connection closed.
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.client_closed() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(server.client_closed(), 1, "the server never saw the close");
}

#[tokio::test]
async fn cancel_before_the_first_byte_still_counts_as_reached() {
    let server = LoopbackServer::start(vec![
        chat_stream_ok().with_delay(Duration::from_millis(1500))
    ]);
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        t2.cancel();
    });
    let started = Instant::now();
    let a = client()
        .with_stream_limits(limits())
        .generate_stream_metered(
            &ctx_for(&chat_route(&server.base())),
            &request(),
            &Rec::default(),
            &token,
        )
        .await;
    assert!(started.elapsed() < Duration::from_millis(1400));
    assert!(matches!(err_of(&a), ProviderError::StreamCancelled { .. }));
    assert!(a.cancelled);
    assert!(a.attempt.reached_provider, "the request may have been sent");
    assert_eq!(a.attempt.http_status, None);
}

#[tokio::test]
async fn cancel_before_dispatch_sends_nothing_and_is_not_reached() {
    let server = LoopbackServer::start(vec![chat_stream_ok()]);
    let token = CancellationToken::new();
    token.cancel();
    let a = client()
        .with_stream_limits(limits())
        .generate_stream_metered(
            &ctx_for(&chat_route(&server.base())),
            &request(),
            &Rec::default(),
            &token,
        )
        .await;
    assert!(matches!(err_of(&a), ProviderError::StreamCancelled { .. }));
    assert!(!a.attempt.reached_provider);
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------
// anthropic_messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_stream_sends_a_stream_request_and_returns_text_and_final_usage() {
    let server = LoopbackServer::start(vec![messages_stream_ok()]);
    let rec = Rec::default();
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &rec,
    )
    .await;
    let out = a.attempt.result.as_ref().unwrap();
    assert_eq!(out.response.content, "Hello");
    assert_eq!(out.finish_reason.as_deref(), Some("end_turn"));
    let u = a.attempt.usage.expect("message_delta usage is final");
    assert_eq!((u.prompt_tokens, u.completion_tokens), (25, 15));
    assert_eq!(rec.text(), "Hello");
    assert!(!rec.text().contains("SECRET-THOUGHT"));

    let sent = server.captured();
    assert_eq!(sent[0].path, "/v1/messages");
    assert_eq!(sent[0].header("x-api-key"), Some(SECRET));
    assert_eq!(sent[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    let body = sent[0].json();
    assert_eq!(body["stream"], true);
    assert!(body.get("stream_options").is_none());
    assert_eq!(body["max_tokens"], 256);
}

#[tokio::test]
async fn messages_stream_without_a_usage_figure_has_no_usage() {
    // The reference shows streams whose message_delta carries no usage.
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        m_start(),
        m_text("Hi"),
        m_delta("end_turn", false),
        m_stop(),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert_eq!(a.attempt.result.as_ref().unwrap().response.content, "Hi");
    assert!(a.attempt.usage.is_none());
}

#[tokio::test]
async fn messages_stream_a_message_start_figure_alone_is_not_final_usage() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        m_start(),
        m_text("Hi"),
        named(
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "x"}}),
        ),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));
    assert!(a.attempt.usage.is_none());
    assert_eq!(a.received_text, "Hi");
}

#[tokio::test]
async fn messages_stream_overloaded_before_any_text_keeps_the_retryable_class() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![named(
        "error",
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "x"}}),
    )]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(err_of(&a), ProviderError::ServerError { .. }));
}

#[tokio::test]
async fn messages_stream_cut_and_missing_message_stop_are_interrupted() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(m_start()),
        StreamStep::Chunk(m_text("Hi")),
        StreamStep::Cut,
    ])]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(matches!(
        err_of(&a),
        ProviderError::StreamInterrupted { .. }
    ));

    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        m_start(),
        m_text("Hi"),
        m_delta("end_turn", true),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    assert!(
        matches!(err_of(&a), ProviderError::StreamInterrupted { .. }),
        "{:?}",
        err_of(&a)
    );
}

#[tokio::test]
async fn messages_stream_max_tokens_with_text_is_truncated() {
    let server = LoopbackServer::start(vec![Canned::sse(chunks(vec![
        m_start(),
        m_text("Hi"),
        m_delta("max_tokens", true),
        m_stop(),
    ]))]);
    let a = run(
        &client().with_stream_limits(limits()),
        &messages_route(&server.base()),
        &Rec::default(),
    )
    .await;
    let out = a.attempt.result.as_ref().unwrap();
    assert!(out.truncated);
    assert_eq!(
        out.response.completion_status,
        crate::providers::types::CompletionStatus::Truncated
    );
}

#[tokio::test]
async fn messages_stream_cancel_stops_the_read() {
    let server = LoopbackServer::start(vec![Canned::sse(vec![
        StreamStep::Chunk(m_start()),
        StreamStep::Chunk(m_text("Hi")),
        StreamStep::Hold(Duration::from_secs(20)),
    ])]);
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        t2.cancel();
    });
    let a = client()
        .with_stream_limits(limits())
        .generate_stream_metered(
            &ctx_for(&messages_route(&server.base())),
            &request(),
            &Rec::default(),
            &token,
        )
        .await;
    assert!(matches!(err_of(&a), ProviderError::StreamCancelled { .. }));
    assert!(a.attempt.reached_provider);
    assert_eq!(a.received_text, "Hi");
}

// ---------------------------------------------------------------------------
// Decoders (pure)
// ---------------------------------------------------------------------------

fn ev(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_owned(),
    }
}

#[test]
fn chat_decoder_reads_text_finish_usage_and_done() {
    let mut d = StreamDecoder::new(Dialect::OpenaiChat);
    assert_eq!(
        d.on_event(&ev(
            r#"{"choices":[{"delta":{"content":"a"},"finish_reason":null}]}"#
        )),
        Ok(Step::Text("a".to_owned()))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)),
        Ok(Step::Skip)
    );
    assert!(d.complete(), "a finish reason completes the chat dialect");
    assert_eq!(
        d.on_event(&ev(
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#
        )),
        Ok(Step::Skip)
    );
    assert_eq!(d.usage().unwrap().completion_tokens, 2);
    assert_eq!(d.on_event(&ev("[DONE]")), Ok(Step::Done));
}

#[test]
fn chat_decoder_ignores_empty_choices_unknown_fields_and_non_string_content() {
    let mut d = StreamDecoder::new(Dialect::OpenaiChat);
    for s in [
        r#"{"choices":[]}"#,
        r#"{}"#,
        r#"{"choices":[{"delta":{"content":null}}]}"#,
        r#"{"choices":[{"delta":{"content":5}}]}"#,
        r#"{"choices":[{"delta":{"content":""}}]}"#,
        r#"{"choices":"x"}"#,
        r#"[]"#,
    ] {
        assert_eq!(d.on_event(&ev(s)), Ok(Step::Skip), "{s}");
    }
    assert!(!d.complete());
}

#[test]
fn chat_decoder_faults() {
    let mut d = StreamDecoder::new(Dialect::OpenaiChat);
    assert_eq!(d.on_event(&ev("not json")), Err(Fault::Malformed));
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"code":401}}"#)),
        Err(Fault::Upstream(Upstream::Auth))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"code":402}}"#)),
        Err(Fault::Upstream(Upstream::Payment))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"type":"insufficient_quota"}}"#)),
        Err(Fault::Upstream(Upstream::Payment))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"code":"rate_limit_exceeded"}}"#)),
        Err(Fault::Upstream(Upstream::RateLimit))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"code":404}}"#)),
        Err(Fault::Upstream(Upstream::NotFound))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"error":{"message":"x"}}"#)),
        Err(Fault::Upstream(Upstream::Other))
    );
    assert_eq!(
        d.on_event(&ev(r#"{"choices":[{"finish_reason":"error"}]}"#)),
        Err(Fault::Upstream(Upstream::Other))
    );
}

#[test]
fn messages_decoder_reads_the_documented_sequence() {
    let mut d = StreamDecoder::new(Dialect::AnthropicMessages);
    let steps = [
        (
            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1}}}"#,
            Step::Skip,
        ),
        (r#"{"type":"ping"}"#, Step::Skip),
        (
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            Step::Skip,
        ),
        (
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            Step::Text("Hi".to_owned()),
        ),
        (
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"}}"#,
            Step::Skip,
        ),
        (r#"{"type":"content_block_stop","index":0}"#, Step::Skip),
        (r#"{"type":"future_event","x":1}"#, Step::Skip),
    ];
    for (s, expect) in steps {
        assert_eq!(d.on_event(&ev(s)), Ok(expect), "{s}");
    }
    assert!(d.usage().is_none(), "message_start is not final");
    assert_eq!(
        d.on_event(&ev(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#)),
        Ok(Step::Skip)
    );
    let u = d.usage().unwrap();
    assert_eq!((u.prompt_tokens, u.completion_tokens), (25, 15));
    assert!(!d.complete());
    assert_eq!(
        d.on_event(&ev(r#"{"type":"message_stop"}"#)),
        Ok(Step::Done)
    );
    assert!(d.complete());
    assert_eq!(d.finish_reason().as_deref(), Some("end_turn"));
}

#[test]
fn messages_decoder_uses_the_event_name_when_the_data_has_no_type() {
    let mut d = StreamDecoder::new(Dialect::AnthropicMessages);
    let e = SseEvent {
        event: Some("message_stop".to_owned()),
        data: "{}".to_owned(),
    };
    assert_eq!(d.on_event(&e), Ok(Step::Done));
}

#[test]
fn messages_decoder_faults() {
    let mut d = StreamDecoder::new(Dialect::AnthropicMessages);
    assert_eq!(d.on_event(&ev("nope")), Err(Fault::Malformed));
    for (t, c) in [
        ("authentication_error", Upstream::Auth),
        ("permission_error", Upstream::Auth),
        ("billing_error", Upstream::Payment),
        ("not_found_error", Upstream::NotFound),
        ("rate_limit_error", Upstream::RateLimit),
        ("overloaded_error", Upstream::Server(529)),
        ("api_error", Upstream::Server(500)),
        ("invalid_request_error", Upstream::Other),
        ("something_new", Upstream::Other),
    ] {
        let s = format!(r#"{{"type":"error","error":{{"type":"{t}","message":"m"}}}}"#);
        assert_eq!(d.on_event(&ev(&s)), Err(Fault::Upstream(c)), "{t}");
    }
}

#[test]
fn hostile_json_never_panics_a_decoder() {
    for dialect in [Dialect::OpenaiChat, Dialect::AnthropicMessages] {
        let mut d = StreamDecoder::new(dialect);
        for s in [
            "",
            "null",
            "123",
            "\"x\"",
            "[[[[[[[[",
            r#"{"choices":[null]}"#,
            r#"{"type":5}"#,
            r#"{"type":"content_block_delta","delta":5}"#,
            r#"{"usage":{"prompt_tokens":-1,"completion_tokens":1e99}}"#,
            r#"{"type":"message_delta","usage":{"output_tokens":99999999999999999999}}"#,
        ] {
            let _ = d.on_event(&ev(s));
        }
    }
}

// ---------------------------------------------------------------------------
// Source scans
// ---------------------------------------------------------------------------

/// decisions.id=883: delta text is display-only and never logged. The stream
/// code (parser, decoders, client) makes no log call at all.
#[test]
fn stream_code_never_logs() {
    for (name, src) in [
        ("hosted_stream.rs", include_str!("hosted_stream.rs")),
        ("sse.rs", include_str!("sse.rs")),
    ] {
        // Strip this file's own test module before scanning the production code.
        let production = src.split("#[cfg(test)]").next().unwrap();
        for line in production.lines() {
            let code = line.split("//").next().unwrap_or("");
            assert!(
                !code.contains("log::")
                    && !code.contains("println!")
                    && !code.contains("eprintln!"),
                "{name} logs: {line}"
            );
        }
    }
}

/// The streamed client method is called only from the spend gate (and tests),
/// so no streamed request can skip consent, price, cap and limit checks.
#[test]
fn streaming_is_reachable_only_through_the_spend_gate() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with("_tests.rs") || name == "hosted_stream.rs" || name == "gate.rs" {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        let production = src.split("#[cfg(test)]").next().unwrap();
        let code: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("generate_stream_metered"),
            "{} calls the streamed client method outside the spend gate",
            f.display()
        );
    }
}
