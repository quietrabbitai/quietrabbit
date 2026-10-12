// src-tauri/src/providers/hosted_client_tests.rs
//
// Loopback-HTTP tests for the generic hosted client (items.id=709, absorbing
// the intent of items.id=673 and items.id=209). Everything talks to a local
// 127.0.0.1 server; nothing leaves the machine. No provider or model name
// appears here (decisions.id=872): rows are neutral fixtures.

use std::time::Duration;

use serde_json::{json, Value};

use super::*;
use crate::persistence::provider_store::ModelStatus;
use crate::providers::hosted_call::HostedRoute;
use crate::providers::loopback_test_server::{Canned, LoopbackServer};

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

fn request() -> HostedRequest {
    HostedRequest {
        system: None,
        prompt: "hello".to_owned(),
        temperature: Some(0.5),
        max_output_tokens: None,
    }
}

fn chat_ok() -> Canned {
    Canned::json(
        200,
        &json!({
            "choices": [{"message": {"role": "assistant", "content": "hi there"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        })
        .to_string(),
    )
}

fn messages_ok() -> Canned {
    Canned::json(
        200,
        &json!({
            "type": "message",
            "content": [{"type": "text", "text": "hi "}, {"type": "tool_use", "id": "x"},
                        {"type": "text", "text": "there"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 21, "output_tokens": 9}
        })
        .to_string(),
    )
}

/// Everything user-visible or loggable about an error.
fn error_text(e: &ProviderError) -> String {
    format!("{e:?} | {e} | {}", plain_text(e))
}

fn plain_text(e: &ProviderError) -> String {
    let plain = match e {
        ProviderError::MissingApiKey { plain_language, .. }
        | ProviderError::InvalidApiKey { plain_language, .. }
        | ProviderError::RateLimit { plain_language, .. }
        | ProviderError::Timeout { plain_language, .. }
        | ProviderError::Unavailable { plain_language, .. }
        | ProviderError::ServerError { plain_language, .. }
        | ProviderError::ModelNotFound { plain_language, .. }
        | ProviderError::PaymentRequired { plain_language, .. }
        | ProviderError::MalformedResponse { plain_language, .. }
        | ProviderError::OutputBudgetExhausted { plain_language, .. }
        | ProviderError::Misconfigured { plain_language, .. }
        | ProviderError::StreamInterrupted { plain_language, .. }
        | ProviderError::StreamCancelled { plain_language, .. }
        | ProviderError::ProviderFailure { plain_language, .. }
        | ProviderError::UnknownProvider { plain_language, .. }
        | ProviderError::MissingQrHostedConfig { plain_language } => plain_language.clone(),
    };
    plain
}

// ---------------------------------------------------------------------------
// openai_chat dialect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_success_sends_the_expected_request_and_extracts_usage() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));

    let out = client().generate(&ctx, &request()).await.unwrap();

    assert_eq!(out.response.content, "hi there");
    assert_eq!(out.response.prompt_token_count, 11);
    assert_eq!(out.response.output_token_count, 7);
    assert_eq!(out.response.model, "fixture-model");
    assert_eq!(out.response.completion_status, CompletionStatus::Complete);
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    assert_eq!(out.reasoning_tokens, None);

    let sent = server.captured();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].path, "/v1/chat/completions");
    assert_eq!(
        sent[0].header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(sent[0].header("x-api-key"), None);
    assert_eq!(sent[0].header("content-type"), Some("application/json"));
    let body = sent[0].json();
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["stream"], false);
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["max_completion_tokens"], 512);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hello");
    assert!(
        !sent[0].body.contains(SECRET),
        "key must not be in the body"
    );
}

#[tokio::test]
async fn chat_system_prompt_is_a_leading_system_message() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let mut req = request();
    req.system = Some("be brief".to_owned());
    client().generate(&ctx, &req).await.unwrap();

    let body = server.captured()[0].json();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "be brief");
    assert_eq!(body["messages"][1]["role"], "user");
}

#[tokio::test]
async fn chat_reports_reasoning_tokens_when_present() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 40,
                      "completion_tokens_details": {"reasoning_tokens": 33}}
        })
        .to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let out = client().generate(&ctx, &request()).await.unwrap();
    assert_eq!(out.reasoning_tokens, Some(33));
    assert_eq!(out.response.output_token_count, 40);
}

#[tokio::test]
async fn chat_missing_usage_counts_as_zero() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let out = client().generate(&ctx, &request()).await.unwrap();
    assert_eq!(out.response.prompt_token_count, 0);
    assert_eq!(out.response.output_token_count, 0);
}

// -- token budget: the row names the key, a call may replace the value, and
//    with neither the call fails closed before any traffic ------------------

#[tokio::test]
async fn call_budget_replaces_the_rows_value_under_the_rows_key() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let mut req = request();
    req.max_output_tokens = Some(100);
    client().generate(&ctx, &req).await.unwrap();
    let body = server.captured()[0].json();
    assert_eq!(body["max_completion_tokens"], 100);
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn call_budget_without_a_row_key_is_sent_as_max_tokens() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({}));
    let ctx = ctx_for(&route);
    let mut req = request();
    req.max_output_tokens = Some(64);
    client().generate(&ctx, &req).await.unwrap();
    let body = server.captured()[0].json();
    assert_eq!(body["max_tokens"], 64);
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn a_row_that_uses_max_tokens_keeps_that_key() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({"max_tokens": 300}));
    let ctx = ctx_for(&route);
    client().generate(&ctx, &request()).await.unwrap();
    let body = server.captured()[0].json();
    assert_eq!(body["max_tokens"], 300);
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn chat_fails_closed_with_no_token_limit_anywhere() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({}));
    let ctx = ctx_for(&route);

    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Misconfigured { .. }),
        "{err:?}"
    );
    assert!(plain_text(&err).contains("Fixture Host"));
    assert_eq!(
        server.connections(),
        0,
        "nothing may be sent without a limit"
    );

    // A zero budget on the call does not count as a budget either.
    let mut req = request();
    req.max_output_tokens = Some(0);
    assert!(client().generate(&ctx, &req).await.is_err());
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn request_options_merge_into_the_body() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({
        "max_completion_tokens": 512,
        "reasoning_effort": "low",
        "provider": {"data_collection": "deny", "order": ["a", "b"]},
        "store": false,
        "top_p": 0.9,
        "temperature": null
    }));
    let ctx = ctx_for(&route);
    client().generate(&ctx, &request()).await.unwrap();

    let body = server.captured()[0].json();
    assert_eq!(body["reasoning_effort"], "low");
    assert_eq!(body["provider"]["data_collection"], "deny");
    assert_eq!(body["provider"]["order"], json!(["a", "b"]));
    assert_eq!(body["store"], false);
    assert_eq!(body["top_p"], 0.9);
    assert!(
        body.get("temperature").is_none(),
        "a null option removes the key (call temperature included)"
    );
    // The client's own fields survive the merge.
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["stream"], false);
}

