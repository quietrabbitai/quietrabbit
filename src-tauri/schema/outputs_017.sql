-- outputs_017.sql
--
-- items.id=901 (decisions.id=942 architecture point 4 and rulings Q2 and Q3, plan label
-- B4): the RUN PLAN's two small tables in the persona's outputs.db. Schema only. The typed
-- stores are persistence/plan_approval_store.rs and persistence/run_park_store.rs. The
-- plan itself (conductor::run_plan) is computed, never stored: only the user's approval of
-- it and the reason a run parked are kept.
--
-- WHY outputs.db AND NOT shared.db. Both tables record decisions and events of one user's
-- chats (what they approved to be spent or sent, why a run stopped). That is the user's
-- own behaviour, so it stays in the persona's ENCRYPTED file (decisions.id=862, as
-- pass_artifacts and step_reviews do), never in the unencrypted shared.db. The file is the
-- persona, so no row carries a persona_id.
--
-- plan_approvals. ONE ROW PER (context_key, focus_id): the approval a user gave to a
-- Focus's run plan in one chat, so a document session asks once. A row holds the DIGEST of
-- the approved plan (a SHA-256 of what the plan could do, see conductor::run_plan), the
-- per-run COST CEILING the user chose in micro-USD, and choices_json, the per-step choices
-- (action id, strategy, provider id and model id of a pinned model) and nothing else. It
-- holds no prompt, no draft, no persona fact and no model output. A write replaces the row
-- and moves version up by one: the latest approval wins. The row is written ONLY inside one
-- BEGIN IMMEDIATE transaction that first checks the one-time write latch (decisions.id=939
-- section 4), so a latched chat writes nothing and a latch set between the check and the
-- write cannot be raced. A latched chat, a chat whose latch cannot be read and a run with no
-- chat keep their approval in process memory only (conductor::plan_memory), never here.
-- Approvals written before a latch was set stay and still read. Removing an approval (a
-- revocation) is allowed in a latched chat because it only withdraws a permission.
--
-- run_parks. ONE ROW PER RUN: the latest reason the run parked, as a registered code
-- (spend::codes::Code) plus its structured parameters (opaque ids, counts, reason codes),
-- so a screen can read it, because RunStatusPayload carries none. It is written exactly when
-- the run's own focus_runs row is written, so it makes no write a latched chat does not
-- already make. No text of any kind.
--
-- NO FOREIGN KEYS (items.id=633 rebuilds focus_runs and must not have to carry these), no
-- triggers, no DROP TABLE. Retention: plan_approvals are keyed by context_key like
-- session_variables, so the chat-level clear and the retention sweep (both items.id=888)
-- must delete them by context_key and by updated_at, and run_parks by created_at. Neither
-- exists yet and this item adds neither.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals
-- and no comment text on a line that ends with a semicolon.
-- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS plan_approvals (
    context_key     TEXT NOT NULL CHECK (length(context_key) BETWEEN 1 AND 128),
    focus_id        TEXT NOT NULL CHECK (length(focus_id) BETWEEN 1 AND 128),
    digest          TEXT NOT NULL CHECK (length(digest) = 64),
    ceiling_micros  INTEGER NOT NULL CHECK (ceiling_micros >= 0),
    choices_json    TEXT NOT NULL DEFAULT '[]' CHECK (length(CAST(choices_json AS BLOB)) <= 8192),
    version         INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (context_key, focus_id)
);

CREATE INDEX IF NOT EXISTS idx_plan_approvals_updated_at
    ON plan_approvals(updated_at);

CREATE TABLE IF NOT EXISTS run_parks (
    run_id          TEXT NOT NULL PRIMARY KEY CHECK (length(run_id) BETWEEN 1 AND 128),
    action_id       TEXT NOT NULL CHECK (length(action_id) BETWEEN 1 AND 256),
    code            TEXT NOT NULL CHECK (length(code) BETWEEN 1 AND 64),
    params_json     TEXT NOT NULL DEFAULT '{}' CHECK (length(CAST(params_json AS BLOB)) <= 4096),
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_run_parks_created_at
    ON run_parks(created_at);

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (17, datetime('now'),
    'items.id=901: run plan approvals plan_approvals and park reasons run_parks (decisions.id=942 point 4, Q2, Q3)');
