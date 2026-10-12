// src-tauri/src/providers/hosted_call.rs
//
// items.id=709 (decisions.id=875, PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md
// Part 8b): the call context for the generic hosted client. Everything a
// request needs is resolved here, once, from three inputs: the provider row,
// the model row and the user's stored key record. Nothing in this module (or
// in hosted_client.rs) names a provider or a model (decisions.id=872).
//
// This module is PURE: no network, no database. All validation of row data
// happens in `CallContext::build`, so a bad row fails before any traffic:
//   - the wire format (`api_dialect`) must be one the code implements;
//   - the base URL must be https, and user variables (an account id, say)
//     may be substituted ONLY into path segments, after a strict character
//     check, so a variable can never change the host;
//   - `extra_headers` may not touch authentication, host or framing headers;
//   - `request_options` must be a JSON object and may not override `model`,
//     `messages`, `stream` (or `system` on the Messages dialect).
//
// CONTRACT carried over from the retired QrHostedProvider trait:
//   - Prompts reaching a hosted provider contain abstracted values only; the
//     disclosure buffer (StepExecutor Step 8) guarantees it upstream, and this
//     layer performs no privacy validation.
//   - The disclosure log is the executor's job, never this layer's.
//   - Callers never see a raw reqwest error: everything maps to ProviderError.
//
// SECURITY: the key lives in `ApiKey`, a zeroizing newtype whose Debug prints
// a fixed marker and which has no Display. `CallContext` derives Debug only
// because its key field cannot print.

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::persistence::provider_store::{ModelStatus, Provider, ProviderModel};
use crate::providers::errors::ProviderError;

// ---------------------------------------------------------------------------
// Wire formats
// ---------------------------------------------------------------------------

/// Wire format of a hosted provider. Named by protocol, never by provider
/// (spec 8a rule 1). Further dialects are added when a curated row needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// POST `{base}/chat/completions`, Bearer auth by default.
    OpenaiChat,
    /// POST `{base}/v1/messages`, `x-api-key` auth by default, `max_tokens`
    /// required, system prompt as a top-level field.
    AnthropicMessages,
}

impl Dialect {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "openai_chat" => Some(Self::OpenaiChat),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
        }
    }

    /// Path appended to the base URL for a generation request.
    pub(crate) fn generate_path(self) -> &'static str {
        match self {
            Self::OpenaiChat => "/chat/completions",
            Self::AnthropicMessages => "/v1/messages",
        }
    }

    fn default_auth(self) -> AuthScheme {
        match self {
            Self::OpenaiChat => AuthScheme::Bearer,
            Self::AnthropicMessages => AuthScheme::XApiKey,
        }
    }
}

/// How the key is presented. `providers.auth_scheme` is open TEXT in the
/// schema (shared_026.sql), validated here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`
    XApiKey,
}

impl AuthScheme {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "bearer" => Some(Self::Bearer),
            "x-api-key" => Some(Self::XApiKey),
            _ => None,
        }
    }
}

/// Protocol revision header the Messages wire format requires. A property of
/// the wire format, not of any provider row, so it is a dialect constant; a
/// row may override it through `extra_headers`.
pub(crate) const MESSAGES_DIALECT_VERSION_HEADER: &str = "anthropic-version";
pub(crate) const MESSAGES_DIALECT_VERSION_DEFAULT: &str = "2023-06-01";

/// Token-limit key sent when the call supplies a budget but the row names no
/// key. Every OpenAI-compatible server accepts it; rows for models that
/// reject it (reasoning models) carry `max_completion_tokens` instead.
const DEFAULT_OPENAI_LIMIT_KEY: &str = "max_tokens";
const OPENAI_LIMIT_KEYS: [&str; 2] = ["max_completion_tokens", "max_tokens"];

/// Option keys a row may never set (the client owns them).
const FORBIDDEN_OPTION_KEYS: [&str; 3] = ["model", "messages", "stream"];

/// Header names a row may never set.
const FORBIDDEN_HEADER_NAMES: [&str; 8] = [
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "authorization",
    "x-api-key",
    "cookie",
];

const MAX_VARIABLE_LEN: usize = 64;
const MAX_VARIABLE_NAME_LEN: usize = 32;

// ---------------------------------------------------------------------------
// ApiKey
// ---------------------------------------------------------------------------

/// The user's API key. Zeroized on drop; Debug prints a fixed marker; there is
/// no Display and no Serialize. The only way to read it is `expose()`, which
/// only the client's header composition calls.
pub struct ApiKey(Zeroizing<String>);