#[tokio::test]
async fn row_options_cannot_override_model_messages_or_stream() {
    // build() rejects them outright, so no request is ever made.
    for reserved in ["model", "messages", "stream"] {
        let server = LoopbackServer::start(vec![chat_ok()]);
        let mut route = chat_route(&server.base());
        route.request_options = Some(json!({ reserved: "x", "max_tokens": 5 }));
        let out = CallContext::build_for_loopback(&route, SECRET, &json!({}));
        assert!(matches!(out, Err(ProviderError::Misconfigured { .. })));
        assert_eq!(server.connections(), 0);
    }
}

// -- error mapping -----------------------------------------------------------

#[tokio::test]
async fn chat_status_codes_map_to_typed_errors_naming_the_row() {
    let cases: Vec<(u16, fn(&ProviderError) -> bool)> = vec![
        (401, |e| matches!(e, ProviderError::InvalidApiKey { .. })),
        (403, |e| matches!(e, ProviderError::InvalidApiKey { .. })),
        (402, |e| matches!(e, ProviderError::PaymentRequired { .. })),
        (404, |e| matches!(e, ProviderError::ModelNotFound { .. })),
        (408, |e| matches!(e, ProviderError::Timeout { .. })),
        (429, |e| matches!(e, ProviderError::RateLimit { .. })),
        (500, |e| {
            matches!(
                e,
                ProviderError::ServerError {
                    status_code: 500,
                    ..
                }
            )
        }),
        (503, |e| {
            matches!(
                e,
                ProviderError::ServerError {
                    status_code: 503,
                    ..
                }
            )
        }),
        (529, |e| {
            matches!(
                e,
                ProviderError::ServerError {
                    status_code: 529,
                    ..
                }
            )
        }),
        (400, |e| {
            matches!(
                e,
                ProviderError::ProviderFailure {
                    status_code: Some(400),
                    ..
                }
            )
        }),
        (422, |e| {
            matches!(
                e,
                ProviderError::ProviderFailure {
                    status_code: Some(422),
                    ..
                }
            )
        }),
    ];
    for (status, check) in cases {
        let server = LoopbackServer::start(vec![Canned::json(status, r#"{"error":"x"}"#)]);
        let ctx = ctx_for(&chat_route(&server.base()));
        let err = client().generate(&ctx, &request()).await.unwrap_err();
        assert!(check(&err), "status {status} mapped to {err:?}");
        assert!(
            plain_text(&err).contains("Fixture Host"),
            "status {status}: copy must use the row's display name"
        );
    }
}

#[tokio::test]
async fn rate_limit_carries_retry_after_seconds() {
    let server = LoopbackServer::start(vec![
        Canned::json(429, "{}").with_header("Retry-After", "7"),
        Canned::json(429, "{}"),
        Canned::json(429, "{}").with_header("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT"),
    ]);
    let ctx = ctx_for(&chat_route(&server.base()));

    let first = client().generate(&ctx, &request()).await.unwrap_err();
    match &first {
        ProviderError::RateLimit {
            retry_after_secs,
            plain_language,
            ..
        } => {
            assert_eq!(*retry_after_secs, Some(7));
            assert!(plain_language.contains("7 seconds"), "{plain_language}");
        }
        other => panic!("{other:?}"),
    }
    let second = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(matches!(
        second,
        ProviderError::RateLimit {
            retry_after_secs: None,
            ..
        }
    ));
    // HTTP-date form is not parsed (delta-seconds only).
    let third = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(matches!(
        third,
        ProviderError::RateLimit {
            retry_after_secs: None,
            ..
        }
    ));
}

#[tokio::test]
async fn malformed_success_bodies_are_malformed_response_errors() {
    for body in [
        "not json at all",
        "{}",
        r#"{"choices":[]}"#,
        r#"{"choices":[{"message":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"message":{"content":null},"finish_reason":"stop"}]}"#,
        r#"{"choices":[{"message":{"content":42},"finish_reason":"stop"}]}"#,
    ] {
        let server = LoopbackServer::start(vec![Canned::json(200, body)]);
        let ctx = ctx_for(&chat_route(&server.base()));
        let err = client().generate(&ctx, &request()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::MalformedResponse { .. }),
            "body {body:?} gave {err:?}"
        );
    }
}

#[tokio::test]
async fn reasoning_that_eats_the_budget_is_an_explicit_error() {
    for content in [r#""content": null"#, r#""content": """#] {
        let body = format!(
            r#"{{"choices":[{{"message":{{{content}}},"finish_reason":"length"}}],
                "usage":{{"prompt_tokens":5,"completion_tokens":1024}}}}"#
        );
        let server = LoopbackServer::start(vec![Canned::json(200, &body)]);
        let ctx = ctx_for(&chat_route(&server.base()));
        let err = client().generate(&ctx, &request()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::OutputBudgetExhausted { .. }),
            "{err:?}"
        );
    }
}

#[tokio::test]
async fn truncated_but_non_empty_output_stays_complete() {
    // items.id=696 / 716 own truncation; this item does not change it.
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}],
            "usage":{"prompt_tokens":3,"completion_tokens":512}}"#,
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let out = client().generate(&ctx, &request()).await.unwrap();
    assert_eq!(out.response.content, "partial");
    assert_eq!(out.response.completion_status, CompletionStatus::Complete);
    assert_eq!(out.finish_reason.as_deref(), Some("length"));
}

// -- transport ---------------------------------------------------------------

#[tokio::test]
async fn slow_server_times_out() {
    let server = LoopbackServer::start(vec![chat_ok().with_delay(Duration::from_millis(1500))]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let fast = HostedClient::for_loopback_tests(
        Duration::from_millis(300),
        Duration::from_millis(300),
        1024 * 1024,
    );
    let err = fast.generate(&ctx, &request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::Timeout { .. }), "{err:?}");
}

#[tokio::test]
async fn refused_connection_is_unavailable() {
    // Bind then drop to get a port nothing listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ctx = ctx_for(&chat_route(&format!("http://127.0.0.1:{port}")));
    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::Unavailable { .. }), "{err:?}");
}

