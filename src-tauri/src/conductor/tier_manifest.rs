// src-tauri/src/conductor/tier_manifest.rs
//
// items.id=878 (decisions.id=938 point 4): the bundled tier manifest.
//
// A read-only table, compiled into the binary and keyed by focus_id, giving each
// shipped Focus's maximum routing tier. It is the reference the loader holds a
// DATABASE-composed shipped Focus against: shared.db is unencrypted and a
// focuses row is only data, so the row alone must not decide how far a shipped
// Focus may reach. Stage 1 (items.id=129) compared a database row only against a
// bundled .focus file, so a Focus authored only as database rows (no .focus
// file) had no reference at all.
//
// RULE (lifecycle::load_focus_definition): the loader takes the STRICTER of the
// database row's max_routing_tier, the bundled .focus file's (when one exists)
// and this manifest's. A row can narrow a shipped Focus and can never widen it.
//
// A focus_id with NO entry here gets DEFAULT_TIER, the most restrictive one
// (local_only), so adding a database row for a Focus without recording its
// ceiling here fails closed rather than loading wide. This applies to every
// focuses row, whatever its status column says: the table holds shipped content
// only (decisions.id=862), and a rule keyed on status would let a tampered row
// opt out by changing it.
//
// WHY COMPILED IN rather than a bundled data file: it cannot be missing, moved
// or unreadable at run time (the .focus lookup is silently skipped when the
// bundle directory is unset), it needs no parse step that could fail open, and
// a shipped Focus cannot be added without a release anyway. It sits BESIDE the
// .focus comparison and does not replace it: the file stays the authoring source
// of truth for a YAML Focus, and a test holds the two together.
//
// Adding a shipped Focus: add its entry here in the same change as its seed
// migration or .focus file. The drift test in lifecycle.rs (manifest_matches_every_bundled_focus_file) fails on a bundled
// .focus file with no entry or with a different ceiling.

use crate::conductor::tokens::ExternalAccess;

/// Ceiling for a focus_id with no entry: the most restrictive tier.
pub const DEFAULT_TIER: ExternalAccess = ExternalAccess::LocalOnly;

/// (focus_id, max routing tier). Mirrors the max_routing_tier of each bundled
/// .focus file under app/core_artifacts/focuses/ (YAML 1 = local_only, 2 =
/// anonymous_required, 3 = unrestricted). Keep sorted by focus_id.
pub const SHIPPED_FOCUS_TIERS: &[(&str, ExternalAccess)] = &[
    ("api-chat", ExternalAccess::Unrestricted),
    ("qr-admin", ExternalAccess::LocalOnly),
    ("quick-ask", ExternalAccess::LocalOnly),
    ("research-and-buy", ExternalAccess::AnonymousRequired),
    ("role-assessment", ExternalAccess::AnonymousRequired),
    ("writing-assistant", ExternalAccess::AnonymousRequired),
];

/// The manifest's entry for `focus_id`, if any.
pub fn manifest_tier(focus_id: &str) -> Option<ExternalAccess> {
    SHIPPED_FOCUS_TIERS
        .iter()
        .find(|(id, _)| *id == focus_id)
        .map(|(_, tier)| *tier)
}

/// The ceiling the manifest allows `focus_id`: its entry, or DEFAULT_TIER.
pub fn manifest_ceiling(focus_id: &str) -> ExternalAccess {
    manifest_tier(focus_id).unwrap_or(DEFAULT_TIER)
}

/// A database row's max_routing_tier held against the manifest: the stricter.
pub fn apply_manifest(focus_id: &str, db_tier: ExternalAccess) -> ExternalAccess {
    ExternalAccess::min(db_tier, manifest_ceiling(focus_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_cannot_widen_a_shipped_focus() {
        assert_eq!(
            apply_manifest("writing-assistant", ExternalAccess::Unrestricted),
            ExternalAccess::AnonymousRequired
        );
        assert_eq!(
            apply_manifest("writing-assistant", ExternalAccess::AnonymousPreferred),
            ExternalAccess::AnonymousRequired
        );
    }

    #[test]
    fn a_stricter_row_narrows() {
        assert_eq!(
            apply_manifest("writing-assistant", ExternalAccess::LocalOnly),
            ExternalAccess::LocalOnly
        );
        assert_eq!(
            apply_manifest("writing-assistant", ExternalAccess::AnonymousRequired),
            ExternalAccess::AnonymousRequired
        );
    }

    #[test]
    fn a_missing_entry_does_not_widen() {
        assert_eq!(manifest_tier("not-a-shipped-focus"), None);
        assert_eq!(
            apply_manifest("not-a-shipped-focus", ExternalAccess::Unrestricted),
            ExternalAccess::LocalOnly
        );
        assert_eq!(
            apply_manifest("not-a-shipped-focus", ExternalAccess::LocalOnly),
            ExternalAccess::LocalOnly
        );
    }

    #[test]
    fn the_table_is_sorted_and_has_no_duplicates() {
        let ids: Vec<&str> = SHIPPED_FOCUS_TIERS.iter().map(|(id, _)| *id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(ids, sorted);
    }
}
