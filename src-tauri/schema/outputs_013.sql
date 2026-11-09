-- outputs_013.sql
--
-- items.id=881 -- per-persona session and case stores in the persona's
-- outputs.db (decisions.id=939 section 1, design handoff
-- chat_session_handoffs.id=625): session_variables, case_records,
-- case_entities, case_strategies, config_deltas, config_delta_entities.
-- Schema only. The typed store API is persistence/session_store.rs and
-- persistence/case_store.rs. Nothing reads or writes these tables from the
-- executor yet (seeding is items.id=886, store_op writers items.id=887,
-- lifecycle, resume pin and retention items.id=888).
--
-- WHY outputs.db AND NOT ANOTHER FILE (decisions.id=939 section 1): not
-- personal.db (persona sync, PersonalTrack, extraction target), not
-- domain_context.db or plan_state.db (MemoryBroker reads them into
-- persona_context), not a new .db file, not shared.db (decisions.id=862).
-- The file is already one encrypted database per persona, so persona
-- isolation is the file boundary and no row carries a persona_id. This
-- supersedes the tech_support_001.sql file name in decisions.id=508 under
-- decisions.id=857. The tables stay dedicated, as 508 says.
--
-- NO FOREIGN KEYS TO focus_runs OR focus_run_snapshots: items.id=633 rebuilds
-- both. Every run reference below (written_by_run, opened_by_run,
-- source_run_id, recorded_by_run) is a plain TEXT value with no constraint.
-- Entity ids reference entities in personal.db, a different file, so they are
-- plain TEXT too. The only foreign keys are inside this file: child rows of a
-- case or a config delta cascade when the parent row is deleted (the
-- entity-purge cleanup in items.id=888 deletes a case and relies on this).
-- No triggers. No DROP TABLE.
--
-- session_variables: one row per (context_key, focus_id, name). context_key
-- is the chat's key (items.id=886 plumbs it). version is the compare-and-set
-- counter for session writes: nothing serialises runs per chat, so two passes
-- in one chat can overlap. value is capped at 2048 bytes here and in
-- session_store.rs. The row named write_mode is the one-time latch
-- (decisions.id=939 section 4): one control row, no user content, scoped to
-- the chat, and the CHECK below lets it hold only the value suppressed at
-- version 1, so no UPDATE can move it to another value. Absence of the row
-- means writes are open. Everything else about the latch (set only once,
-- readers fail closed, every other write refused once it exists) is enforced
-- in session_store.rs because the rules need a transaction and no triggers
-- are allowed.
--
-- case_records: lifecycle state names are exactly those of decisions.id=508:
-- open, in-progress, escalated, awaiting-external, then the terminal states
-- resolved, replaced, closed. closed_at is set exactly when the state is
-- terminal (CHECK), which is how 508 keeps "closed" independent of
-- resolution_path. resolution_path says where the case went and is not the
-- state. current_rung is the escalation ladder rung (1 to 5, decisions.id=508).
-- subject_label is a denormalised display label of the primary subject and
-- can identify the device, so purge removes the case (decisions.id=939
-- section 7). case_records.version is the compare-and-set counter for case
-- updates. refresh_subject_label does not bump it.
--
-- case_entities: the link from a case to the entities it concerns. The
-- primary subject is the one row with role primary (a partial unique index
-- allows at most one per case) so there is a single source for it. A case
-- with no entity link is deleted by the purge in items.id=888.
--
-- case_strategies: the strategies_attempted log of decisions.id=508,
-- APPEND-ONLY. case_store.rs has no function that updates or deletes a row
-- and a test pins that. A row leaves only when its case is deleted (cascade).
-- UNIQUE (case_id, source_run_id, source_action_id, source_attempt) is the
-- idempotency key: a retried append of the same step attempt is a no-op.
-- strategy_id is only the append order.
--
-- config_deltas and config_delta_entities (decisions.id=508, 510): a delta is
-- an append-only description of a change. Its per-entity status carries the
-- stored status and status_changed_at, the clock that confirmation resets.
-- The 90-day degradation of decisions.id=510 is computed at READ time in
-- case_store.rs and is never written back, so no sweep is needed.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals and no comment text on a line that ends with a semicolon.
-- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS session_variables (
    context_key     TEXT NOT NULL CHECK (length(context_key) BETWEEN 1 AND 128),
    focus_id        TEXT NOT NULL CHECK (length(focus_id) BETWEEN 1 AND 128),
    name            TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    value           TEXT NOT NULL CHECK (length(CAST(value AS BLOB)) <= 2048),
    version         INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    written_by_run  TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (context_key, focus_id, name),
    CHECK (name <> 'write_mode' OR (value = 'suppressed' AND version = 1))
);

