-- messages_003.sql
--
-- items.id=406 (decisions.id=755) -- Privacy Guardian trigger model, the
-- provider-selection re-check trigger. When a Tier-3-bound message is
-- approved (gate3_review_status='approved'), reviewed_at_risk_rating
-- records the destination risk rating that review was actually scored
-- against (providers.risk_rating, MAX across whatever providers were
-- active in the rail at approval time -- see commands/consent.rs's
-- request_cloud_frontier_gate3_review). When the user later activates a NEW
-- provider row not covered by that review, recheck_cloud_frontier_provider_selection
-- compares the newly-active set's max risk against this stored value: only
-- a STRICTLY HIGHER risk triggers a fresh review pass -- a newly-selected
-- provider that is the same or lower risk than what was already reviewed
-- needs no re-check.
--
-- NULL for every row until this feature's write path populates it (existing
-- approved messages predating this migration have no value to backfill --
-- fabricating one would be dishonest data, same reasoning shared_011.sql
-- used for its own NULLable existing_max_permitted_tier backfill gap).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE messages ADD COLUMN reviewed_at_risk_rating INTEGER
    CHECK (reviewed_at_risk_rating IS NULL OR reviewed_at_risk_rating IN (1, 2, 3));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (3, datetime('now'),
    'items.id=406 (decisions.id=755): messages.reviewed_at_risk_rating -- the destination risk rating a Tier-3 approval was scored against, for the provider-selection re-check trigger');
