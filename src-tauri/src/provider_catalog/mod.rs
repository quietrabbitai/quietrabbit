// src-tauri/src/provider_catalog/mod.rs
//
// Signed provider catalog update (items.id=713; decisions.id=875 point 6,
// spec Part 8f). Between app releases the curated prices, dates and flags of
// the hosted-provider rows can be refreshed, only when a signed-in user clicks.
//
// FLOW (every step is a click; nothing runs at startup or on a timer except
// `reconcile_after_migration`, which only re-applies a catalog the user
// already applied):
//   1. check_catalog_update: fetch the manifest and its minisign signature,
//      verify, fetch the body pinned by the manifest's size and SHA-256,
//      stage all three files, and return a PREVIEW (what changes, which
//      providers will ask for consent again). The database is not touched.
//   2. apply_catalog_update (admin): re-verify the staged files, apply in one
//      transaction, record the version and the undo images.
//   3. rollback_catalog (admin): restore the undo images.
//
// TRUST. The signature covers the manifest; the manifest pins the body. The
// public keys are compiled in (verify::TRUSTED_PUBLIC_KEYS). Until Jason
// supplies the production public key that list is EMPTY and the feature
// fails closed: check_catalog_update returns `catalog.error.no_trusted_key`
// before any network traffic. That is a RELEASE PREREQUISITE.
//
// WHAT A CATALOG MAY CHANGE is exactly what format.rs can express (see there
// and apply.rs). Consent effects, release-only columns and the two named
// guarantees are enforced in apply.rs.
//
// BASELINE (decisions: Chat-PM D7 = a). src-tauri/catalog/provider-catalog.json
// is the catalog the release's own migrations agree with. Its version is the
// floor for updates (a catalog at or below it is never offered) and a test
// proves the file validates and is a no-op on a freshly migrated database.

pub mod apply;
pub mod flow;
pub mod format;
pub mod verify;

#[cfg(test)]
mod tests;

use std::sync::OnceLock;

use chrono::{Datelike, NaiveDate};

/// The checked-in baseline catalog (the file a release signs and publishes).
pub const BASELINE_CATALOG_JSON: &str = include_str!("../../catalog/provider-catalog.json");

/// Release asset locations on the QR repository's rolling release tag
/// `provider-catalog` (the repository is public, `gh repo view`, 2026-10-11).
/// No query string, no identifier.
pub const RELEASE_BASE_URL: &str =
    "https://github.com/quietrabbitai/quietrabbit/releases/download/provider-catalog";
pub const MANIFEST_NAME: &str = "catalog-manifest.json";
pub const SIGNATURE_NAME: &str = "catalog-manifest.json.minisig";

/// The catalog version the baseline file carries.
pub fn baseline_version() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    *V.get_or_init(|| {
        serde_json::from_str::<serde_json::Value>(BASELINE_CATALOG_JSON)
            .ok()
            .and_then(|v| v.get("catalog_version").and_then(serde_json::Value::as_u64))
            .unwrap_or(0)
    })
}

/// The date inside a YYYYMMDDNN version.
pub fn version_date(version: u64) -> Option<NaiveDate> {
    let ymd = version / 100;
    let year = i32::try_from(ymd / 10_000).ok()?;
    let month = u32::try_from((ymd / 100) % 100).ok()?;
    let day = u32::try_from(ymd % 100).ok()?;
    let d = NaiveDate::from_ymd_opt(year, month, day)?;
    (d.year() >= 2026).then_some(d)
}

/// Stable codes the catalog code emits. The frontend maps them to i18n keys
/// (items.id=728). There is no user-facing prose in this module.
pub mod codes {
    pub const REJECT_DATABASE: &str = "catalog.reject.database";
    pub const REJECT_UNKNOWN_PROVIDER: &str = "catalog.reject.unknown_provider";
    pub const REJECT_NOT_HOSTED_PROVIDER: &str = "catalog.reject.not_hosted_provider";
    pub const REJECT_UNKNOWN_MODEL: &str = "catalog.reject.unknown_model";
    pub const REJECT_MODEL_EXISTS: &str = "catalog.reject.model_exists";
    pub const REJECT_DUPLICATE_ENTRY: &str = "catalog.reject.duplicate_entry";
    pub const REJECT_BAD_VALUE: &str = "catalog.reject.bad_value";
    pub const REJECT_BAD_PRICE: &str = "catalog.reject.bad_price";
    pub const REJECT_BAD_URL: &str = "catalog.reject.bad_url";
    pub const REJECT_URL_HOST_CHANGED: &str = "catalog.reject.url_host_changed";
    pub const REJECT_OUTPUT_LIMIT: &str = "catalog.reject.output_limit";
    pub const REJECT_DEFAULT_MODEL: &str = "catalog.reject.default_model";
    pub const REJECT_RELEASE_ONLY_CHANGED: &str = "catalog.reject.release_only_changed";
    pub const REJECT_REQUIRED_OPTIONS_CHANGED: &str = "catalog.reject.required_options_changed";
    pub const REJECT_UNEXPLAINED_CONSENT_CHANGE: &str = "catalog.reject.unexplained_consent_change";

