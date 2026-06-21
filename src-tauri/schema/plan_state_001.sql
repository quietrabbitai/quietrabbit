-- persistence/schema/plan_state_001.sql
-- Initial schema for plan_state.db.
-- Per-user, per-life, per-focus, per-topic encrypted database.
-- Path: /users/{user_id}/lives/{life_id}/focuses/{focus_id}/topics/{topic_id}/plan_state.db
-- Stores state for exactly ONE named topic pursuit.
-- Does not carry forward when topic closes — archived or discarded per user choice.
-- Encrypted with SQLCipher using user master key.
-- Part of Phase B data model extension (D6-226+).
--
-- Source of truth declaration:
--   outputs.db topics table = authoritative source of truth for topic metadata.
--   topic_header in this file = cache copy for offline coherence (backup/restore).
--   On conflict between topic_header and outputs.db, outputs.db governs.
--   topic_header is updated by Phase 5A and Reconciliation Boot Check.
--
-- Plan State block sensitivity model (ADR-013 Section 5.7):
--   Block-level tagging — NOT a single ceiling for the whole topic.
--   A medical block does NOT elevate general pricing research blocks.
--   Output blocks inherit highest sensitivity from dependency_refs lineage.
--   Sensitive data cannot be laundered through iterative summarisation.
--
-- PersonalTrack invariant: NEVER serialised here.
--   Re-fetched fresh from personal.db in Phase 3 INITIALIZE on every session.
--
-- Authority hierarchy (ADR-013 Section 8.9):
--   Plan State, Domain Context, Profile = authoritative.
--   Standing Summary, Retrospective, Broker Index = derived, never authoritative.