#[tokio::test]
async fn oversized_response_is_refused() {
    let big = vec![b'x'; 4096];
    let server = LoopbackServer::start(vec![Canned::bytes(200, big)]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let small =
        HostedClient::for_loopback_tests(Duration::from_secs(5), Duration::from_secs(2), 1024);
    let err = small.generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::MalformedResponse { .. }),
        "{err:?}"
    );
    assert!(plain_text(&err).contains("larger"));
}

#[tokio::test]
async fn a_redirect_is_refused_and_never_followed() {
    let target = LoopbackServer::start(vec![chat_ok()]);
    let location = format!("{}/stolen", target.base());
    let origin = LoopbackServer::start(vec![
        Canned::json(302, "{}").with_header("Location", &location)
    ]);
    let ctx = ctx_for(&chat_route(&origin.base()));

    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(
            err,
            ProviderError::ProviderFailure {
                status_code: Some(302),
                ..
            }
        ),
        "{err:?}"
    );
    // Give a (wrongly) followed redirect every chance to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        target.connections(),
        0,
        "the redirect target must see nothing"
    );
    assert!(target.captured().is_empty());
    assert_eq!(origin.connections(), 1);
}

#[tokio::test]
async fn the_production_client_refuses_plain_http() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    // The URL check in build() already rejects http:// ...
    let route = chat_route(&server.base());
    assert!(matches!(
        CallContext::build(&route, SECRET, &json!({})),
        Err(ProviderError::Misconfigured { .. })
    ));
    // ... and the production client independently refuses to send one, even
    // if a context somehow carried an http URL (defense in depth).
    let ctx = ctx_for(&route);
    let err = HostedClient::new()
        .generate(&ctx, &request())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Misconfigured { .. }),
        "{err:?}"
    );
    assert_eq!(server.connections(), 0);
}

// -- variables ---------------------------------------------------------------

#[tokio::test]
async fn user_variable_lands_in_the_request_path() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.base_url_template = Some(format!("{}/accounts/{{account_id}}/v1", server.base()));
    route.user_variables = Some(json!(["account_id"]));
    let ctx = CallContext::build_for_loopback(&route, SECRET, &json!({"account_id": "abc-123_X"}))
        .unwrap();
    client().generate(&ctx, &request()).await.unwrap();
    assert_eq!(
        server.captured()[0].path,
        "/accounts/abc-123_X/v1/chat/completions"
    );
}

#[tokio::test]
async fn hostile_user_variables_never_reach_the_network() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    for bad in [
        "x.evil.test",
        "a/b",
        "..",
        "a@evil.test",
        "a:80",
        "a%2Fb",
        "a b",
        "",
    ] {
        let mut route = chat_route(&server.base());
        route.base_url_template = Some(format!("{}/accounts/{{account_id}}/v1", server.base()));
        route.user_variables = Some(json!(["account_id"]));
        let out = CallContext::build_for_loopback(&route, SECRET, &json!({"account_id": bad}));
        assert!(out.is_err(), "{bad:?}");
    }
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------
// anthropic_messages dialect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn messages_success_sends_the_expected_request_and_extracts_usage() {
    let server = LoopbackServer::start(vec![messages_ok()]);
    let ctx = ctx_for(&messages_route(&server.base()));
    let mut req = request();
    req.system = Some("be brief".to_owned());

    let out = client().generate(&ctx, &req).await.unwrap();
    assert_eq!(
        out.response.content, "hi there",
        "text blocks joined, others skipped"
    );
    assert_eq!(out.response.prompt_token_count, 21);
    assert_eq!(out.response.output_token_count, 9);
    assert_eq!(out.finish_reason.as_deref(), Some("end_turn"));

    let sent = &server.captured()[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.path, "/v1/messages");
    assert_eq!(sent.header("x-api-key"), Some(SECRET));
    assert_eq!(sent.header("authorization"), None);
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
    let body = sent.json();
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["stream"], false);
    assert_eq!(body["temperature"], 0.5);
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "hello");
    assert!(!sent.body.contains(SECRET));
}

#[tokio::test]
async fn messages_omits_system_when_there_is_none() {
    let server = LoopbackServer::start(vec![messages_ok()]);
    let ctx = ctx_for(&messages_route(&server.base()));
    client().generate(&ctx, &request()).await.unwrap();
    assert!(server.captured()[0].json().get("system").is_none());
}

#[tokio::test]
async fn messages_requires_a_token_limit_from_the_call_or_the_row() {
    let server = LoopbackServer::start(vec![messages_ok()]);
    let mut route = messages_route(&server.base());
    route.request_options = Some(json!({}));
    let ctx = ctx_for(&route);

    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Misconfigured { .. }),
        "{err:?}"
    );
    assert_eq!(server.connections(), 0);

    let mut req = request();
    req.max_output_tokens = Some(64);
    client().generate(&ctx, &req).await.unwrap();
    assert_eq!(server.captured()[0].json()["max_tokens"], 64);
}

#[tokio::test]
async fn messages_call_budget_overrides_the_row() {
    let server = LoopbackServer::start(vec![messages_ok()]);
    let ctx = ctx_for(&messages_route(&server.base()));
    let mut req = request();
    req.max_output_tokens = Some(1000);
    client().generate(&ctx, &req).await.unwrap();
    assert_eq!(server.captured()[0].json()["max_tokens"], 1000);
}

#[tokio::test]
async fn extra_headers_compose_and_may_override_the_version_header() {
    let server = LoopbackServer::start(vec![messages_ok()]);
    let mut route = messages_route(&server.base());
    route.extra_headers = Some(json!({"anthropic-version": "2030-01-01", "X-Org-Tag": "tag-1"}));
    let ctx = ctx_for(&route);
    client().generate(&ctx, &request()).await.unwrap();
    let sent = &server.captured()[0];
    assert_eq!(sent.header("anthropic-version"), Some("2030-01-01"));
    assert_eq!(sent.header("x-org-tag"), Some("tag-1"));
    assert_eq!(sent.header("x-api-key"), Some(SECRET));
}

#[tokio::test]
async fn chat_dialect_extra_headers_are_sent_and_no_version_header_is_added() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.extra_headers = Some(json!({"X-Org-Tag": "tag-2"}));
    let ctx = ctx_for(&route);
    client().generate(&ctx, &request()).await.unwrap();
    let sent = &server.captured()[0];
    assert_eq!(sent.header("x-org-tag"), Some("tag-2"));
    assert_eq!(sent.header("anthropic-version"), None);
}

