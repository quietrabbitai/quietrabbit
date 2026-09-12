-- shared_016.sql
--
-- items.id=465 (design approved items.id=464, chat_session_handoffs.id=316)
-- -- removes the last hardcoded-provider-literal patterns out of
-- commands/tier2.rs and conductor/executor.rs (VALID_TIER2_PROVIDERS array,
-- the `match tier2_provider_preference { Some("mistral") => ..., Some("groq")
-- => ... }` dispatch, and select_model()/get_context_window()'s hardcoded
-- Tier 2 model-id/context-window literals). Those Rust-side fixes need two
-- new catalog surfaces on providers to read from instead:
--
--   1. qr_recommended: which providers QR itself suggests, per
--      provider_type "slot" -- NOT a cross-slot rankable field (a
--      qr_recommended=1 cloud_inference_api row and a qr_recommended=1
--      split_screen_web row are two independent recommendations, one per
--      slot, never compared against each other). Interpreted jointly with
--      provider_type, exactly like qr_internal_eligible is interpreted
--      jointly with nothing else (that flag has no slot concept) -- this
--      one does, so future onboarding UI reading this column must group by
--      provider_type first, never treat qr_recommended as a single global
--      ranking. DEFAULT 0, same unmarked-is-excluded convention
--      qr_internal_eligible established (shared_013.sql).
--
--   2. privacy_commitment_basis: whether a provider's privacy posture is
--      contractually committed (DPA/Services Agreement language, the kind
--      of sourced claim shared_014.sql's groq curation already cites) or
--      merely descriptive policy prose with no contractual backing.
--      Nullable, human-curated only -- never derived or parsed out of
--      documentation_gate's freeform research text, same "decided flag
--      column, not a formula over other columns" rule shared_013.sql's
--      header already established for is_anonymous/retains_data/etc.
--      NULL means "not yet assessed", a meaningfully different state from
--      either enum value -- same reasoning shared_013.sql's
--      user_privacy_summary NULL-vs-'{}' distinction already used.
--
--   3. performance_profile: throughput/latency class (JSON, nullable),
--      decoupled from hardware_requirement -- hardware_requirement stays
--      exactly what it was (Tier 1/1.5 local-install sizing: min RAM/VRAM
--      class, TEXT JSON, nullable, per shared_013.sql's own header, not
--      touched by this migration), while performance_profile can describe
--      any provider row's throughput/latency class regardless of whether
--      it has hardware requirements at all (a cloud_inference_api row has
--      no install footprint but very much has a throughput class worth
--      recording). Same JSON-escape-hatch rationale as hardware_requirement
--      itself: objectively measurable data, not a curated flag.
--
-- provider_models (closes B3, partially closes B4 -- see below): the
-- catalog select_model()/get_context_window() now read instead of matching
-- on literal provider-id/model-id strings. id is "provider_id:model_id"
-- literally -- the exact opaque string executor.rs's select_model() already
-- returns and get_context_window() already keys on today (e.g.
-- "groq:llama-3.1-8b-instant"), so no new parsing/splitting logic is needed
-- at either read site: get_default_model(provider_id) resolves the Tier 2
-- dispatch's chosen model, get_model(id) resolves a context window by the
-- same full id already flowing through the rest of the step-execution path.
-- is_default + the partial unique index below is this table's only
-- "current model for provider X" concept -- deliberately no richer
-- versioning/history model, matching decisions.id=710(b)'s release-bundled-
-- catalog precedent for providers itself.
--
-- B4 (get_context_window()) is only PARTIALLY closed by this migration
-- (confirmed acceptable scope, chat_session_handoffs.id=316): the 3 local
-- Ollama model ids (llama3.2:3b, llama3.1:8b, qwen2.5:7b) have no
-- providers row to hang a provider_models entry off yet -- Ollama-as-Tier1
-- wiring (a separate, larger initiative, explicitly out of scope here) is
-- what would give local models real provider rows. Rust's own
-- get_context_window() keeps a 3-entry hardcoded fallback map for exactly
-- these 3 ids, checked only after a provider_models lookup misses -- not
-- a full close of B4, stated as such in this session's own handoff.
--
-- B5 (user_provider_preference.local_model_version), B6/B7 (Ollama-as-
-- Tier1 OllamaClient/OllamaSidecar wiring), and renaming the persisted
-- TIER2_KEY_TYPE = "tier2" string in integration_keys.db rows are all
-- explicitly out of scope for this migration -- see this session's own
-- chat_session_handoffs row for the full scoping rationale, not repeated
-- here.
--
-- CURATION PASS (same migration, per shared_014.sql's own "unpushed local
-- commit" precedent for folding curation into the seeding migration rather
-- than a follow-up): qr_recommended=1 for groq and mistral (the two
-- existing cloud_inference_api rows -- Tier 1.5's "user choice" set,
-- CLAUDE.md); one provider_models row each, using the exact values
-- select_model()/get_context_window() had hardcoded before this fix;
-- privacy_commitment_basis='contractual' for groq only, sourced from its
-- Services Agreement / DPA language already cited in shared_014.sql's own
-- documentation_gate for that row. Mistral's privacy_commitment_basis is
-- left NULL deliberately -- shared_014.sql's own Mistral curation never
-- assessed contractual-vs-policy-only basis specifically, and this session
-- was not scoped to do that research fresh; NULL honestly represents "not
-- yet assessed", not a guessed value. DeepInfra/Cerebras rows are
-- deliberately NOT seeded here -- a separate future dispatch once Jason
-- picks adoption (out of scope, per this session's own instructions).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN qr_recommended INTEGER NOT NULL DEFAULT 0
    CHECK (qr_recommended IN (0, 1));

ALTER TABLE providers ADD COLUMN privacy_commitment_basis TEXT
    CHECK (privacy_commitment_basis IS NULL
        OR privacy_commitment_basis IN ('contractual', 'policy_only'));

ALTER TABLE providers ADD COLUMN performance_profile TEXT;

CREATE TABLE IF NOT EXISTS provider_models (
    id                      TEXT PRIMARY KEY,
    provider_id             TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_id                TEXT NOT NULL,
    context_window_tokens   INTEGER NOT NULL,
    is_default              INTEGER NOT NULL DEFAULT 0
                                CHECK (is_default IN (0, 1)),
    created_at              TEXT NOT NULL
);

-- At most one default model per provider -- the row select_model() resolves
-- via "WHERE provider_id = ? AND is_default = 1".
CREATE UNIQUE INDEX IF NOT EXISTS idx_provider_models_default
    ON provider_models (provider_id)
    WHERE is_default = 1;

UPDATE providers SET qr_recommended = 1 WHERE id IN ('groq', 'mistral');

UPDATE providers SET privacy_commitment_basis = 'contractual' WHERE id = 'groq';

INSERT INTO provider_models
    (id, provider_id, model_id, context_window_tokens, is_default, created_at)
VALUES
    ('groq:llama-3.1-8b-instant', 'groq', 'llama-3.1-8b-instant', 8192, 1, datetime('now')),
    ('mistral:mistral-small-latest', 'mistral', 'mistral-small-latest', 32768, 1, datetime('now'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (16, datetime('now'),
    'items.id=465: providers.qr_recommended/privacy_commitment_basis/performance_profile columns + provider_models table (closes B3, partially closes B4) -- removes hardcoded Tier 2 provider/model literals from commands/tier2.rs and conductor/executor.rs. Curation: groq+mistral marked qr_recommended, their existing hardcoded model ids seeded as default provider_models rows, groq marked privacy_commitment_basis=contractual (mistral left NULL, not yet assessed)');