CREATE INDEX IF NOT EXISTS idx_session_variables_updated_at
    ON session_variables(updated_at);

CREATE TABLE IF NOT EXISTS case_records (
    case_id                  TEXT PRIMARY KEY
                                 CHECK (length(case_id) BETWEEN 1 AND 128),
    lifecycle_state          TEXT NOT NULL DEFAULT 'open'
                                 CHECK (lifecycle_state IN (
                                     'open','in-progress','escalated',
                                     'awaiting-external',
                                     'resolved','replaced','closed')),
    subject_label            TEXT NOT NULL DEFAULT '',
    symptoms                 TEXT NOT NULL DEFAULT '',
    current_rung             INTEGER NOT NULL DEFAULT 1
                                 CHECK (current_rung BETWEEN 1 AND 5),
    resolution_path          TEXT
                                 CHECK (resolution_path IS NULL OR resolution_path IN (
                                     'resolved','escalated-human',
                                     'warranty-rma','replaced')),
    outcome                  TEXT,
    durable_facts_extracted  TEXT NOT NULL DEFAULT '[]'
                                 CHECK (json_valid(durable_facts_extracted)),
    version                  INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    opened_by_run            TEXT NOT NULL,
    opened_at                TEXT NOT NULL,
    closed_at                TEXT,
    updated_at               TEXT NOT NULL,
    CHECK ((lifecycle_state IN ('resolved','replaced','closed')) = (closed_at IS NOT NULL))
);

CREATE INDEX IF NOT EXISTS idx_case_records_open
    ON case_records(opened_at)
    WHERE lifecycle_state IN ('open','in-progress','escalated','awaiting-external');

CREATE TABLE IF NOT EXISTS case_entities (
    case_id     TEXT NOT NULL REFERENCES case_records(case_id) ON DELETE CASCADE,
    entity_id   TEXT NOT NULL CHECK (length(entity_id) BETWEEN 1 AND 128),
    role        TEXT NOT NULL CHECK (role IN ('primary','affected')),
    linked_at   TEXT NOT NULL,
    PRIMARY KEY (case_id, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_case_entities_entity
    ON case_entities(entity_id);

CREATE UNIQUE INDEX IF NOT EXISTS uq_case_entities_one_primary
    ON case_entities(case_id)
    WHERE role = 'primary';

CREATE TABLE IF NOT EXISTS case_strategies (
    strategy_id        INTEGER PRIMARY KEY AUTOINCREMENT,
    case_id            TEXT NOT NULL REFERENCES case_records(case_id) ON DELETE CASCADE,
    source_run_id      TEXT NOT NULL,
    source_action_id   TEXT NOT NULL,
    source_attempt     INTEGER NOT NULL CHECK (source_attempt >= 0),
    strategy           TEXT NOT NULL,
    outcome            TEXT NOT NULL,
    user_signal        TEXT,
    recorded_at        TEXT NOT NULL,
    UNIQUE (case_id, source_run_id, source_action_id, source_attempt)
);

CREATE TABLE IF NOT EXISTS config_deltas (
    delta_id           TEXT PRIMARY KEY
                           CHECK (length(delta_id) BETWEEN 1 AND 128),
    session_reference  TEXT NOT NULL,
    description        TEXT NOT NULL,
    relevance_tags     TEXT NOT NULL DEFAULT '[]'
                           CHECK (json_valid(relevance_tags)),
    changed_at         TEXT NOT NULL,
    recorded_by_run    TEXT NOT NULL,
    created_at         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS config_delta_entities (
    delta_id           TEXT NOT NULL REFERENCES config_deltas(delta_id) ON DELETE CASCADE,
    entity_id          TEXT NOT NULL CHECK (length(entity_id) BETWEEN 1 AND 128),
    status             TEXT NOT NULL DEFAULT 'believed-active'
                           CHECK (status IN (
                               'believed-active','confirmed-active',
                               'believed-changed','unknown')),
    status_changed_at  TEXT NOT NULL,
    PRIMARY KEY (delta_id, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_config_delta_entities_entity
    ON config_delta_entities(entity_id);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (13, datetime('now'),
    'items.id=881: per-persona session_variables, case_records, case_entities, case_strategies, config_deltas, config_delta_entities (decisions.id=939)');