#[tokio::test]
async fn auth_scheme_is_a_row_choice_independent_of_dialect() {
    // The chat dialect with x-api-key auth, and the messages dialect with Bearer.
    let server = LoopbackServer::start(vec![chat_ok(), messages_ok()]);
    let mut chat = chat_route(&server.base());
    chat.auth_scheme = Some("x-api-key".to_owned());
    client()
        .generate(&ctx_for(&chat), &request())
        .await
        .unwrap();
    let mut msg = messages_route(&server.base());
    msg.auth_scheme = Some("bearer".to_owned());
    client().generate(&ctx_for(&msg), &request()).await.unwrap();

    let sent = server.captured();
    assert_eq!(sent[0].header("x-api-key"), Some(SECRET));
    assert_eq!(sent[0].header("authorization"), None);
    assert_eq!(
        sent[1].header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(sent[1].header("x-api-key"), None);
}

#[tokio::test]
async fn messages_error_mapping_and_malformed_bodies() {
    for (status, ok) in [
        (401u16, true),
        (404, true),
        (429, true),
        (500, true),
        (529, true),
    ] {
        let server = LoopbackServer::start(vec![Canned::json(status, "{}")]);
        let ctx = ctx_for(&messages_route(&server.base()));
        let err = client().generate(&ctx, &request()).await.unwrap_err();
        let matched = match status {
            401 => matches!(err, ProviderError::InvalidApiKey { .. }),
            404 => matches!(err, ProviderError::ModelNotFound { .. }),
            429 => matches!(err, ProviderError::RateLimit { .. }),
            _ => matches!(err, ProviderError::ServerError { .. }),
        };
        assert!(ok && matched, "status {status} gave {err:?}");
    }
    for body in [
        "nope",
        "{}",
        r#"{"content":"a string"}"#,
        r#"{"content":[{"type":"tool_use"}],"stop_reason":"end_turn"}"#,
    ] {
        let server = LoopbackServer::start(vec![Canned::json(200, body)]);
        let ctx = ctx_for(&messages_route(&server.base()));
        let err = client().generate(&ctx, &request()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::MalformedResponse { .. }),
            "body {body:?} gave {err:?}"
        );
    }
}

#[tokio::test]
async fn messages_no_text_with_max_tokens_is_budget_exhausted() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"content":[],"stop_reason":"max_tokens","usage":{"input_tokens":1,"output_tokens":256}}"#,
    )]);
    let ctx = ctx_for(&messages_route(&server.base()));
    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::OutputBudgetExhausted { .. }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Model list / health check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_key_lists_model_ids_with_a_get() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"data":[{"id":"model-a"},{"id":"model-b"},{"no_id":true}]}"#,
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let ids = client().check_key(&ctx).await.unwrap();
    assert_eq!(ids, vec!["model-a", "model-b"]);

    let sent = &server.captured()[0];
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/v1/models");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert!(sent.body.is_empty());
}

#[tokio::test]
async fn check_key_accepts_the_models_name_shape_and_messages_headers() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"models":[{"name":"m1"},{"name":"m2"}]}"#,
    )]);
    let ctx = ctx_for(&messages_route(&server.base()));
    let ids = client().check_key(&ctx).await.unwrap();
    assert_eq!(ids, vec!["m1", "m2"]);
    let sent = &server.captured()[0];
    assert_eq!(sent.path, "/v1/models");
    assert_eq!(sent.header("x-api-key"), Some(SECRET));
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
}

#[tokio::test]
async fn check_key_maps_401_and_403_to_invalid_key_and_404_to_misconfigured() {
    for status in [401u16, 403] {
        let server = LoopbackServer::start(vec![Canned::json(status, "{}")]);
        let ctx = ctx_for(&chat_route(&server.base()));
        let err = client().check_key(&ctx).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::InvalidApiKey { .. }),
            "{status}: {err:?}"
        );
    }
    let server = LoopbackServer::start(vec![Canned::json(404, "{}")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let err = client().check_key(&ctx).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Misconfigured { .. }),
        "{err:?}"
    );

    let server = LoopbackServer::start(vec![Canned::json(200, "[]")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    assert!(matches!(
        client().check_key(&ctx).await.unwrap_err(),
        ProviderError::MalformedResponse { .. }
    ));
}

#[tokio::test]
async fn check_key_without_a_models_path_makes_no_request() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.models_path = None;
    let ctx = ctx_for(&route);
    assert!(matches!(
        client().check_key(&ctx).await.unwrap_err(),
        ProviderError::Misconfigured { .. }
    ));
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn list_models_reads_context_and_retirement_where_reported() {
    // Each context field name a list may use, a retirement date, a malformed
    // optional value and an entry with no id. Only the id is required.
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        r#"{"data":[
            {"id":"m-window","context_window":131072},
            {"id":"m-maxctx","max_context_length":256000,"retires_at":"2027-02-01T00:00:00Z"},
            {"id":"m-maxin","max_input_tokens":200000,"expiration_date":"2027-03-01"},
            {"id":"m-len","context_length":8192},
            {"id":"m-bare"},
            {"id":"m-bad","context_window":"big","retires_at":"soon; drop table"},
            {"id":"m-zero","context_window":0},
            {"nothing":true}
        ]}"#,
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let got = client().list_models(&ctx).await.unwrap();
    let by = |id: &str| got.iter().find(|m| m.id == id).unwrap().clone();

    assert_eq!(got.len(), 7);
    assert_eq!(by("m-window").context_tokens, Some(131_072));
    assert_eq!(by("m-maxctx").context_tokens, Some(256_000));
    assert_eq!(
        by("m-maxctx").retires_at.as_deref(),
        Some("2027-02-01T00:00:00Z")
    );
    assert_eq!(by("m-maxin").context_tokens, Some(200_000));
    assert_eq!(by("m-maxin").retires_at.as_deref(), Some("2027-03-01"));
    assert_eq!(by("m-len").context_tokens, Some(8192));
    assert_eq!(by("m-bare").context_tokens, None);
    assert_eq!(by("m-bare").retires_at, None);
    // A malformed optional value is dropped; the model is still listed.
    assert_eq!(by("m-bad").context_tokens, None);
    assert_eq!(by("m-bad").retires_at, None);
    assert_eq!(by("m-zero").context_tokens, None);

    let sent = &server.captured()[0];
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/v1/models");
}