impl ApiKey {
    pub fn new(raw: &str) -> Self {
        Self(Zeroizing::new(raw.trim().to_owned()))
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.as_str()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

// ---------------------------------------------------------------------------
// Route: the plain-data view of the rows (unit-testable without a database)
// ---------------------------------------------------------------------------

/// The fields of a provider row and a model row the client needs.
/// `from_rows` is the only production constructor.
#[derive(Debug, Clone)]
pub struct HostedRoute {
    pub provider_id: String,
    pub display_name: String,
    pub api_dialect: Option<String>,
    pub base_url_template: Option<String>,
    pub auth_scheme: Option<String>,
    pub extra_headers: Option<Value>,
    pub models_path: Option<String>,
    pub user_variables: Option<Value>,
    pub model_id: String,
    pub model_status: ModelStatus,
    pub request_options: Option<Value>,
    /// The provider's `required_request_options` (items.id=711): merged OVER
    /// `request_options` on every call; a conflicting model row is
    /// Misconfigured.
    pub required_request_options: Option<Value>,
}

impl HostedRoute {
    pub fn from_rows(provider: &Provider, model: &ProviderModel) -> Self {
        Self {
            provider_id: provider.id.clone(),
            display_name: provider.display_name.clone(),
            api_dialect: provider.api_dialect.clone(),
            base_url_template: provider.base_url_template.clone(),
            auth_scheme: provider.auth_scheme.clone(),
            extra_headers: provider.extra_headers.clone(),
            models_path: provider.models_path.clone(),
            user_variables: provider.user_variables.clone(),
            model_id: model.model_id.clone(),
            model_status: model.status,
            request_options: model.request_options.clone(),
            required_request_options: provider.required_request_options.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// CallContext
// ---------------------------------------------------------------------------

/// Everything one hosted call needs, validated. Built by `build`; consumed by
/// `HostedClient`. Holds the key, so it is built per call and dropped after.
#[derive(Debug)]
pub struct CallContext {
    pub provider_id: String,
    pub display_name: String,
    pub model_id: String,
    pub(crate) dialect: Dialect,
    pub(crate) auth: AuthScheme,
    /// Base URL with variables substituted, no trailing slash, https.
    base: String,
    models_path: Option<String>,
    pub(crate) extra_headers: Vec<(HeaderName, HeaderValue)>,
    /// Validated `request_options` (a JSON object; forbidden keys absent).
    pub(crate) options: Map<String, Value>,
    pub(crate) key: ApiKey,
}

impl CallContext {
    /// Build the call context. Fails with a `ProviderError` (never touching
    /// the network) when any row field is unusable or unsafe.
    ///
    /// `key_metadata` is the key record's `extra_metadata` (a JSON object);
    /// user variables named by the provider's `user_variables` are read from it.
    pub fn build(
        route: &HostedRoute,
        credential: &str,
        key_metadata: &Value,
    ) -> Result<Self, ProviderError> {
        Self::build_inner(route, credential, key_metadata, false)
    }

    /// Test-only: like `build`, but accepts `http://127.0.0.1` so the loopback
    /// servers can stand in for a provider. Does not exist in non-test builds.
    #[cfg(test)]
    pub(crate) fn build_for_loopback(
        route: &HostedRoute,
        credential: &str,
        key_metadata: &Value,
    ) -> Result<Self, ProviderError> {
        Self::build_inner(route, credential, key_metadata, true)
    }

    fn build_inner(
        route: &HostedRoute,
        credential: &str,
        key_metadata: &Value,
        allow_loopback_http: bool,
    ) -> Result<Self, ProviderError> {
        let fail = |detail: &str| misconfigured(route, detail);

        if route.model_status == ModelStatus::Retired {
            return Err(ProviderError::ModelNotFound {
                provider: route.provider_id.clone(),
                plain_language: format!(
                    "{} has retired the model this is set to use. \
                     Choose another model in Settings. [Open Settings] [Get help]",
                    route.display_name
                ),
            });
        }

        let dialect = route
            .api_dialect
            .as_deref()
            .and_then(Dialect::parse)
            .ok_or_else(|| fail("it uses a connection format this version does not support"))?;

        let auth = match route.auth_scheme.as_deref() {
            None => dialect.default_auth(),
            Some(raw) => AuthScheme::parse(raw)
                .ok_or_else(|| fail("it uses a sign-in method this version does not support"))?,
        };

        let key = ApiKey::new(credential);
        if key.is_empty() {
            return Err(missing_key(route));
        }

        let template = route
            .base_url_template
            .as_deref()
            .ok_or_else(|| fail("it has no service address"))?;
        let declared = declared_variables(route)?;
        let base = resolve_base_url(
            route,
            template,
            &declared,
            key_metadata,
            allow_loopback_http,
        )?;

        let models_path = match route.models_path.as_deref() {
            None => None,
            Some(p) => {
                validate_path(p).map_err(|why| fail(&format!("its model list path {why}")))?;
                Some(p.to_owned())
            }
        };

        let extra_headers = parse_extra_headers(route)?;
        let options = parse_options(route, dialect)?;

        Ok(Self {
            provider_id: route.provider_id.clone(),
            display_name: route.display_name.clone(),
            model_id: route.model_id.clone(),
            dialect,
            auth,
            base,
            models_path,
            extra_headers,
            options,
            key,
        })
    }

    /// URL for a generation request.
    pub(crate) fn generate_url(&self) -> String {
        format!("{}{}", self.base, self.dialect.generate_path())
    }

    /// URL for the model list, if the row has one.
    pub(crate) fn models_url(&self) -> Option<String> {
        self.models_path
            .as_ref()
            .map(|p| format!("{}{}", self.base, p))
    }

    /// Host and scheme of the resolved base, for tests asserting that a
    /// variable never moved the request to another host.
    #[cfg(test)]
    pub(crate) fn base_url(&self) -> &str {
        &self.base
    }

    /// Token-limit key and the row's own value for it (None when the row
    /// carries no limit). Misconfigured rows (both keys) were rejected in build.
    pub(crate) fn row_token_limit(&self) -> Option<(&'static str, u64)> {
        let keys: &[&'static str] = match self.dialect {
            Dialect::OpenaiChat => &OPENAI_LIMIT_KEYS,
            Dialect::AnthropicMessages => &["max_tokens"],
        };
        keys.iter().find_map(|k| {
            self.options
                .get(*k)
                .and_then(Value::as_u64)
                .map(|v| (*k, v))
        })
    }

    /// The key name a call-supplied budget is sent under.
    pub(crate) fn limit_key_for_call(&self) -> &'static str {
        match self.dialect {
            Dialect::AnthropicMessages => "max_tokens",
            Dialect::OpenaiChat => self
                .row_token_limit()
                .map(|(k, _)| k)
                .unwrap_or(DEFAULT_OPENAI_LIMIT_KEY),
        }
    }
}

// ---------------------------------------------------------------------------
// Error constructors (display name from the row, never a literal)
// ---------------------------------------------------------------------------

pub(crate) fn misconfigured(route: &HostedRoute, detail: &str) -> ProviderError {
    ProviderError::Misconfigured {
        provider: route.provider_id.clone(),
        plain_language: format!(
            "{} can't be used because {detail}. [Get help] [Use local AI instead]",
            route.display_name
        ),
    }
}

pub(crate) fn missing_key(route: &HostedRoute) -> ProviderError {
    ProviderError::MissingApiKey {
        provider: route.provider_id.clone(),
        plain_language: format!(
            "No {} API key is saved. Add one in Settings to use it. \
             [Open Settings] [Use local AI instead]",
            route.display_name
        ),
    }
}

fn missing_variable(route: &HostedRoute, name: &str) -> ProviderError {
    ProviderError::MissingApiKey {
        provider: route.provider_id.clone(),
        plain_language: format!(
            "{} needs one more detail ({name}) before it can be used. \
             Finish connecting it in Settings. [Open Settings] [Use local AI instead]",
            route.display_name
        ),
    }
}

// ---------------------------------------------------------------------------
// Base URL and user variables
// ---------------------------------------------------------------------------

fn is_valid_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.len() <= MAX_VARIABLE_NAME_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A user-variable VALUE: `^[A-Za-z0-9_-]{1,64}$`. No dots (no `..`, no host
/// labels), slashes, `@`, `:`, `%`, `?`, `#`, whitespace or non-ASCII.
pub(crate) fn is_valid_variable_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VARIABLE_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn declared_variables(route: &HostedRoute) -> Result<Vec<String>, ProviderError> {
    match &route.user_variables {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut names = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(n) if is_valid_variable_name(n) => names.push(n.to_owned()),
                    _ => return Err(misconfigured(route, "its variable list is invalid")),
                }
            }
            Ok(names)
        }
        Some(_) => Err(misconfigured(route, "its variable list is invalid")),
    }
}

/// Substitute `{name}` placeholders into the PATH of the template and return
/// the base URL without a trailing slash.
fn resolve_base_url(
    route: &HostedRoute,
    template: &str,
    declared: &[String],
    key_metadata: &Value,
    allow_loopback_http: bool,
) -> Result<String, ProviderError> {
    let fail = |detail: &str| misconfigured(route, detail);

    let (scheme, rest) = if let Some(rest) = template.strip_prefix("https://") {
        ("https://", rest)
    } else if allow_loopback_http && template.starts_with("http://127.0.0.1") {
        ("http://", &template["http://".len()..])
    } else {
        return Err(fail("its service address is not a secure (https) address"));
    };

    if template.contains('?') || template.contains('#') {
        return Err(fail("its service address carries a query or fragment"));
    }

    // The authority (host and port) is everything before the first slash. It
    // may hold no placeholder and no userinfo: a variable must never be able
    // to move the request to another host.
    let (authority, path_template) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty()
        || authority.contains(['{', '}', '@', '\\', ' '])
        || authority.chars().any(char::is_control)
    {
        return Err(fail("its service address is not valid"));
    }

    // Substitute placeholders in the path part only.
    let mut path = String::with_capacity(path_template.len());
    let mut chars = path_template.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for n in chars.by_ref() {
                    if n == '}' {
                        closed = true;
                        break;
                    }
                    name.push(n);
                }
                if !closed || !is_valid_variable_name(&name) {
                    return Err(fail("its service address has a malformed placeholder"));
                }
                if !declared.iter().any(|d| d == &name) {
                    return Err(fail("its service address uses an undeclared placeholder"));
                }
                let value = match key_metadata.get(&name) {
                    None | Some(Value::Null) => return Err(missing_variable(route, &name)),
                    Some(Value::String(s)) => s.as_str(),
                    Some(_) => return Err(fail("a saved detail is not valid text")),
                };
                if !is_valid_variable_value(value) {
                    return Err(fail(
                        "a saved detail contains characters that are not allowed",
                    ));
                }
                path.push_str(value);
            }
            '}' => return Err(fail("its service address has a malformed placeholder")),
            other => path.push(other),
        }
    }

    let assembled = format!("{scheme}{authority}{path}");
    let parsed =
        Url::parse(&assembled).map_err(|_| fail("its service address is not a valid address"))?;

    // Defense in depth: the parsed host/port must equal the template's own
    // authority, and the URL must have no userinfo, query or fragment.
    let template_origin = Url::parse(&format!("{scheme}{authority}"))
        .map_err(|_| fail("its service address is not a valid address"))?;
    if parsed.host_str() != template_origin.host_str()
        || parsed.port_or_known_default() != template_origin.port_or_known_default()
        || parsed.scheme() != template_origin.scheme()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(fail("its service address changed while being assembled"));
    }

    Ok(assembled.trim_end_matches('/').to_owned())
}

