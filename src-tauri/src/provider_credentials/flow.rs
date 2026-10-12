// src-tauri/src/provider_credentials/flow.rs
//
// Check a candidate key with its provider, then store it (items.id=714).
//
// ORDER. Validate everything locally first (key text, variables), build the
// call context (this also refuses a variable that would move the request to
// another host), then check the key, and only then write. A key the provider
// says is invalid is never stored. A check that could not be completed
// (network, 5xx, rate limit) stores nothing unless the user confirmed saving
// without one (`save_unverified`).
//
// NETWORK. The check is one GET of the provider's model list with the user's
// own key, the same request refresh_available_models makes; it carries no
// content. It only ever runs from a click on save_provider_key.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Map, Value};
use specta::Type;
use sqlx::SqliteConnection;

use super::key_pattern::normalise_key_text;
use super::{declared_variables, is_hosted};
use crate::persistence::integration_keys_store::{
    save_credential_conn, NewCredential, QR_HOSTED_KEY_TYPE,
};
use crate::persistence::provider_store::{Provider, ProviderModel};
use crate::providers::errors::ProviderError;
use crate::providers::hosted_call::{is_valid_variable_value, CallContext, HostedRoute};
use crate::providers::hosted_client::HostedClient;

pub type BuildFn = fn(&HostedRoute, &str, &Value) -> Result<CallContext, ProviderError>;

/// `auth_type` stored for a key the user pasted (AUTH section 8.2's closed set;
/// the value `set_qr_hosted_provider` already writes).
pub const PASTED_KEY_AUTH_TYPE: &str = "api_key";

/// What happened to the key check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
pub enum CheckState {
    /// The provider accepted the key; `last_verified_at` is set.
    Verified,
    /// The row has no model list to ask, so the key could not be checked. Saved
    /// unverified; the first real call is its test.
    Unavailable,
    /// The check could not be completed and the user chose to save anyway.
    Inconclusive,
}

pub struct SaveParams<'a> {
    pub provider: &'a Provider,
    pub models: &'a [ProviderModel],
    /// The key text as typed or offered; normalised here.
    pub key: &'a str,
    pub variables: &'a BTreeMap<String, String>,
    /// Save when the check is inconclusive. Never overrides a rejected key.
    pub save_unverified: bool,
}

/// Validate the user's variables against the row's declared list. Returns the
/// object to store, or None when the row declares none.
pub fn validate_variables(
    provider: &Provider,
    given: &BTreeMap<String, String>,
) -> Result<Option<Value>, &'static str> {
    let declared = declared_variables(provider).ok_or("misconfigured")?;
    if given.keys().any(|k| !declared.contains(k)) {
        return Err("variable_undeclared");
    }
    if declared.is_empty() {
        return Ok(None);
    }
    let mut object = Map::new();
    for name in &declared {
        let value = given.get(name).ok_or("variable_missing")?;
        if !is_valid_variable_value(value) {
            return Err("variable_invalid");
        }
        object.insert(name.clone(), json!(value));
    }
    Ok(Some(Value::Object(object)))
}

fn build_error(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::InvalidApiKey { .. } => "invalid_api_key",
        _ => "misconfigured",
    }
}

/// Check and store. See the module comment for the order.
pub async fn save_impl(
    client: &HostedClient,
    conn: &mut SqliteConnection,
    params: &SaveParams<'_>,
    build: BuildFn,
    now: DateTime<Utc>,
) -> Result<CheckState, String> {
    let provider = params.provider;
    if !is_hosted(provider) {
        return Err("unknown_provider".to_owned());
    }
    let key = normalise_key_text(params.key).ok_or("invalid_key_syntax")?;
    let metadata = validate_variables(provider, params.variables)?;

    // A route needs a model row to build from; every hosted provider has one.
    let route_model = params
        .models
        .iter()
        .find(|m| m.is_default)
        .or_else(|| params.models.first())
        .ok_or("misconfigured")?;
    let route = HostedRoute::from_rows(provider, route_model);
    let ctx_metadata = metadata.clone().unwrap_or_else(|| json!({}));
    let ctx = build(&route, key, &ctx_metadata).map_err(|e| build_error(&e))?;

    let state = if provider.models_path.is_none() {
        CheckState::Unavailable
    } else {
        match client.check_key(&ctx).await {
            Ok(_) => CheckState::Verified,
            Err(ProviderError::InvalidApiKey { .. }) => return Err("invalid_api_key".to_owned()),
            Err(ProviderError::Misconfigured { .. }) => return Err("misconfigured".to_owned()),
            Err(_) if params.save_unverified => CheckState::Inconclusive,
            Err(_) => return Err("could_not_verify".to_owned()),
        }
    };
    drop(ctx);

    let verified_at = (state == CheckState::Verified).then(|| now.to_rfc3339());
    save_credential_conn(
        conn,
        &NewCredential {
            provider: &provider.id,
            key_type: QR_HOSTED_KEY_TYPE,
            credential: key,
            persona_id: None,
            auth_type: Some(PASTED_KEY_AUTH_TYPE),
            expires_at: None,
            last_verified_at: verified_at.as_deref(),
            extra_metadata: metadata.as_ref(),
        },
    )
    .await
    .map_err(|e| {
        log::warn!("provider_credentials: store failed: {e}");
        "store_failed".to_owned()
    })?;
    Ok(state)
}
