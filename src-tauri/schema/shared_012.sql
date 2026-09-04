-- shared_012.sql
--
-- items.id=406 (decisions.id=753/757) -- Privacy Guardian risk-routing
-- redesign. Replaces the hardcoded "Tier 3 destination always forces High
-- review" rule with a per-provider risk rating read live from this table,
-- so a provider's actual posture (not its category label) drives routing.
--
-- risk_rating is intentionally its own dedicated column, not folded into
-- documentation_gate's freeform JSON (items.id=409 finding: documentation_
-- gate is display-only, ToS/retention citation text -- overloading it would
-- make the routing-critical value unqueryable without a JSON extract, and
-- would conflate "what we tell the user" with "what gates content"). This
-- was confirmed the accepted approach over a second table (decisions.id=757
-- / items.id=409 CLOSED COMPLETE notes).
--
-- Values: 1=Low, 2=Medium, 3=High -- deliberately parallel to ReviewTier's
-- own three values (conductor/privacy/types.rs), since a provider's risk
-- rating feeds directly into the same High-forcing OR-condition
-- assign_review_tier already uses for content severity. DEFAULT 3 (High) is
-- the fail-safe for any future provider row added without an explicit
-- rating -- unknown risk is never silently treated as low.
--
-- Seed values below preserve today's actual behavior exactly: Tier 3
-- providers (claude/chatgpt/gemini) keep rating 3 (High) -- this is NOT a
-- fresh per-provider risk assessment, just carrying forward the existing
-- always-High outcome onto the new live mechanism. Tier 2 (duckai) gets
-- rating 1 (Low), consistent with decisions.id=682's symmetry note: Tier 2
-- destinations are anonymous/no-login by construction, so destination-
-- attribution risk is structurally absent -- no forced floor, same as today.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE tier3_providers
    ADD COLUMN risk_rating INTEGER NOT NULL DEFAULT 3
        CHECK (risk_rating IN (1, 2, 3));

UPDATE tier3_providers SET risk_rating = 1 WHERE tier = 2;
UPDATE tier3_providers SET risk_rating = 3 WHERE tier = 3;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (12, datetime('now'),
    'items.id=406 (decisions.id=753): tier3_providers.risk_rating -- live per-provider destination-risk rating (1=Low/2=Medium/3=High) replacing the hardcoded Tier-3-always-High routing rule; seeded to preserve todays exact outcome');