CREATE TABLE IF NOT EXISTS schema_version (
    version         INTEGER PRIMARY KEY,
    applied_at      TEXT NOT NULL,
    description     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS migration_lock (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    locked_at   TEXT,
    locked_by   TEXT
);
INSERT OR IGNORE INTO migration_lock (id) VALUES (1);

-- Topic header — single row, documents the topic this database belongs to.
-- Denormalised from outputs.db topics for offline coherence (backup/restore safety).
-- outputs.db topics table is authoritative — this is a cache copy only.
-- name and placeholder_name mirrored from topics table — updated on rename.
-- current_phase: human-readable phase label for resume summary generation.
--   Set by Conductor at each phase transition — for display only, not logic.
-- session_count: incremented at each Phase 3 INITIALIZE for this topic.
-- Resuming a topic: Conductor reads this header first before opening any blocks.
CREATE TABLE IF NOT EXISTS topic_header (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    topic_id            TEXT NOT NULL,
    focus_id            TEXT NOT NULL,
    life_id             TEXT NOT NULL,
    name                TEXT,
    placeholder_name    TEXT NOT NULL,
    lifecycle_state     TEXT NOT NULL DEFAULT 'active',
    current_phase       TEXT,
    session_count       INTEGER NOT NULL DEFAULT 0,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

-- Plan state blocks — topic-scoped pursuit context.
-- block_type values:
--   decision           — a choice made within this pursuit
--   research           — information gathered for this pursuit
--   action_output      — output from a Conductor-assigned action
--   user_note          — user-authored note within this pursuit
--   system_observation — Conductor observation, inferred_by_system=1
-- content: accumulated context for this block — may span multiple sessions.
-- visibility_scope + transformation: two-dimension model per ADR-013 Section 6.2.
-- sensitivity_preset: named shortcut. NULL = custom combination.
-- dependency_refs: JSON array of block_ids this block references.
--   Output blocks inherit highest sensitivity from dependency_refs lineage.
--   Sensitive data cannot be laundered through iterative summarisation.
-- inferred_by_system: 1 = Conductor-proposed. 0 = user-authored.
-- focus_run_id: which focus run last wrote this block.
-- archived_at: non-null when topic closes. Blocks retained, not deleted.
--   User preference determines archive vs discard at topic close.
CREATE TABLE IF NOT EXISTS plan_state_blocks (
    id                  TEXT PRIMARY KEY,
    block_type          TEXT NOT NULL
                            CHECK (block_type IN (
                                'decision', 'research', 'action_output',
                                'user_note', 'system_observation'
                            )),
    content             TEXT NOT NULL,
    visibility_scope    TEXT NOT NULL DEFAULT 'tier2_permitted'
                            CHECK (visibility_scope IN (
                                'tier_1_only', 'anonymous_tier2',
                                'tier2_permitted', 'tier3_permitted'
                            )),
    transformation      TEXT NOT NULL DEFAULT 'generalize_ok'
                            CHECK (transformation IN (
                                'no_generalize', 'generalize_ok',
                                'anonymize_ok', 'no_transform'
                            )),
    sensitivity_preset  TEXT
                            CHECK (sensitivity_preset IS NULL OR
                                sensitivity_preset IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    relevance_tags      TEXT NOT NULL DEFAULT '[]',
    token_estimate      INTEGER NOT NULL DEFAULT 0,
    dependency_refs     TEXT NOT NULL DEFAULT '[]',
    inferred_by_system  INTEGER NOT NULL DEFAULT 1,
    focus_run_id        TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    archived_at         TEXT,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_psb_active
    ON plan_state_blocks (block_type, updated_at DESC)
    WHERE archived_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_psb_retrieval
    ON plan_state_blocks (visibility_scope, token_estimate)
    WHERE archived_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_psb_focus_run
    ON plan_state_blocks (focus_run_id, created_at DESC);

-- Handoff tokens — for Awaiting state (Tier 3 external action dependency).
-- Awaiting invariant: topic_id != null required — enforced at application layer.
--   Quick Ask can never enter Awaiting (is_quick_ask invariant).
-- topic_id: explicit ownership — makes Boot Check reconciliation unambiguous.
--   Boot Check reads topic_storage_locations in outputs.db, opens only live DBs.
-- action_id: the Conductor-assigned action that produced this handoff.
-- expected_return_schema: JSON describing required shape of return result.
-- consumed_at: set when return result validated and accepted. Terminal state.
-- expired_at: set by Boot Check when expiry_at passes without return.
--   Boot Check transitions topic to paused, reason: dependency_timeout.
--   Dashboard shows: "External update timed out — retry or resume manually."
CREATE TABLE IF NOT EXISTS handoff_tokens (
    id                      TEXT PRIMARY KEY,
    topic_id                TEXT NOT NULL,
    focus_run_id            TEXT NOT NULL,
    action_id               TEXT NOT NULL,
    expected_return_schema  TEXT NOT NULL DEFAULT '{}',
    created_at              TEXT NOT NULL,
    expiry_at               TEXT NOT NULL,
    consumed_at             TEXT,
    expired_at              TEXT,
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_handoff_tokens_active
    ON handoff_tokens (expiry_at)
    WHERE consumed_at IS NULL AND expired_at IS NULL;

-- Soft ceiling notification state — single row (id=1).
-- ADR-013 Section 3.3: when accumulated Plan State exceeds threshold,
-- surface a calm consolidation prompt. NOT automatic — user controls response.
-- System NEVER automatically closes or consolidates a topic.
-- current_token_estimate: sum of token_estimate across all active (non-archived) blocks.
--   Updated by plan_state_store.py after each block write.
-- ceiling_threshold: default 32000 tokens. User-configurable per topic.
-- notification_sent_at: when the prompt was last surfaced to the user.
-- user_response: NULL = not yet responded.
--   'consolidate' = user wants to review and consolidate.
--   'continue'    = user chose to continue without consolidating.
-- CHECK uses OR IS NULL pattern — IN (..., NULL) does not evaluate correctly in SQL.
CREATE TABLE IF NOT EXISTS state_ceiling_status (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    current_token_estimate  INTEGER NOT NULL DEFAULT 0,
    ceiling_threshold       INTEGER NOT NULL DEFAULT 32000,
    notification_sent_at    TEXT,
    user_response           TEXT
                                CHECK (user_response IN ('consolidate', 'continue')
                                    OR user_response IS NULL),
    extra_metadata          TEXT NOT NULL DEFAULT '{}'
);

INSERT OR IGNORE INTO state_ceiling_status
    (id, current_token_estimate, ceiling_threshold)
VALUES (1, 0, 32000);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'Initial plan_state.db schema: topic_header, plan_state_blocks, handoff_tokens, state_ceiling_status');
