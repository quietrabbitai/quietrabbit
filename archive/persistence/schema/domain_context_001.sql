-- persistence/schema/domain_context_001.sql
-- Initial schema for domain_context.db.
-- Per-user, per-life, per-focus encrypted database.
-- Path: /users/{user_id}/lives/{life_id}/focuses/{focus_id}/domain_context.db
-- Stores accumulated standing knowledge for one focus domain.
-- Persists across ALL topics within the focus — never topic-scoped.
-- Never contains documents or named artifacts — those are Assets.
-- Encrypted with SQLCipher using user master key.
-- Part of Phase B data model extension (D6-226+).
--
-- Two-dimension sensitivity model per ADR-013 Section 6.2:
--   visibility_scope:  tier_1_only | anonymous_tier2 | tier2_permitted | tier3_permitted
--   transformation:    no_generalize | generalize_ok | anonymize_ok | no_transform
--   sensitivity_preset: standard | sensitive | private | locked (named shortcut)
--
-- Extraction authority: generalisation pass ONLY (ADR-013 Section 6.6).
--   "The generalisation pass is the only mechanism that writes to Domain Context."
--   Users review and approve extraction cards — they do not author blocks directly.
--   pending_extractions is the staging area. provenance_log records all approvals.
--
-- Every block carries full provenance: source_topic_id, extraction_event_id.
-- inferred_by_system: 1 = Mode 1 classification. 0 = user explicitly calibrated.
-- Retrieval Eligibility Check pre-filters on visibility_scope before Gate1.
-- Gate1 is the single abstraction authority — Retrieval Eligibility Check is not.
--
-- Circular FK note: domain_context_blocks.extraction_event_id → provenance_log.id
--   AND provenance_log.approved_block_id → domain_context_blocks.id.
--   Insertion order at application layer resolves this:
--     1. INSERT provenance_log with approved_block_id = NULL
--     2. INSERT domain_context_blocks with extraction_event_id = provenance_log.id
--     3. UPDATE provenance_log SET approved_block_id = domain_context_blocks.id
--   This is enforced in domain_context_store.py write_approved_extraction().

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