#[tokio::test]
async fn list_models_drops_unusable_ids_duplicates_and_caps_the_list() {
    let mut entries = vec![
        json!({"id":"ok-1"}),
        json!({"id":"ok-1"}),
        json!({"id":"has space"}),
        json!({"id":"has\nnewline"}),
        json!({"id":""}),
        json!({"id":"x".repeat(201)}),
        json!({"id":"@scope/ok:2"}),
    ];
    for i in 0..(crate::providers::hosted_client::MAX_LIVE_MODELS + 10) {
        entries.push(json!({"id": format!("bulk-{i}")}));
    }
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({ "data": entries }).to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let got = client().list_models(&ctx).await.unwrap();
    assert_eq!(got.len(), crate::providers::hosted_client::MAX_LIVE_MODELS);
    assert_eq!(got[0].id, "ok-1");
    assert_eq!(got[1].id, "@scope/ok:2");
    assert!(got.iter().all(|m| !m.id.contains(' ') && !m.id.is_empty()));
}

#[tokio::test]
async fn list_models_without_a_models_path_makes_no_request() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.models_path = None;
    let ctx = ctx_for(&route);
    assert!(matches!(
        client().list_models(&ctx).await.unwrap_err(),
        ProviderError::Misconfigured { .. }
    ));
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn health_check_never_errors_and_reports_each_state() {
    // Available
    let server = LoopbackServer::start(vec![Canned::json(200, r#"{"data":[{"id":"m"}]}"#)]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let h = client().health_check(&ctx).await;
    assert_eq!(h.status, ProviderStatus::Available);
    assert_eq!(h.provider, "fixture-host");
    assert_eq!(h.available_models, vec!["m"]);
    assert!(h.error.is_none());

    // Rejected key → Degraded
    let server = LoopbackServer::start(vec![Canned::json(401, "{}")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let h = client().health_check(&ctx).await;
    assert_eq!(h.status, ProviderStatus::Degraded);
    assert_eq!(h.error.as_deref(), Some("API key rejected"));

    // Server error → Degraded
    let server = LoopbackServer::start(vec![Canned::json(500, "{}")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    assert_eq!(
        client().health_check(&ctx).await.status,
        ProviderStatus::Degraded
    );

    // Unreachable → Unavailable
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ctx = ctx_for(&chat_route(&format!("http://127.0.0.1:{port}")));
    assert_eq!(
        client().health_check(&ctx).await.status,
        ProviderStatus::Unavailable
    );

    // Timeout → Unavailable
    let server = LoopbackServer::start(vec![
        Canned::json(200, "{}").with_delay(Duration::from_millis(1500))
    ]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let fast = HostedClient::for_loopback_tests(
        Duration::from_millis(300),
        Duration::from_millis(300),
        1024,
    );
    let h = fast.health_check(&ctx).await;
    assert_eq!(h.status, ProviderStatus::Unavailable);
    assert_eq!(h.error.as_deref(), Some("health check timed out"));
}

// ---------------------------------------------------------------------------
// Key redaction (items.id=709: the key never reaches Debug, Display, an
// error or a log)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_never_appears_in_any_error_debug_or_display() {
    // Hostile server: echoes the Authorization header and the key in bodies,
    // statuses, and a Location header.
    let echo = format!("Bearer {SECRET} key={SECRET}");
    let hostile = [
        Canned::json(401, &json!({"error": echo}).to_string()),
        Canned::json(402, &json!({"error": echo}).to_string()),
        Canned::json(404, &json!({"error": echo}).to_string()),
        Canned::json(429, &json!({"error": echo}).to_string()).with_header("Retry-After", "5"),
        Canned::json(500, &json!({"error": echo}).to_string()),
        Canned::json(400, &json!({"error": echo}).to_string()),
        Canned::json(302, "{}").with_header("Location", &format!("http://127.0.0.1:1/{SECRET}")),
        Canned::json(200, &echo),
        Canned::json(
            200,
            &json!({"choices": [{"message": {"content": null},
                                  "finish_reason": "length"}], "echo": echo})
            .to_string(),
        ),
    ];
    for canned in hostile {
        for route_for in [chat_route as fn(&str) -> HostedRoute, messages_route] {
            let server = LoopbackServer::start(vec![canned.clone()]);
            let route = route_for(&server.base());
            let ctx = ctx_for(&route);
            let err = client().generate(&ctx, &request()).await.unwrap_err();
            let text = error_text(&err);
            assert!(!text.contains(SECRET), "error leaked the key: {text}");
            assert!(
                !text.contains("Bearer"),
                "error leaked auth material: {text}"
            );
            assert!(
                !text.contains("echo"),
                "error leaked upstream body text: {text}"
            );

            let health = client().health_check(&ctx).await;
            let health_text = format!("{health:?}");
            assert!(!health_text.contains(SECRET), "{health_text}");
        }
    }
}

#[tokio::test]
async fn the_key_never_appears_in_debug_output_of_the_context_or_headers() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    assert!(!format!("{ctx:?}").contains(SECRET));
    assert!(!format!("{:?}", ctx.key).contains(SECRET));
    // The composed headers mark the secret sensitive, so even a Debug of the
    // header map (which reqwest/tracing might print) hides it.
    let headers = client().headers(&ctx, true).unwrap();
    assert!(!format!("{headers:?}").contains(SECRET));
    for v in headers.values() {
        if v.is_sensitive() {
            assert!(!format!("{v:?}").contains(SECRET));
        }
    }
    let messages = ctx_for(&messages_route(&server.base()));
    assert!(!format!("{:?}", client().headers(&messages, true).unwrap()).contains(SECRET));
    // The client itself holds no key.
    assert!(!format!("{:?}", ctx_for(&chat_route(&server.base()))).contains(SECRET));
}

#[tokio::test]
async fn a_key_that_cannot_be_a_header_value_is_refused_without_echoing_it() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let route = chat_route(&server.base());
    let bad = "sk-bad\nX-Injected: 1";
    let ctx = CallContext::build_for_loopback(&route, bad, &json!({})).unwrap();
    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Misconfigured { .. }),
        "{err:?}"
    );
    assert!(!error_text(&err).contains("sk-bad"));
    assert_eq!(server.connections(), 0);
}

// ---------------------------------------------------------------------------
// Source scans (items.id=709): no certificate-verification bypass anywhere
// in non-test code, and the production constructor is HTTPS-only with
// redirects off.
// ---------------------------------------------------------------------------

fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_source_disables_certificate_verification() {
    // Needles are assembled so this file does not contain them contiguously.
    let needles = [
        concat!("danger", "_accept"),
        concat!("https_only", "(false)"),
        concat!("danger", "ous_configuration"),
        concat!("add_root", "_certificate"),
        concat!("tls_", "danger"),
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    assert!(files.len() > 20, "scan must actually find the source tree");

    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        for needle in needles {
            if text.contains(needle) {
                offenders.push(format!("{}: {needle}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "certificate verification must never be relaxed: {offenders:?}"
    );
}

#[test]
fn production_constructor_is_https_only_with_redirects_off() {
    let source = include_str!("hosted_client.rs");
    let ctor = source
        .split("pub fn new() -> Self {")
        .nth(1)
        .and_then(|rest| rest.split("\n    }\n").next())
        .expect("find HostedClient::new");
    assert!(ctor.contains("https_only(true)"));
    assert!(ctor.contains("redirect::Policy::none()"));
    assert!(ctor.contains("connect_timeout"));
    assert!(
        !ctor.contains("no_proxy"),
        "production honors proxy env vars"
    );
}

// ---------------------------------------------------------------------------
// LIVE CHECK (items.id=709, absorbing items.id=703). #[ignore]: never runs in
// the gate. One real call per run, started by hand.
//
// TEST-ONLY ENVIRONMENT READ. This test reads QR_LIVE_KEY so a developer can
// enter a key in their own terminal without it passing through a chat, a
// command line or a log. It is NOT the retired GROQ_API_KEY/MISTRAL_API_KEY
// bridge: no product code path reads any environment variable for a key (keys
// come only from integration_keys), and this read exists only in this ignored
// test.
//
//   read -rs QR_LIVE_KEY; export QR_LIVE_KEY
//   QR_LIVE_PROVIDER=<provider row id> QR_LIVE_MAX_TOKENS=<n> \
//     [QR_LIVE_LIMIT_KEY=max_completion_tokens|max_tokens] \
//     [QR_LIVE_EFFORT=low|medium|high] \
//     cargo test --lib live_single_call -- --ignored --nocapture
//
// It prints ONLY: HTTP status, finish reason, prompt/completion/reasoning
// token counts. Never the response text. It asserts that nothing it printed
// contains any part of the key.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "live network call: needs QR_LIVE_KEY, QR_LIVE_PROVIDER, QR_LIVE_MAX_TOKENS"]
async fn live_single_call() {
    // Fixed, neutral, no personal data.
    const PROMPT: &str = "A farmer has 17 sheep. All but 9 run away, then he buys twice as \
        many as remain. How many sheep does he have now? Explain your reasoning step by \
        step in about 150 words.";

    let key = std::env::var("QR_LIVE_KEY").expect("QR_LIVE_KEY not set");
    let provider_id = std::env::var("QR_LIVE_PROVIDER").expect("QR_LIVE_PROVIDER not set");
    let budget: u32 = std::env::var("QR_LIVE_MAX_TOKENS")
        .expect("QR_LIVE_MAX_TOKENS not set")
        .parse()
        .expect("QR_LIVE_MAX_TOKENS must be a number");
    let limit_key = std::env::var("QR_LIVE_LIMIT_KEY").ok();
    let effort = std::env::var("QR_LIVE_EFFORT").ok();

    // Provider and model rows come from the migrated catalog, like production.
    let _lock = crate::test_support::ENV_MUTEX.lock().await;
    let saved_root = std::env::var("QR_DATA_ROOT").ok();
    let tempdir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("QR_DATA_ROOT", tempdir.path());
    crate::persistence::migrations::migrate_shared_db()
        .await
        .expect("shared.db migration");
    let pool =
        sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
            &crate::providers::utils::db_path_shared(),
        ))
        .await
        .expect("shared.db pool");
    let provider = crate::persistence::provider_store::get_provider(&pool, &provider_id)
        .await
        .expect("provider lookup")
        .expect("provider row must exist");
    let model = crate::persistence::provider_store::get_default_model(&pool, &provider_id)
        .await
        .expect("model lookup")
        .expect("default model row must exist");
    match saved_root {
        Some(v) => std::env::set_var("QR_DATA_ROOT", v),
        None => std::env::remove_var("QR_DATA_ROOT"),
    }

    let mut route = HostedRoute::from_rows(&provider, &model);
    let mut options = route
        .request_options
        .clone()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let mut request = HostedRequest {
        system: None,
        prompt: PROMPT.to_owned(),
        temperature: Some(0.5),
        max_output_tokens: None,
    };
    match &limit_key {
        Some(k) => {
            options.insert(k.clone(), json!(budget));
        }
        None => request.max_output_tokens = Some(budget),
    }
    if let Some(e) = &effort {
        options.insert("reasoning_effort".to_owned(), json!(e));
    }
    route.request_options = Some(Value::Object(options));

    let mut printed: Vec<String> = Vec::new();
    printed.push(format!(
        "budget={budget} limit_key={limit_key:?} effort={effort:?}"
    ));

    match CallContext::build(&route, &key, &json!({})) {
        Err(e) => printed.push(format!("build_error={e}")),
        Ok(ctx) => match HostedClient::new().generate(&ctx, &request).await {
            Ok(out) => {
                printed.push("http_status=200".to_owned());
                printed.push(format!("finish_reason={:?}", out.finish_reason));
                printed.push(format!("prompt_tokens={}", out.response.prompt_token_count));
                printed.push(format!(
                    "completion_tokens={}",
                    out.response.output_token_count
                ));
                printed.push(format!("reasoning_tokens={:?}", out.reasoning_tokens));
                printed.push(format!(
                    "answer_chars={}",
                    out.response.content.chars().count()
                ));
            }
            Err(e) => {
                let status = match &e {
                    ProviderError::ProviderFailure { status_code, .. } => {
                        format!("{status_code:?}")
                    }
                    ProviderError::ServerError { status_code, .. } => format!("{status_code}"),
                    _ => "n/a".to_owned(),
                };
                printed.push(format!("http_status={status}"));
                printed.push(format!("error={e}"));
            }
        },
    }

    // Nothing printed may contain any part of the key.
    let windows: Vec<&str> = if key.len() >= 8 {
        (0..=key.len() - 8)
            .filter(|i| key.is_char_boundary(*i) && key.is_char_boundary(i + 8))
            .map(|i| &key[i..i + 8])
            .collect()
    } else {
        vec![key.as_str()]
    };
    for line in &printed {
        assert!(
            !line.contains(&key) && !windows.iter().any(|w| line.contains(w)),
            "a printed line contained part of the key"
        );
    }
    for line in &printed {
        println!("LIVE {line}");
    }
}

// ---------------------------------------------------------------------------
// Metering (items.id=710): usage on every outcome, rate-limit headers, and
// whether a request reached the provider.
// ---------------------------------------------------------------------------

fn chat_budget_exhausted() -> Canned {
    Canned::json(
        200,
        &json!({
            "choices": [{"message": {"role": "assistant", "content": ""},
                         "finish_reason": "length"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4096,
                      "completion_tokens_details": {"reasoning_tokens": 4096}}
        })
        .to_string(),
    )
}

#[tokio::test]
async fn a_budget_exhausted_call_still_reports_the_tokens_it_burned() {
    let server = LoopbackServer::start(vec![chat_budget_exhausted()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(matches!(
        a.result,
        Err(ProviderError::OutputBudgetExhausted { .. })
    ));
    assert_eq!(
        a.usage,
        Some(HostedUsage {
            prompt_tokens: 10,
            completion_tokens: 4096,
            reasoning_tokens: Some(4096)
        })
    );
    assert!(a.reached_provider);
    assert_eq!(a.http_status, Some(200));
    // The unmetered entry point is unchanged: still an error, no usage.
    let server = LoopbackServer::start(vec![chat_budget_exhausted()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let err = client().generate(&ctx, &request()).await.unwrap_err();
    assert!(matches!(err, ProviderError::OutputBudgetExhausted { .. }));
}

#[tokio::test]
async fn the_messages_dialect_reports_usage_on_budget_exhaustion_too() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({
            "type": "message", "content": [],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 30, "output_tokens": 256}
        })
        .to_string(),
    )]);
    let ctx = ctx_for(&messages_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(matches!(
        a.result,
        Err(ProviderError::OutputBudgetExhausted { .. })
    ));
    assert_eq!(
        a.usage,
        Some(HostedUsage {
            prompt_tokens: 30,
            completion_tokens: 256,
            reasoning_tokens: None
        })
    );
}

#[tokio::test]
async fn a_truncated_non_empty_answer_is_flagged_and_still_complete() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({
            "choices": [{"message": {"content": "cut off mid-sen"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 512}
        })
        .to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    let out = a.result.expect("a truncated answer is still an answer");
    assert!(out.truncated);
    assert_eq!(out.response.output_token_count, 512);
    assert_eq!(a.usage.unwrap().completion_tokens, 512);

    let server = LoopbackServer::start(vec![chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    assert!(!client().generate(&ctx, &request()).await.unwrap().truncated);

    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({
            "content": [{"type": "text", "text": "cut"}],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 1, "output_tokens": 256}
        })
        .to_string(),
    )]);
    let ctx = ctx_for(&messages_route(&server.base()));
    assert!(client().generate(&ctx, &request()).await.unwrap().truncated);
}

#[tokio::test]
async fn a_malformed_answer_that_carried_usage_still_reports_it() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3}}).to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(matches!(
        a.result,
        Err(ProviderError::MalformedResponse { .. })
    ));
    assert_eq!(a.usage.unwrap().prompt_tokens, 12);
    assert!(a.reached_provider);
}

#[tokio::test]
async fn a_response_with_no_usage_object_reports_none() {
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]}).to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(a.result.is_ok());
    assert_eq!(a.usage, None);
    // An empty usage object is no usage either.
    let server = LoopbackServer::start(vec![Canned::json(
        200,
        &json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}], "usage": {}})
            .to_string(),
    )]);
    let ctx = ctx_for(&chat_route(&server.base()));
    assert_eq!(
        client().generate_metered(&ctx, &request()).await.usage,
        None
    );
}

