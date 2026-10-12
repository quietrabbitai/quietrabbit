// src-tauri/src/spend/fixtures.rs
//
// Neutral catalog fixtures for the spend tests (decisions.id=872: no provider
// or model is named in code or tests). Test builds only.

use serde_json::json;

use crate::persistence::provider_store::{
    ActivationStatus, ModelStatus, PrivacyCommitmentBasis, Provider, ProviderMode, ProviderModel,
    VerificationStatus,
};

pub(crate) fn provider() -> Provider {
    Provider {
        id: "fixture-host-a".to_owned(),
        display_name: "Fixture Host A".to_owned(),
        provider_type: "cloud_inference_api".to_owned(),
        mode: ProviderMode::Api,
        launch_url: None,
        activation_status: ActivationStatus::Active,
        documentation_gate: json!({
            "model_overrides": ["Batch features retain data."],
            "disclosures": ["Fixture disclosure."],
            "jurisdiction": "Fixtureland",
        }),
        user_privacy_summary: None,
        last_reviewed_at: None,
        review_trigger_note: None,
        created_at: "t".to_owned(),
        is_local: false,
        is_anonymous: true,
        retains_data: false,
        trains_on_data_by_default: false,
        login_required: false,
        qr_internal_eligible: false,
        privacy_guardian_default_level: None,
        risk_rating: 1,
        hardware_requirement: None,
        qr_recommended: false,
        privacy_commitment_basis: Some(PrivacyCommitmentBasis::Contractual),
        performance_profile: None,
        preference_tier: "supported".to_owned(),
        installed: false,
        qr_disabled_by_user: false,
        local_model_tag: None,
        local_model_digest: None,
        installed_at: None,
        focus_eligible: false,
        cloud_chat_visible: false,
        qr_recommendation_rank: None,
        api_dialect: Some("openai_chat".to_owned()),
        base_url_template: None,
        auth_scheme: None,
        extra_headers: None,
        models_path: None,
        user_variables: None,
        free_tier: None,
        plan_inclusion: None,
        retention_days: None,
        jurisdiction: Some("Fixtureland".to_owned()),
        verification_status: VerificationStatus::Verified,
        credential_acquisition: None,
        spend_limit_url: None,
        logo_key: None,
        required_request_options: None,
    }
}

pub(crate) fn model(id: &str) -> ProviderModel {
    ProviderModel {
        id: format!("row-{id}"),
        provider_id: "fixture-host-a".to_owned(),
        model_id: id.to_owned(),
        context_window_tokens: 8192,
        is_default: true,
        created_at: "t".to_owned(),
        pricing: None,
        request_options: Some(json!({"max_tokens": 1024})),
        token_factor: 1.0,
        output_overhead: 1.0,
        status: ModelStatus::Active,
        retires_at: None,
        requires_paid: false,
        qr_recommended: false,
        qr_recommendation_rank: None,
        retains_data_override: None,
        trains_on_data_override: None,
        retention_days_override: None,
        context_window_source: None,
        context_window_as_of: None,
    }
}