    pub const SKIP_PRICE_OLDER: &str = "catalog.skip.price_older_than_current";
    pub const SKIP_CONTEXT_OLDER: &str = "catalog.skip.context_older_than_current";

    /// Why a provider will ask for consent again after this catalog. The
    /// consent attributes hash covers the set of non-retired models and each
    /// one's cost class, so retiring a model re-asks consent as well
    /// (disclosure.rs, as built).
    pub const CONSENT_MODEL_ADDED: &str = "catalog.consent.model_added";
    pub const CONSENT_MODEL_RETIRED: &str = "catalog.consent.model_retired";
    pub const CONSENT_COST_CLASS_CHANGED: &str = "catalog.consent.cost_class_changed";

    pub const STATE_UP_TO_DATE: &str = "catalog.state.up_to_date";
    pub const STATE_UPDATE_AVAILABLE: &str = "catalog.state.update_available";

    pub const ERROR_NO_TRUSTED_KEY: &str = "catalog.error.no_trusted_key";
    pub const ERROR_SIGNATURE_INVALID: &str = "catalog.error.signature_invalid";
    pub const ERROR_MANIFEST_INVALID: &str = "catalog.error.manifest_invalid";
    pub const ERROR_CATALOG_INVALID: &str = "catalog.error.catalog_invalid";
    pub const ERROR_VERSION_TOO_FAR_AHEAD: &str = "catalog.error.version_too_far_ahead";
    pub const ERROR_NEEDS_NEWER_APP: &str = "catalog.error.needs_newer_app";
    pub const ERROR_NOT_NEWER: &str = "catalog.error.not_newer";
    pub const ERROR_DOWNLOAD_FAILED: &str = "catalog.error.download_failed";
    pub const ERROR_ASSET_MISMATCH: &str = "catalog.error.asset_mismatch";
    pub const ERROR_NOT_STAGED: &str = "catalog.error.not_staged";
    pub const ERROR_NOT_ADMIN: &str = "catalog.error.not_admin";
    pub const ERROR_NOT_LOGGED_IN: &str = "catalog.error.not_logged_in";
    pub const ERROR_NOTHING_TO_ROLL_BACK: &str = "catalog.error.nothing_to_roll_back";
    pub const ERROR_INTERNAL: &str = "catalog.error.internal";

    pub const ALL: &[&str] = &[
        REJECT_DATABASE,
        REJECT_UNKNOWN_PROVIDER,
        REJECT_NOT_HOSTED_PROVIDER,
        REJECT_UNKNOWN_MODEL,
        REJECT_MODEL_EXISTS,
        REJECT_DUPLICATE_ENTRY,
        REJECT_BAD_VALUE,
        REJECT_BAD_PRICE,
        REJECT_BAD_URL,
        REJECT_URL_HOST_CHANGED,
        REJECT_OUTPUT_LIMIT,
        REJECT_DEFAULT_MODEL,
        REJECT_RELEASE_ONLY_CHANGED,
        REJECT_REQUIRED_OPTIONS_CHANGED,
        REJECT_UNEXPLAINED_CONSENT_CHANGE,
        SKIP_PRICE_OLDER,
        SKIP_CONTEXT_OLDER,
        CONSENT_MODEL_ADDED,
        CONSENT_MODEL_RETIRED,
        CONSENT_COST_CLASS_CHANGED,
        STATE_UP_TO_DATE,
        STATE_UPDATE_AVAILABLE,
        ERROR_NO_TRUSTED_KEY,
        ERROR_SIGNATURE_INVALID,
        ERROR_MANIFEST_INVALID,
        ERROR_CATALOG_INVALID,
        ERROR_VERSION_TOO_FAR_AHEAD,
        ERROR_NEEDS_NEWER_APP,
        ERROR_NOT_NEWER,
        ERROR_DOWNLOAD_FAILED,
        ERROR_ASSET_MISMATCH,
        ERROR_NOT_STAGED,
        ERROR_NOT_ADMIN,
        ERROR_NOT_LOGGED_IN,
        ERROR_NOTHING_TO_ROLL_BACK,
        ERROR_INTERNAL,
    ];
}