#[tokio::test]
async fn invalid_json_reached_the_provider_with_no_usage() {
    let server = LoopbackServer::start(vec![Canned::json(200, "not json at all")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(matches!(
        a.result,
        Err(ProviderError::MalformedResponse { .. })
    ));
    assert_eq!(a.usage, None);
    assert!(a.reached_provider);
    assert_eq!(a.http_status, Some(200));
}

#[tokio::test]
async fn error_statuses_count_as_requests_with_their_headers_and_no_usage() {
    for status in [429u16, 500, 503, 401, 402, 404] {
        let server = LoopbackServer::start(vec![Canned::json(status, "{}")
            .with_header("x-ratelimit-remaining-requests", "0")
            .with_header("Retry-After", "7")]);
        let ctx = ctx_for(&chat_route(&server.base()));
        let a = client().generate_metered(&ctx, &request()).await;
        assert!(a.result.is_err(), "{status}");
        assert_eq!(a.usage, None, "{status}");
        assert!(a.reached_provider, "{status}");
        assert_eq!(a.http_status, Some(status));
        assert_eq!(a.rate_limit.remaining_requests(), Some(0), "{status}");
    }
}

#[tokio::test]
async fn a_429_hands_retry_after_to_the_error() {
    let server =
        LoopbackServer::start(vec![Canned::json(429, "{}").with_header("Retry-After", "7")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    match a.result {
        Err(ProviderError::RateLimit {
            retry_after_secs, ..
        }) => assert_eq!(retry_after_secs, Some(7)),
        other => panic!("expected a rate limit, got {other:?}"),
    }
    // And it survives the conversion the executor's retry loop sees.
    let server =
        LoopbackServer::start(vec![Canned::json(429, "{}").with_header("Retry-After", "7")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let err = client().generate(&ctx, &request()).await.unwrap_err();
    match crate::conductor::failure::ConductorError::from(err) {
        crate::conductor::failure::ConductorError::ProviderRateLimit {
            retry_after_secs, ..
        } => assert_eq!(retry_after_secs, Some(7)),
        other => panic!("expected ProviderRateLimit, got {other:?}"),
    }
}

#[tokio::test]
async fn a_connection_that_never_opened_did_not_reach_the_provider() {
    // Bind a port and release it, so the connection is refused.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let ctx = ctx_for(&chat_route(&format!("http://127.0.0.1:{port}")));
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(a.result.is_err());
    assert!(!a.reached_provider, "a refused connection is not a request");
    assert_eq!(a.http_status, None);
    assert_eq!(a.usage, None);
}

#[tokio::test]
async fn a_read_timeout_after_sending_may_have_reached_the_provider() {
    let server = LoopbackServer::start(vec![chat_ok().with_delay(Duration::from_millis(1500))]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let slow = HostedClient::for_loopback_tests(
        Duration::from_millis(250),
        Duration::from_secs(2),
        1024 * 1024,
    );
    let a = slow.generate_metered(&ctx, &request()).await;
    assert!(matches!(a.result, Err(ProviderError::Timeout { .. })));
    assert!(
        a.reached_provider,
        "the request was sent before it timed out"
    );
    assert_eq!(a.http_status, None);
    assert_eq!(a.usage, None);
}

#[tokio::test]
async fn a_misconfigured_call_makes_no_request_and_is_not_counted() {
    let server = LoopbackServer::start(vec![chat_ok()]);
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({}));
    let ctx = ctx_for(&route);
    let a = client().generate_metered(&ctx, &request()).await;
    assert!(matches!(a.result, Err(ProviderError::Misconfigured { .. })));
    assert!(!a.reached_provider);
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn the_output_limit_helper_matches_what_is_sent() {
    let server = LoopbackServer::start(vec![chat_ok(), chat_ok()]);
    let ctx = ctx_for(&chat_route(&server.base()));
    // Row limit.
    assert_eq!(effective_output_limit(&ctx, &request()), Some(512));
    // A call-supplied budget wins.
    let mut req = request();
    req.max_output_tokens = Some(64);
    assert_eq!(effective_output_limit(&ctx, &req), Some(64));
    client().generate(&ctx, &req).await.unwrap();
    assert_eq!(
        server.captured()[0].json()["max_completion_tokens"],
        64,
        "the cap is computed from exactly the limit that is sent"
    );
    // Zero is not a limit.
    req.max_output_tokens = Some(0);
    assert_eq!(effective_output_limit(&ctx, &req), Some(512));
    // No limit anywhere.
    let mut route = chat_route(&server.base());
    route.request_options = Some(json!({}));
    assert_eq!(effective_output_limit(&ctx_for(&route), &request()), None);
}

#[tokio::test]
async fn a_successful_outcome_carries_the_rate_limit_headers() {
    let server = LoopbackServer::start(vec![chat_ok()
        .with_header("x-ratelimit-remaining-requests", "41")
        .with_header("x-ratelimit-limit-requests", "50")
        .with_header("x-ratelimit-remaining-tokens", "9000")
        .with_header("x-ratelimit-limit-tokens", "10000")
        .with_header("x-ratelimit-reset-tokens", "6s")
        .with_header("x-unrelated", "ignored")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let out = client().generate(&ctx, &request()).await.unwrap();
    let r = &out.rate_limit;
    assert_eq!(r.remaining_requests(), Some(41));
    assert_eq!(r.limit_requests(), Some(50));
    assert_eq!(r.remaining_tokens(), Some(9000));
    assert_eq!(r.limit_tokens(), Some(10_000));
    assert_eq!(r.reset_tokens().as_deref(), Some("6s"));
    assert!(r.entries().iter().all(|(k, _)| k != "x-unrelated"));
}

#[test]
fn rate_limit_quantities_are_found_by_the_words_in_their_names() {
    // A differently ordered naming scheme, plus several token windows: the
    // smallest remaining is the conservative reading.
    let info = RateLimitInfo::from_pairs(&[
        ("vendor-ratelimit-requests-remaining", "12"),
        ("vendor-ratelimit-requests-limit", "60"),
        ("vendor-ratelimit-requests-reset", "2026-10-11T18:00:00Z"),
        ("vendor-ratelimit-input-tokens-remaining", "800"),
        ("vendor-ratelimit-output-tokens-remaining", "300"),
        ("vendor-ratelimit-tokens-remaining", "1000"),
        ("vendor-ratelimit-tokens-limit", "5000"),
        ("retry-after", "3"),
    ]);
    assert_eq!(info.remaining_requests(), Some(12));
    assert_eq!(info.limit_requests(), Some(60));
    assert_eq!(info.remaining_tokens(), Some(300));
    assert_eq!(info.limit_tokens(), Some(5000));
    assert_eq!(
        info.reset_requests().as_deref(),
        Some("2026-10-11T18:00:00Z")
    );
    assert_eq!(info.reset_tokens(), None);
    // Retry-After is kept as an entry, not mistaken for a quantity.
    assert!(info.entries().iter().any(|(k, _)| k == "retry-after"));
}

#[test]
fn rate_limit_capture_is_bounded_and_ignores_hostile_values() {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    // Too long a value is dropped.
    h.insert(
        HeaderName::from_static("x-ratelimit-remaining-requests"),
        HeaderValue::from_str(&"9".repeat(200)).unwrap(),
    );
    // A non-numeric value is kept as text but never read as a count.
    h.insert(
        HeaderName::from_static("x-ratelimit-remaining-tokens"),
        HeaderValue::from_static("lots"),
    );
    // A value with bytes outside visible ASCII is dropped.
    h.insert(
        HeaderName::from_static("x-ratelimit-reset-requests"),
        HeaderValue::from_bytes(b"ab\xffcd").unwrap(),
    );
    // An empty value is dropped.
    h.insert(
        HeaderName::from_static("x-ratelimit-limit-requests"),
        HeaderValue::from_static(""),
    );
    let info = RateLimitInfo::from_headers(&h);
    assert_eq!(info.remaining_requests(), None);
    assert_eq!(info.remaining_tokens(), None);
    assert_eq!(info.reset_requests(), None);
    assert_eq!(info.limit_requests(), None);
    assert_eq!(info.entries().len(), 1);

    // The number of captured headers is capped.
    let mut many = HeaderMap::new();
    for i in 0..100 {
        many.insert(
            HeaderName::from_bytes(format!("x-ratelimit-extra-{i}").as_bytes()).unwrap(),
            HeaderValue::from_static("1"),
        );
    }
    assert_eq!(RateLimitInfo::from_headers(&many).entries().len(), 32);
}

#[tokio::test]
async fn rate_limit_values_never_leak_into_error_text() {
    let server = LoopbackServer::start(vec![Canned::json(429, "{}")
        .with_header("x-ratelimit-remaining-requests", "SECRET-LOOKING-TEXT")
        .with_header("Retry-After", "5")]);
    let ctx = ctx_for(&chat_route(&server.base()));
    let a = client().generate_metered(&ctx, &request()).await;
    let text = error_text(&a.result.unwrap_err());
    assert!(!text.contains("SECRET-LOOKING-TEXT"));
}