/// A relative request path: starts with `/`, only `[A-Za-z0-9/_.-]`, no `..`,
/// no `//`. Returns the reason on failure.
fn validate_path(p: &str) -> Result<(), &'static str> {
    if !p.starts_with('/') {
        return Err("does not start with a slash");
    }
    if p.contains("..") || p.contains("//") {
        return Err("is not a plain path");
    }
    if !p
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '.' | '-'))
    {
        return Err("contains characters that are not allowed");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Headers and options
// ---------------------------------------------------------------------------

fn parse_extra_headers(
    route: &HostedRoute,
) -> Result<Vec<(HeaderName, HeaderValue)>, ProviderError> {
    let fail = |detail: &str| misconfigured(route, detail);
    let obj = match &route.extra_headers {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(o)) => o,
        Some(_) => return Err(fail("its extra headers are not a set of name/value pairs")),
    };
    let mut out = Vec::with_capacity(obj.len());
    for (name, value) in obj {
        let lower = name.to_ascii_lowercase();
        if FORBIDDEN_HEADER_NAMES.contains(&lower.as_str()) || lower.starts_with("proxy-") {
            return Err(fail("its extra headers include one that is not allowed"));
        }
        let header_name = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| fail("its extra headers include an invalid name"))?;
        let text = value
            .as_str()
            .ok_or_else(|| fail("its extra headers include a value that is not text"))?;
        let header_value = HeaderValue::from_str(text)
            .map_err(|_| fail("its extra headers include an invalid value"))?;
        out.push((header_name, header_value));
    }
    Ok(out)
}