-- Provenance log — MUST be created before domain_context_blocks
-- due to FK reference from domain_context_blocks.extraction_event_id.
-- NEVER deleted — permanent audit trail retained even after block revocation.
-- approved_block_id: NULL if approval_action='discarded', set after block write.
-- edited_content: the user's edited version if approval_action='edited'. NULL otherwise.
-- proposed_content: the raw generalisation pass output before user review.
CREATE TABLE IF NOT EXISTS provenance_log (
    id                  TEXT PRIMARY KEY,
    source_topic_id     TEXT NOT NULL,
    source_focus_run_id TEXT NOT NULL,
    proposed_content    TEXT NOT NULL,
    approval_action     TEXT NOT NULL
                            CHECK (approval_action IN (
                                'approved', 'edited', 'discarded'
                            )),
    edited_content      TEXT,
    approved_block_id   TEXT,
    approved_at         TEXT NOT NULL,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_provenance_topic
    ON provenance_log (source_topic_id, approved_at DESC);

-- Domain context blocks — accumulated focus-level knowledge.
-- content: the extracted generalised preference or strategy.
--   Never contains instance-specific identifiers from source topic.
-- relevance_tags: JSON array of taxonomy tags for retrieval scoring.
-- token_estimate: pre-computed for context budget management by Memory Broker.
-- dependency_refs: JSON array of block_ids this block was derived from.
--   Output blocks inherit highest sensitivity from dependency_refs lineage.
-- source_topic_id: provenance — which topic this was extracted from.
-- extraction_event_id: FK to provenance_log.id — the approval event.
--   See circular FK note at top of file for insertion order.
-- standing_summary_eligible: 1 = this block contributes to standing summary cache.
-- revoked_at: non-null if user deleted this block.
--   Provenance log entry retained after revocation — permanent audit trail.
-- revocation_reason: why the block was revoked. NULL = not revoked.
--   Values: user_deleted | superseded | privacy_request | classification_error
-- inferred_by_system: 1 = Mode 1 conservative classification default.
--   0 = user explicitly calibrated via Mode 2 or Mode 3 response.
CREATE TABLE IF NOT EXISTS domain_context_blocks (
    id                          TEXT PRIMARY KEY,
    content                     TEXT NOT NULL,
    visibility_scope            TEXT NOT NULL DEFAULT 'tier2_permitted'
                                    CHECK (visibility_scope IN (
                                        'tier_1_only', 'anonymous_tier2',
                                        'tier2_permitted', 'tier3_permitted'
                                    )),
    transformation              TEXT NOT NULL DEFAULT 'generalize_ok'
                                    CHECK (transformation IN (
                                        'no_generalize', 'generalize_ok',
                                        'anonymize_ok', 'no_transform'
                                    )),
    sensitivity_preset          TEXT NOT NULL DEFAULT 'standard'
                                    CHECK (sensitivity_preset IN (
                                        'standard', 'sensitive', 'private', 'locked'
                                    )),
    relevance_tags              TEXT NOT NULL DEFAULT '[]',
    token_estimate              INTEGER NOT NULL DEFAULT 0,
    dependency_refs             TEXT NOT NULL DEFAULT '[]',
    source_topic_id             TEXT NOT NULL,
    extraction_event_id         TEXT NOT NULL REFERENCES provenance_log(id),
    inferred_by_system          INTEGER NOT NULL DEFAULT 1,
    standing_summary_eligible   INTEGER NOT NULL DEFAULT 1,
    revoked_at                  TEXT,
    revocation_reason           TEXT
                                    CHECK (revocation_reason IS NULL OR
                                        revocation_reason IN (
                                        'user_deleted', 'superseded',
                                        'privacy_request', 'classification_error'
                                    )),
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL,
    extra_metadata              TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_dc_blocks_active
    ON domain_context_blocks (sensitivity_preset, standing_summary_eligible)
    WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_dc_blocks_retrieval
    ON domain_context_blocks (visibility_scope, token_estimate)
    WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_dc_blocks_source_topic
    ON domain_context_blocks (source_topic_id);

-- Standing summary cache — derived artifact, NEVER authoritative.
-- Single row (id=1). Regenerated after any domain_context_blocks write or revocation.
-- Pre-seeded at database creation — always present, initially empty.
-- token_count: enforced ceiling at assembly time by Memory Broker.
-- invalidated_at: set when blocks change, cleared after regeneration.
-- source_block_ids: JSON array of block IDs contributing to this summary.
-- Content: the assembled standing summary text for Tier A always-loaded retrieval.
CREATE TABLE IF NOT EXISTS standing_summary (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    content             TEXT NOT NULL DEFAULT '',
    token_count         INTEGER NOT NULL DEFAULT 0,
    source_block_ids    TEXT NOT NULL DEFAULT '[]',
    generated_at        TEXT NOT NULL,
    invalidated_at      TEXT
);

INSERT OR IGNORE INTO standing_summary
    (id, content, token_count, source_block_ids, generated_at)
VALUES (1, '', 0, '[]', datetime('now'));

-- Pending extraction cards — encrypted temporary staging area.
-- Written by Phase 5B generalisation pass. Purged aggressively after review.
-- Surfaced to user as editable key-value cards before any Domain Context write.
-- Extraction authority invariant: no Domain Context block is written without
--   passing through pending_extractions → user review → provenance_log.
-- status: pending_review → approved or discarded (terminal).
-- proposed_preset: the generalisation pass's suggested sensitivity preset.
--   User may change before approving.
-- provenance_log growth note: provenance_log is retained permanently (audit trail).
--   pending_extractions rows are purged after review — aggressive cleanup expected.
CREATE TABLE IF NOT EXISTS pending_extractions (
    id                  TEXT PRIMARY KEY,
    source_topic_id     TEXT NOT NULL,
    source_focus_run_id TEXT NOT NULL,
    proposed_content    TEXT NOT NULL,
    proposed_preset     TEXT NOT NULL DEFAULT 'standard'
                            CHECK (proposed_preset IN (
                                'standard', 'sensitive', 'private', 'locked'
                            )),
    status              TEXT NOT NULL DEFAULT 'pending_review'
                            CHECK (status IN (
                                'pending_review', 'approved', 'discarded'
                            )),
    created_at          TEXT NOT NULL,
    reviewed_at         TEXT,
    extra_metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_pending_extractions_status
    ON pending_extractions (status, created_at DESC)
    WHERE status = 'pending_review';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (1, datetime('now'),
    'Initial domain_context.db schema: provenance_log, domain_context_blocks, standing_summary, pending_extractions');
