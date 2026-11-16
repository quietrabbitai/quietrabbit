-- shared_047.sql
--
-- items.id=897 stage 1 (decisions.id=942 point 8 and addendum 3 g, decisions.id=948):
-- per-Focus launch questions, their options, mode profiles, and one short display
-- line. Number checked against the live schema directory on the build date:
-- shared_046 was the latest (043 blocks, 044 consent, 045 capabilities, 046
-- hand-off row). Numbers before the items.id=633 cutover are provisional, since the
-- cutover folds every prefix into one baseline (items.id=633 must absorb this file
-- and regenerate tests/golden/schema_shape.txt).
--
-- SHIPPED DATA ONLY. shared.db is unencrypted (items.id=661), so nothing here is
-- ever a user's answer. Answers live in the persona's encrypted outputs.db as
-- session variables, and a user's saved adjustments are items.id=905's. These
-- tables are identical for every install and carry no user content.
--
-- NO FOREIGN KEYS. Five of the six built Focuses have no `focuses` row (their
-- display data lives in the .focus file), so a Focus here is just a focus_id string,
-- as in model_capabilities. A focus_id with no definition is ignored.
--
-- A Focus's display NAME and DESCRIPTION are deliberately NOT here. They stay in
-- their one existing home (the `focuses` row, else the loaded .focus definition)
-- and conductor::focus_display reads them. Only ui_one_liner is a new fact: the
-- longest of the six descriptions is 210 characters, too long for a one-line row.
--
-- focus_launch_questions   one row per question a Focus asks before a run.
--   slot            the compiled bound name that carries the answer: document_type,
--                   launch_mode or launch_answer_1 to launch_answer_3. One question
--                   per slot, so a Focus has at most five questions.
--   kind            mode (its options are the Focus's modes, at most one such
--                   question, and it must use the launch_mode slot) or choice.
--   asked           1 = shown to the user. 0 = a preset that fixes the value (the
--                   default is used and nothing is submitted).
--   default_option  the option used when nothing was answered. Rust checks that it
--                   is one of the question's options at LOAD and in the admission
--                   test.
--   prompt          placeholder wording, owned by Chat-BRAND (items.id=506).
-- focus_launch_options     the closed options of a question. label and hint are
--                   placeholder wording, owned by Chat-BRAND.
-- focus_mode_profiles      what a mode narrows. envelope is JSON parsed as
--                   EnvelopeOverride (deny_unknown_fields: quality_floor,
--                   highest_tier, destinations, declared_inputs) and applied only
--                   through tighten(), so a mode can only remove.
-- focus_launch_display     ui_one_liner, at most 80 characters, placeholder wording.
--
-- Only the CHECKs below are closed in SQL. Everything else is validated in Rust
-- (conductor::launch_config::validate), the same function LOAD and the admission
-- test use. Rows for a real Focus ship with that Focus's rebuild (items.id=903 for
-- Writing Assistant). None ship here besides the display lines.
--
-- ALTER precedent: none (new tables). CREATE TABLE IF NOT EXISTS and INSERT OR
-- IGNORE keep the file safe to run again on a database that already has it.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS focus_launch_questions (
    focus_id TEXT NOT NULL,
    question_id TEXT NOT NULL,
    slot TEXT NOT NULL CHECK (slot IN (
        'document_type', 'launch_mode',
        'launch_answer_1', 'launch_answer_2', 'launch_answer_3'
    )),
    kind TEXT NOT NULL CHECK (kind IN ('mode', 'choice')),
    sort_order INTEGER NOT NULL,
    asked INTEGER NOT NULL DEFAULT 1 CHECK (asked IN (0, 1)),
    default_option TEXT NOT NULL,
    prompt TEXT NOT NULL,
    PRIMARY KEY (focus_id, question_id),
    UNIQUE (focus_id, slot),
    CHECK (kind <> 'mode' OR slot = 'launch_mode')
);

CREATE TABLE IF NOT EXISTS focus_launch_options (
    focus_id TEXT NOT NULL,
    question_id TEXT NOT NULL,
    option_id TEXT NOT NULL,
    sort_order INTEGER NOT NULL,
    label TEXT NOT NULL,
    hint TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (focus_id, question_id, option_id)
);

CREATE TABLE IF NOT EXISTS focus_mode_profiles (
    focus_id TEXT NOT NULL,
    option_id TEXT NOT NULL,
    envelope TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (focus_id, option_id)
);

CREATE TABLE IF NOT EXISTS focus_launch_display (
    focus_id TEXT PRIMARY KEY,
    ui_one_liner TEXT NOT NULL CHECK (length(ui_one_liner) BETWEEN 1 AND 80)
);

-- PLACEHOLDER WORDING (Chat-BRAND owns the final text, items.id=506). Three Focuses
-- are launchable from the generic screen at R1: Writing Assistant, Research and Buy
-- and Role Assessment.
INSERT OR IGNORE INTO focus_launch_display (focus_id, ui_one_liner)
VALUES
    ('writing-assistant', 'Write and polish letters, emails and notes in your own voice.'),
    ('research-and-buy', 'Research a purchase and get a clear recommendation.'),
    ('role-assessment', 'Check a job posting against your background and decide what to do.');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (47, datetime('now'),
    'items.id=897: focus_launch_questions, focus_launch_options, focus_mode_profiles and focus_launch_display -- shipped per-Focus launch questions, mode profiles and a one-line display fact, with the three display lines');