fn parse_options(
    route: &HostedRoute,
    dialect: Dialect,
) -> Result<Map<String, Value>, ProviderError> {
    let fail = |detail: &str| misconfigured(route, detail);
    let obj = match &route.request_options {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::Object(o)) => o.clone(),
        Some(_) => return Err(fail("this model's request options are not a JSON object")),
    };
    for key in obj.keys() {
        let k = key.as_str();
        let reserved = FORBIDDEN_OPTION_KEYS.contains(&k)
            || (dialect == Dialect::AnthropicMessages && k == "system");
        if reserved {
            return Err(fail("this model's request options set a field QR controls"));
        }
    }
    if dialect == Dialect::OpenaiChat
        && OPENAI_LIMIT_KEYS
            .iter()
            .filter(|k| obj.get(**k).is_some_and(|v| !v.is_null()))
            .count()
            > 1
    {
        return Err(fail("this model's request options set two token limits"));
    }
    for k in OPENAI_LIMIT_KEYS {
        if let Some(v) = obj.get(k) {
            let ok = v.is_null() || v.as_u64().is_some_and(|n| n > 0);
            if !ok {
                return Err(fail(
                    "this model's token limit is not a positive whole number",
                ));
            }
        }
    }
    let merged = merge_required_options(route, dialect, obj)?;
    // items.id=715: `stream_options` is honored only on streamed calls and is
    // dropped from whole-body calls. A row may replace the default or, with
    // JSON null, remove it for a provider that rejects it.
    if merged
        .get("stream_options")
        .is_some_and(|v| !(v.is_null() || v.is_object()))
    {
        return Err(fail(
            "this model's stream options are not a JSON object or null",
        ));
    }
    Ok(merged)
}

