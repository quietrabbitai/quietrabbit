// src-tauri/src/provider_catalog/format.rs
//
// The two documents of a catalog release (items.id=713).
//
//   catalog-manifest.json       SIGNED (minisign, detached). Names the catalog
//                               body and pins it by size and SHA-256.
//   provider-catalog.json       the body. Trusted only because the signed
//                               manifest pins its hash.
//
// THE FORMAT IS THE ALLOWLIST. Every struct here is `deny_unknown_fields`, and
// there is no field for anything a catalog must not change: no base URL, auth
// scheme, dialect, extra headers, model-list path, OAuth URL, local row, new
// provider, `required_request_options`, raw `request_options`, privacy
// attribute, privacy override, `documentation_gate` or jurisdiction. Those are
// release-only (decisions.id=875, Chat-PM ruling on items.id=713, D4/D5). A
// document that names one is rejected by the parser, not filtered afterwards.
//
// Prices travel as decimal STRINGS ("0.075"). The stored `pricing` text keeps
// each number's source lexeme because spend::price reads exact decimals; a
// string here is rendered back into that lexeme without passing through f64.

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

pub const FORMAT_VERSION: u32 = 1;
/// The one body name a manifest may point at (no path, no other file).
pub const CATALOG_ASSET_NAME: &str = "provider-catalog.json";

pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
pub const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    /// YYYYMMDDNN: the issue date plus a two-digit sequence for the day.
    pub catalog_version: u64,
    /// The newest shared.db schema version this catalog needs (the columns it
    /// writes must exist).
    pub min_shared_schema: u32,
    pub asset_name: String,
    pub size: u64,
    /// Lower-case or upper-case hex SHA-256 of the body.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogDoc {
    pub format: u32,
    pub catalog_version: u64,
    #[serde(default)]
    pub providers: Vec<ProviderUpdate>,
    #[serde(default)]
    pub models: Vec<ModelUpdate>,
    #[serde(default)]
    pub new_models: Vec<NewModel>,
}

/// Neutral columns of an EXISTING hosted provider row. An absent field leaves
/// the column alone.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderUpdate {
    pub id: String,
    /// Free-tier facts. Any object with `available` (bool) and `as_of` (date).
    pub free_tier: Option<Map<String, Value>>,
    pub plan_inclusion: Option<Vec<String>>,
    pub last_reviewed_at: Option<String>,
    pub review_trigger_note: Option<String>,
    pub qr_recommended: Option<bool>,
    pub qr_recommendation_rank: Option<u32>,
    /// https, and the host must not change from the stored value.
    pub spend_limit_url: Option<String>,
    /// `credential_acquisition.keys_page_url`: https, same host rule.
    pub keys_page_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierDoc {
    pub prompt_tokens_up_to: Option<u64>,
    pub prompt_tokens_over: Option<u64>,
    pub input_per_mtok: String,
    pub output_per_mtok: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceDoc {
    pub input_per_mtok: String,
    pub output_per_mtok: String,
    #[serde(default)]
    pub tiers: Vec<TierDoc>,
    pub as_of: String,
    pub source_url: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDoc {
    pub tokens: u32,
    pub as_of: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusDoc {
    Active,
    Deprecated,
    Retired,
}

impl StatusDoc {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusDoc::Active => "active",
            StatusDoc::Deprecated => "deprecated",
            StatusDoc::Retired => "retired",
        }
    }
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// Neutral columns of an EXISTING model row (provider_id + model_id).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUpdate {
    pub provider_id: String,
    pub model_id: String,
    pub pricing: Option<PriceDoc>,
    pub context_window: Option<ContextDoc>,
    pub token_factor: Option<f64>,
    pub output_overhead: Option<f64>,
    /// The VALUE of the model's output token limit. The key name stays as the
    /// row has it, and nothing else in `request_options` can be touched.
    pub output_limit_tokens: Option<u32>,
    pub qr_recommended: Option<bool>,
    pub qr_recommendation_rank: Option<u32>,
    pub requires_paid: Option<bool>,
    pub is_default: Option<bool>,
    pub status: Option<StatusDoc>,
    /// Absent = unchanged, `null` = clear, a date = set.
    #[serde(default, deserialize_with = "double_option")]
    pub retires_at: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitDoc {
    /// `max_tokens` or `max_completion_tokens`.
    pub key: String,
    pub tokens: u32,
}

/// A model the release did not have. It is created with NO privacy overrides
/// and no request options other than its output limit; the provider's
/// `required_request_options` apply to it at call time like to any model.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewModel {
    pub provider_id: String,
    pub model_id: String,
    pub pricing: PriceDoc,
    pub context_window: ContextDoc,
    pub output_limit: LimitDoc,
    pub token_factor: Option<f64>,
    pub output_overhead: Option<f64>,
    #[serde(default)]
    pub qr_recommended: bool,
    pub qr_recommendation_rank: Option<u32>,
    #[serde(default)]
    pub requires_paid: bool,
    #[serde(default)]
    pub is_default: bool,
    /// `active` (the default) or `deprecated`. A new model cannot be retired.
    pub status: Option<StatusDoc>,
    pub retires_at: Option<String>,
}

/// Parse a manifest. Call only on bytes whose signature has verified.
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("manifest: {e}"))
}

/// Parse a catalog body. Call only on bytes whose hash matched a verified
/// manifest.
pub fn parse_catalog(bytes: &[u8]) -> Result<CatalogDoc, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("catalog: {e}"))
}
