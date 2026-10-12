// src-tauri/src/spend/codes.rs
//
// The single registry of the codes the spend subsystem emits (items.id=710).
// Every warning, stop, state and cost-basis statement the backend makes is one
// of these stable strings plus structured parameters; the frontend maps a code
// to an i18n key when the consuming screen lands (items.id=693, 580, 696).
// There is NO user-facing prose in this module or in spend/ at all.
//
// The set is pinned by `code_set_is_pinned` below: renaming or removing a code
// fails that test, because consumers rely on the string values.
//
// Prefixes:
//   warning.*      disclosure warnings (each has a tier: notice or strong)
//   spend_block.*  a call was stopped or parked before any network traffic
//   cost_basis.*   what a cost figure means
//   price_state.*  whether and how well a price is known
//   limit_state.*  where a limit stands
//   consent_state.* where a provider's consent stands

use serde::{Deserialize, Serialize};

/// How loudly a warning should be presented. Ordered: None < Notice < Strong.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum WarningTier {
    None,
    Notice,
    Strong,
}

impl WarningTier {
    /// One level louder, saturating at Strong.
    pub fn raised(self) -> Self {
        match self {
            WarningTier::None => WarningTier::Notice,
            WarningTier::Notice | WarningTier::Strong => WarningTier::Strong,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WarningTier::None => "none",
            WarningTier::Notice => "notice",
            WarningTier::Strong => "strong",
        }
    }
}

/// Every code the spend subsystem emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    // -- disclosure warnings ------------------------------------------------
    /// The provider's posture has not been verified by curation. Strong.
    WarnUnverifiedProvider,
    /// The provider's verification is out of date. Notice.
    WarnVerificationStale,
    /// The provider trains on submitted data by default. Strong.
    WarnTrainsOnDataByDefault,
    /// A model overrides the provider and trains on data. Strong.
    WarnModelTrainsOverride,
    /// The provider retains submitted data. Notice.
    WarnRetainsData,
    /// How long the provider retains data is not stated. Notice.
    WarnRetentionUnstated,
    /// The provider requires an account, so requests are not anonymous. Notice.
    WarnNotAnonymous,
    /// The provider's commitment is policy only, not contractual. Notice.
    WarnCommitmentPolicyOnly,
    /// Using this provider costs money. Notice.
    WarnCostMetered,
    /// The price of the selected model is not known. Strong.
    WarnPriceUnknown,
    /// The price is older than the staleness window. Notice.
    WarnPriceStale,
    /// The curated destination risk rating is medium. Notice (rating floor).
    WarnDestinationRiskMedium,
    /// The curated destination risk rating is high. Strong (rating floor).
    WarnDestinationRiskHigh,
    /// The content comes from a sensitive Persona. Raises the overall tier.
    WarnSensitivePersonaContent,
    /// The selected model came from the provider's live list and QR has not
    /// reviewed it: its price is unknown and its privacy is unverified, and
    /// the consent the user gave covers the reviewed models only (items.id=711).
    /// Strong.
    WarnModelUncurated,
    /// The provider's last list no longer includes this model; it may be
    /// retired (items.id=711). Notice.
    WarnModelMayBeRetired,
    /// The model is marked deprecated, or has a retirement date (items.id=711).
    /// Notice.
    WarnModelDeprecated,

    // -- a call was stopped or parked before any network traffic --------------
    BlockConsentRequired,
    BlockReconsentRequired,
    BlockPriceUnknown,
    BlockCapRequired,
    BlockLimitReached,
    /// An unattended run has no approved per-run budget; the run parks.
    BlockBudgetApprovalRequired,
    /// The provider asked for a wait longer than QR will sleep; the run parks.
    BlockRateLimitedLongWait,
    /// The spend ledger could not be read or written, so nothing can be
    /// enforced. The call is not made (fail closed).
    BlockLedgerUnavailable,
    /// The selected model is marked retired. Nothing is sent and the default
    /// is not substituted (items.id=711).
    BlockModelRetired,
    /// The selected model is neither a reviewed model nor in the user's live
    /// list. Nothing is sent and the default is not substituted (items.id=711).
    BlockModelUnavailable,
    /// An uncurated model has no output token limit the user chose, and the
    /// client will not send without one (items.id=711).
    BlockModelOutputLimitRequired,

    // -- cost basis, price state, limit state, consent state ------------------
    CostBasisListPriceEstimate,
    PriceStateKnown,
    PriceStateUnknown,
    PriceStateStale,
    LimitStateOk,
    LimitStateWarn,
    LimitStateReached,
    ConsentStateNone,
    ConsentStateCurrent,
    ConsentStateStale,
    ConsentStateRevoked,
}