/// Apply the provider's `required_request_options` OVER the model's options
/// (items.id=711). The required object is validated like any request options
/// (an object, no field QR controls). A key present in both must agree: a
/// nested object is merged key by key, anything else must be equal, otherwise
/// the call is Misconfigured and nothing is sent, so a model row can never
/// weaken a provider-wide privacy option.
fn merge_required_options(
    route: &HostedRoute,
    dialect: Dialect,
    mut model_options: Map<String, Value>,
) -> Result<Map<String, Value>, ProviderError> {
    let required = match &route.required_request_options {
        None | Some(Value::Null) => return Ok(model_options),
        Some(Value::Object(o)) => o,
        Some(_) => {
            return Err(misconfigured(
                route,
                "this provider's required request options are not a JSON object",
            ))
        }
    };
    for key in required.keys() {
        let k = key.as_str();
        let reserved = FORBIDDEN_OPTION_KEYS.contains(&k)
            || OPENAI_LIMIT_KEYS.contains(&k)
            || (dialect == Dialect::AnthropicMessages && k == "system");
        if reserved {
            return Err(misconfigured(
                route,
                "this provider's required request options set a field QR controls",
            ));
        }
    }

    fn merge(into: &mut Map<String, Value>, required: &Map<String, Value>) -> bool {
        for (k, want) in required {
            match into.get_mut(k) {
                None => {
                    into.insert(k.clone(), want.clone());
                }
                Some(Value::Object(have)) => match want {
                    Value::Object(want) => {
                        if !merge(have, want) {
                            return false;
                        }
                    }
                    _ => return false,
                },
                Some(have) => {
                    if have != want {
                        return false;
                    }
                }
            }
        }
        true
    }

    if merge(&mut model_options, required) {
        Ok(model_options)
    } else {
        Err(misconfigured(
            route,
            "this model's request options conflict with the provider's required options",
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn route() -> HostedRoute {
        HostedRoute {
            provider_id: "fixture-host".to_owned(),
            display_name: "Fixture Host".to_owned(),
            api_dialect: Some("openai_chat".to_owned()),
            base_url_template: Some("https://api.example.test/v1".to_owned()),
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

    fn build(r: &HostedRoute) -> Result<CallContext, ProviderError> {
        CallContext::build(r, "sk-fixture-secret", &json!({}))
    }

    fn plain(e: &ProviderError) -> String {
        match e {
            ProviderError::Misconfigured { plain_language, .. }
            | ProviderError::MissingApiKey { plain_language, .. }
            | ProviderError::ModelNotFound { plain_language, .. } => plain_language.clone(),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn builds_a_valid_route() {
        let ctx = build(&route()).unwrap();
        assert_eq!(
            ctx.generate_url(),
            "https://api.example.test/v1/chat/completions"
        );
        assert_eq!(
            ctx.models_url().as_deref(),
            Some("https://api.example.test/v1/models")
        );
        assert_eq!(ctx.row_token_limit(), Some(("max_completion_tokens", 512)));
    }

    #[test]
    fn trailing_slash_in_template_is_trimmed() {
        let mut r = route();
        r.base_url_template = Some("https://api.example.test/v1beta/openai/".to_owned());
        assert_eq!(
            build(&r).unwrap().generate_url(),
            "https://api.example.test/v1beta/openai/chat/completions"
        );
    }

    #[test]
    fn messages_dialect_paths_and_default_auth() {
        let mut r = route();
        r.api_dialect = Some("anthropic_messages".to_owned());
        r.auth_scheme = None;
        r.base_url_template = Some("https://api.example.test".to_owned());
        r.models_path = Some("/v1/models".to_owned());
        r.request_options = Some(json!({"max_tokens": 256}));
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.generate_url(), "https://api.example.test/v1/messages");
        assert_eq!(
            ctx.models_url().unwrap(),
            "https://api.example.test/v1/models"
        );
        assert_eq!(ctx.auth, AuthScheme::XApiKey);
    }

    #[test]
    fn unsupported_dialect_and_auth_are_rejected() {
        let mut r = route();
        r.api_dialect = Some("some_future_format".to_owned());
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
        let mut r = route();
        r.api_dialect = None;
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
        let mut r = route();
        r.auth_scheme = Some("oauth_dance".to_owned());
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
    }

    #[test]
    fn non_https_and_hostile_templates_are_rejected() {
        for template in [
            "http://api.example.test/v1",
            "http://127.0.0.1:9/v1", // loopback http is test-constructor only
            "ftp://api.example.test/v1",
            "api.example.test/v1",
            "https://api.example.test/v1?x=1",
            "https://api.example.test/v1#frag",
            "https://user@api.example.test/v1",
            "https://{account}.example.test/v1",
            "https://api.example.test:{account}/v1",
            "https://{account}/v1",
            "https://api.example.test/{account",
            "https://api.example.test/account}",
            "https://api.example.test/{}",
            "https://api.example.test/{Account}",
            "",
        ] {
            let mut r = route();
            r.user_variables = Some(json!(["account"]));
            r.base_url_template = Some(template.to_owned());
            let out = CallContext::build(&r, "k", &json!({"account": "abc123"}));
            assert!(
                matches!(out, Err(ProviderError::Misconfigured { .. })),
                "template {template:?} must be rejected, got {out:?}"
            );
        }
    }

    fn account_route() -> HostedRoute {
        let mut r = route();
        r.user_variables = Some(json!(["account_id"]));
        r.base_url_template =
            Some("https://api.example.test/client/v4/accounts/{account_id}/ai/v1".to_owned());
        r
    }

    #[test]
    fn variable_is_substituted_into_a_path_segment() {
        let ctx = CallContext::build(
            &account_route(),
            "k",
            &json!({"account_id": "0123abcDEF_-9"}),
        )
        .unwrap();
        assert_eq!(
            ctx.generate_url(),
            "https://api.example.test/client/v4/accounts/0123abcDEF_-9/ai/v1/chat/completions"
        );
        assert!(ctx.base_url().starts_with("https://api.example.test/"));
    }

    #[test]
    fn variable_values_with_dangerous_characters_are_rejected() {
        let too_long = "a".repeat(MAX_VARIABLE_LEN + 1);
        for bad in [
            "evil.example",
            "a/b",
            "..",
            "a@b",
            "a:80",
            "a%2Fb",
            "a?b",
            "a#b",
            "a b",
            "a\\b",
            "a\nb",
            "ünï",
            "",
            too_long.as_str(),
        ] {
            let out = CallContext::build(&account_route(), "k", &json!({"account_id": bad}));
            assert!(
                matches!(out, Err(ProviderError::Misconfigured { .. })),
                "value {bad:?} must be rejected, got {out:?}"
            );
        }
        // A non-string value is rejected too.
        let out = CallContext::build(&account_route(), "k", &json!({"account_id": 12345}));
        assert!(matches!(out, Err(ProviderError::Misconfigured { .. })));
    }

    #[test]
    fn a_value_at_the_length_limit_is_accepted() {
        let v = "a".repeat(MAX_VARIABLE_LEN);
        assert!(CallContext::build(&account_route(), "k", &json!({ "account_id": v })).is_ok());
    }

    #[test]
    fn missing_variable_is_a_finish_connecting_error_not_misconfigured() {
        for meta in [
            json!({}),
            json!({"account_id": null}),
            json!({"other": "x"}),
        ] {
            let out = CallContext::build(&account_route(), "k", &meta);
            assert!(
                matches!(out, Err(ProviderError::MissingApiKey { .. })),
                "got {out:?}"
            );
        }
    }

    #[test]
    fn undeclared_placeholder_is_rejected_even_if_metadata_has_it() {
        let mut r = account_route();
        r.user_variables = Some(json!([]));
        let out = CallContext::build(&r, "k", &json!({"account_id": "abc"}));
        assert!(matches!(out, Err(ProviderError::Misconfigured { .. })));
    }

    #[test]
    fn invalid_user_variable_list_is_rejected() {
        for bad in [json!("account"), json!([1]), json!(["Bad-Name"]), json!({})] {
            let mut r = account_route();
            r.user_variables = Some(bad);
            let out = CallContext::build(&r, "k", &json!({"account_id": "abc"}));
            assert!(matches!(out, Err(ProviderError::Misconfigured { .. })));
        }
    }

    #[test]
    fn empty_key_is_a_missing_key() {
        for k in ["", "   ", "\n"] {
            let out = CallContext::build(&route(), k, &json!({}));
            assert!(matches!(out, Err(ProviderError::MissingApiKey { .. })));
        }
    }

    #[test]
    fn retired_model_is_refused_before_any_request() {
        let mut r = route();
        r.model_status = ModelStatus::Retired;
        let e = build(&r).unwrap_err();
        assert!(matches!(e, ProviderError::ModelNotFound { .. }));
        assert!(plain(&e).contains("Fixture Host"));
    }

    #[test]
    fn deprecated_model_is_still_callable() {
        let mut r = route();
        r.model_status = ModelStatus::Deprecated;
        assert!(build(&r).is_ok());
    }

    #[test]
    fn extra_headers_are_validated() {
        let mut r = route();
        r.extra_headers = Some(json!({"X-Org-Tag": "abc", "anthropic-version": "2030-01-01"}));
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.extra_headers.len(), 2);

        for forbidden in [
            "Host",
            "Content-Length",
            "content-type",
            "Transfer-Encoding",
            "Connection",
            "Authorization",
            "X-Api-Key",
            "Cookie",
            "Proxy-Authorization",
            "proxy-anything",
        ] {
            let mut r = route();
            r.extra_headers = Some(json!({ forbidden: "x" }));
            assert!(
                matches!(build(&r), Err(ProviderError::Misconfigured { .. })),
                "{forbidden} must be rejected"
            );
        }
        for bad in [
            json!(["a"]),
            json!({"X-Ok": 5}),
            json!({"bad name": "x"}),
            json!({"X-Ok": "line\nbreak"}),
        ] {
            let mut r = route();
            r.extra_headers = Some(bad);
            assert!(matches!(
                build(&r),
                Err(ProviderError::Misconfigured { .. })
            ));
        }
    }

    // -- items.id=711: provider-level required_request_options ---------------

    fn with_required(model: serde_json::Value, required: serde_json::Value) -> HostedRoute {
        let mut r = route();
        r.request_options = Some(model);
        r.required_request_options = Some(required);
        r
    }

    #[test]
    fn required_options_are_added_to_a_model_that_lacks_them() {
        // The case the field exists for: a model with no curated options of
        // its own still carries the provider-wide privacy option.
        let r = with_required(
            json!({"max_tokens": 64}),
            json!({"fixture_routing": {"fixture_collect": "deny"}}),
        );
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.options["fixture_routing"]["fixture_collect"], "deny");
        assert_eq!(ctx.options["max_tokens"], 64);
    }

    #[test]
    fn required_options_merge_into_a_shared_nested_object() {
        let r = with_required(
            json!({"max_tokens": 64, "fixture_routing": {"order": ["a"]}}),
            json!({"fixture_routing": {"fixture_collect": "deny"}}),
        );
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.options["fixture_routing"]["fixture_collect"], "deny");
        assert_eq!(ctx.options["fixture_routing"]["order"], json!(["a"]));
    }

    #[test]
    fn a_model_row_that_weakens_a_required_option_fails_closed() {
        // The same key set to a different value, at the top level and nested,
        // and a scalar where the provider requires an object: all are
        // Misconfigured, so nothing is sent.
        for model in [
            json!({"max_tokens": 64, "fixture_routing": {"fixture_collect": "allow"}}),
            json!({"max_tokens": 64, "fixture_routing": "anything"}),
            json!({"max_tokens": 64, "fixture_routing": {"fixture_collect": null}}),
        ] {
            let r = with_required(
                model.clone(),
                json!({"fixture_routing": {"fixture_collect": "deny"}}),
            );
            assert!(
                matches!(build(&r), Err(ProviderError::Misconfigured { .. })),
                "{model} must fail closed"
            );
        }
        let r = with_required(
            json!({"max_tokens": 64, "fixture_flag": false}),
            json!({"fixture_flag": true}),
        );
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
    }

    #[test]
    fn identical_required_options_on_the_model_row_are_accepted() {
        let r = with_required(
            json!({"max_tokens": 64, "fixture_routing": {"fixture_collect": "deny"}}),
            json!({"fixture_routing": {"fixture_collect": "deny"}}),
        );
        assert!(build(&r).is_ok());
    }

    #[test]
    fn required_options_cannot_set_fields_qr_controls() {
        for reserved in [
            "model",
            "messages",
            "stream",
            "max_tokens",
            "max_completion_tokens",
        ] {
            let r = with_required(json!({"max_tokens": 64}), json!({ reserved: 1 }));
            assert!(
                matches!(build(&r), Err(ProviderError::Misconfigured { .. })),
                "{reserved} must be rejected in required options"
            );
        }
        let r = with_required(json!({"max_tokens": 64}), json!([1]));
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
        // `system` is reserved on the Messages dialect only.
        let mut r = with_required(json!({"max_tokens": 64}), json!({"system": "x"}));
        r.api_dialect = Some("anthropic_messages".to_owned());
        r.base_url_template = Some("https://api.example.test".to_owned());
        r.auth_scheme = Some("x-api-key".to_owned());
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
    }

    #[test]
    fn request_options_must_be_an_object_without_reserved_keys() {
        for bad in [json!([1]), json!("x"), json!(5)] {
            let mut r = route();
            r.request_options = Some(bad);
            assert!(matches!(
                build(&r),
                Err(ProviderError::Misconfigured { .. })
            ));
        }
        for reserved in ["model", "messages", "stream"] {
            let mut r = route();
            r.request_options = Some(json!({ reserved: "x", "max_tokens": 5 }));
            assert!(
                matches!(build(&r), Err(ProviderError::Misconfigured { .. })),
                "{reserved} must be rejected"
            );
        }
        // `system` is reserved on the Messages dialect only.
        let mut r = route();
        r.api_dialect = Some("anthropic_messages".to_owned());
        r.request_options = Some(json!({"system": "x", "max_tokens": 5}));
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
        let mut r = route();
        r.request_options = Some(json!({"system": "x", "max_tokens": 5}));
        assert!(build(&r).is_ok());
    }

    #[test]
    fn nested_options_pass_through_untouched() {
        let mut r = route();
        r.request_options = Some(json!({
            "max_completion_tokens": 100,
            "provider": {"data_collection": "deny"},
            "reasoning_effort": "low"
        }));
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.options["provider"]["data_collection"], "deny");
        assert_eq!(ctx.options["reasoning_effort"], "low");
    }

    #[test]
    fn token_limit_values_are_validated() {
        for bad in [
            json!({"max_tokens": 0}),
            json!({"max_tokens": -5}),
            json!({"max_tokens": "9"}),
            json!({"max_tokens": 1.5}),
        ] {
            let mut r = route();
            r.request_options = Some(bad);
            assert!(matches!(
                build(&r),
                Err(ProviderError::Misconfigured { .. })
            ));
        }
        let mut r = route();
        r.request_options = Some(json!({"max_tokens": 5, "max_completion_tokens": 5}));
        assert!(matches!(
            build(&r),
            Err(ProviderError::Misconfigured { .. })
        ));
    }

    #[test]
    fn models_path_is_validated() {
        for bad in [
            "models",
            "/../models",
            "//models",
            "/models?x=1",
            "/mo dels",
            "/m#",
        ] {
            let mut r = route();
            r.models_path = Some(bad.to_owned());
            assert!(
                matches!(build(&r), Err(ProviderError::Misconfigured { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn limit_key_follows_the_row_and_defaults_for_calls() {
        let ctx = build(&route()).unwrap();
        assert_eq!(ctx.limit_key_for_call(), "max_completion_tokens");
        let mut r = route();
        r.request_options = Some(json!({}));
        let ctx = build(&r).unwrap();
        assert_eq!(ctx.row_token_limit(), None);
        assert_eq!(ctx.limit_key_for_call(), "max_tokens");
    }

    /// The key never prints: not in Debug of the key, the context, nor in
    /// any error text produced while building.
    #[test]
    fn key_never_appears_in_debug_or_error_text() {
        let secret = "sk-very-secret-value-12345";
        let ctx = CallContext::build(&route(), secret, &json!({})).unwrap();
        assert!(!format!("{ctx:?}").contains(secret));
        assert!(!format!("{:?}", ctx.key).contains(secret));
        assert!(format!("{:?}", ctx.key).contains("redacted"));

        let mut r = route();
        r.api_dialect = Some("nope".to_owned());
        let e = CallContext::build(&r, secret, &json!({})).unwrap_err();
        assert!(!format!("{e:?}").contains(secret));
        assert!(!format!("{e}").contains(secret));
        assert!(!plain(&e).contains(secret));
    }

    /// Spec 8a rule 1 / decisions.id=872: error copy comes from the row.
    #[test]
    fn error_copy_uses_the_rows_display_name() {
        let mut r = route();
        r.display_name = "Zorblax Cloud".to_owned();
        r.api_dialect = Some("nope".to_owned());
        assert!(plain(&build(&r).unwrap_err()).contains("Zorblax Cloud"));
        assert!(plain(&missing_key(&r)).contains("Zorblax Cloud"));
    }
}