impl Code {
    /// Every code, for the pin test and for consumers that enumerate them.
    pub const ALL: &'static [Code] = &[
        Code::WarnUnverifiedProvider,
        Code::WarnVerificationStale,
        Code::WarnTrainsOnDataByDefault,
        Code::WarnModelTrainsOverride,
        Code::WarnRetainsData,
        Code::WarnRetentionUnstated,
        Code::WarnNotAnonymous,
        Code::WarnCommitmentPolicyOnly,
        Code::WarnCostMetered,
        Code::WarnPriceUnknown,
        Code::WarnPriceStale,
        Code::WarnDestinationRiskMedium,
        Code::WarnDestinationRiskHigh,
        Code::WarnSensitivePersonaContent,
        Code::WarnModelUncurated,
        Code::WarnModelMayBeRetired,
        Code::WarnModelDeprecated,
        Code::BlockConsentRequired,
        Code::BlockReconsentRequired,
        Code::BlockPriceUnknown,
        Code::BlockCapRequired,
        Code::BlockLimitReached,
        Code::BlockBudgetApprovalRequired,
        Code::BlockRateLimitedLongWait,
        Code::BlockLedgerUnavailable,
        Code::BlockModelRetired,
        Code::BlockModelUnavailable,
        Code::BlockModelOutputLimitRequired,
        Code::CostBasisListPriceEstimate,
        Code::PriceStateKnown,
        Code::PriceStateUnknown,
        Code::PriceStateStale,
        Code::LimitStateOk,
        Code::LimitStateWarn,
        Code::LimitStateReached,
        Code::ConsentStateNone,
        Code::ConsentStateCurrent,
        Code::ConsentStateStale,
        Code::ConsentStateRevoked,
    ];

    /// The stable string value consumers rely on.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::WarnUnverifiedProvider => "warning.unverified_provider",
            Code::WarnVerificationStale => "warning.verification_stale",
            Code::WarnTrainsOnDataByDefault => "warning.trains_on_data_by_default",
            Code::WarnModelTrainsOverride => "warning.model_trains_override",
            Code::WarnRetainsData => "warning.retains_data",
            Code::WarnRetentionUnstated => "warning.retention_unstated",
            Code::WarnNotAnonymous => "warning.not_anonymous",
            Code::WarnCommitmentPolicyOnly => "warning.commitment_policy_only",
            Code::WarnCostMetered => "warning.cost_metered",
            Code::WarnPriceUnknown => "warning.price_unknown",
            Code::WarnPriceStale => "warning.price_stale",
            Code::WarnDestinationRiskMedium => "warning.destination_risk_medium",
            Code::WarnDestinationRiskHigh => "warning.destination_risk_high",
            Code::WarnSensitivePersonaContent => "warning.sensitive_persona_content",
            Code::WarnModelUncurated => "warning.model_uncurated",
            Code::WarnModelMayBeRetired => "warning.model_may_be_retired",
            Code::WarnModelDeprecated => "warning.model_deprecated",
            Code::BlockConsentRequired => "spend_block.consent_required",
            Code::BlockReconsentRequired => "spend_block.reconsent_required",
            Code::BlockPriceUnknown => "spend_block.price_unknown",
            Code::BlockCapRequired => "spend_block.cap_required",
            Code::BlockLimitReached => "spend_block.limit_reached",
            Code::BlockBudgetApprovalRequired => "spend_block.budget_approval_required",
            Code::BlockRateLimitedLongWait => "spend_block.rate_limited_long_wait",
            Code::BlockLedgerUnavailable => "spend_block.ledger_unavailable",
            Code::BlockModelRetired => "spend_block.model_retired",
            Code::BlockModelUnavailable => "spend_block.model_unavailable",
            Code::BlockModelOutputLimitRequired => "spend_block.model_output_limit_required",
            Code::CostBasisListPriceEstimate => "cost_basis.list_price_estimate",
            Code::PriceStateKnown => "price_state.known",
            Code::PriceStateUnknown => "price_state.unknown",
            Code::PriceStateStale => "price_state.stale",
            Code::LimitStateOk => "limit_state.ok",
            Code::LimitStateWarn => "limit_state.warn",
            Code::LimitStateReached => "limit_state.reached",
            Code::ConsentStateNone => "consent_state.none",
            Code::ConsentStateCurrent => "consent_state.current",
            Code::ConsentStateStale => "consent_state.stale",
            Code::ConsentStateRevoked => "consent_state.revoked",
        }
    }

    /// The tier a warning code carries on its own. Non-warning codes are None.
    pub fn tier(self) -> WarningTier {
        match self {
            Code::WarnUnverifiedProvider
            | Code::WarnTrainsOnDataByDefault
            | Code::WarnModelTrainsOverride
            | Code::WarnModelUncurated
            | Code::WarnPriceUnknown
            | Code::WarnDestinationRiskHigh => WarningTier::Strong,
            Code::WarnVerificationStale
            | Code::WarnRetainsData
            | Code::WarnRetentionUnstated
            | Code::WarnNotAnonymous
            | Code::WarnCommitmentPolicyOnly
            | Code::WarnCostMetered
            | Code::WarnPriceStale
            | Code::WarnModelMayBeRetired
            | Code::WarnModelDeprecated
            | Code::WarnDestinationRiskMedium => WarningTier::Notice,
            // The sensitive-persona code does not carry a tier of its own: it
            // raises the overall tier (see disclosure.rs).
            _ => WarningTier::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract with every consumer. A rename or removal fails here on
    /// purpose; adding a code means adding it to this list too.
    #[test]
    fn code_set_is_pinned() {
        let actual: Vec<&str> = Code::ALL.iter().map(|c| c.as_str()).collect();
        let expected = vec![
            "warning.unverified_provider",
            "warning.verification_stale",
            "warning.trains_on_data_by_default",
            "warning.model_trains_override",
            "warning.retains_data",
            "warning.retention_unstated",
            "warning.not_anonymous",
            "warning.commitment_policy_only",
            "warning.cost_metered",
            "warning.price_unknown",
            "warning.price_stale",
            "warning.destination_risk_medium",
            "warning.destination_risk_high",
            "warning.sensitive_persona_content",
            "warning.model_uncurated",
            "warning.model_may_be_retired",
            "warning.model_deprecated",
            "spend_block.consent_required",
            "spend_block.reconsent_required",
            "spend_block.price_unknown",
            "spend_block.cap_required",
            "spend_block.limit_reached",
            "spend_block.budget_approval_required",
            "spend_block.rate_limited_long_wait",
            "spend_block.ledger_unavailable",
            "spend_block.model_retired",
            "spend_block.model_unavailable",
            "spend_block.model_output_limit_required",
            "cost_basis.list_price_estimate",
            "price_state.known",
            "price_state.unknown",
            "price_state.stale",
            "limit_state.ok",
            "limit_state.warn",
            "limit_state.reached",
            "consent_state.none",
            "consent_state.current",
            "consent_state.stale",
            "consent_state.revoked",
        ];
        assert_eq!(actual, expected);
    }

    #[test]
    fn codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in Code::ALL {
            assert!(seen.insert(c.as_str()), "duplicate code {}", c.as_str());
        }
    }

    /// `ALL` must list every variant: the exhaustive match in `as_str` plus
    /// this length check keeps the two in step.
    #[test]
    fn all_lists_every_variant() {
        // 17 warnings + 11 blocks + 1 cost basis + 3 price states
        // + 3 limit states + 4 consent states.
        assert_eq!(Code::ALL.len(), 39);
    }

    #[test]
    fn tiers_match_the_documented_table() {
        assert_eq!(Code::WarnUnverifiedProvider.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnTrainsOnDataByDefault.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnModelTrainsOverride.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnPriceUnknown.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnDestinationRiskHigh.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnModelUncurated.tier(), WarningTier::Strong);
        assert_eq!(Code::WarnModelMayBeRetired.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnModelDeprecated.tier(), WarningTier::Notice);
        assert_eq!(Code::BlockModelRetired.tier(), WarningTier::None);
        assert_eq!(Code::WarnVerificationStale.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnRetainsData.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnRetentionUnstated.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnNotAnonymous.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnCommitmentPolicyOnly.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnCostMetered.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnPriceStale.tier(), WarningTier::Notice);
        assert_eq!(Code::WarnDestinationRiskMedium.tier(), WarningTier::Notice);
        assert_eq!(Code::BlockConsentRequired.tier(), WarningTier::None);
    }

    #[test]
    fn raising_a_tier_saturates() {
        assert_eq!(WarningTier::None.raised(), WarningTier::Notice);
        assert_eq!(WarningTier::Notice.raised(), WarningTier::Strong);
        assert_eq!(WarningTier::Strong.raised(), WarningTier::Strong);
    }
}
