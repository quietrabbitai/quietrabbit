-- shared_045.sql
--
-- items.id=896 stage 1 (decisions.id=942 point 9 and ruling Q12, addendum 3 b
-- and c): model_capabilities, what each model has been MEASURED to do, per task
-- type and document profile. The resolver of stage 2 reads it. Number checked
-- against the live schema directory on the build date: shared_044 was the
-- latest. Numbers before the items.id=633 cutover are provisional, since the
-- cutover folds every prefix into one baseline (items.id=633 must absorb this
-- file and regenerate tests/golden/schema_shape.txt).
--
-- One row is one measured cell: (subject_kind, provider_id, model_id, task_type,
-- profile).
--   subject_kind  local (model_id is an Ollama tag of a curated providers row,
--                 provider_id is empty) or hosted (provider_id and model_id name
--                 a curated provider_models row).
--   task_type     a ModelTaskType (conductor/model_block/config.rs).
--   profile       the id of a SHIPPED document type, or empty for a task-type-wide
--                 cell. Every writing document type has the one task type
--                 creative_writing and calibration found the strength of a model
--                 varies by kind of document, so the profile is part of the key.
--   attained_bar  the highest named bar reached: below_basic (measured and not
--                 adequate), basic, standard or high. A cell with NO row is
--                 UNMEASURED, which is different from below_basic: the resolver
--                 never chooses an unmeasured cell automatically.
--   model_class   small, medium or large, a property of the model repeated on each
--                 of its rows. A ranking hint for the resolver, never a filter.
--   provenance    jason_read, harness_local or harness_hosted.
-- Only the two columns with a CHECK are closed in SQL. The rest are closed in
-- Rust (the catalog apply validates them, the reader skips a value it does not
-- know), so a later vocabulary needs no table rebuild.
--
-- WHO WRITES IT. Only the signed catalog (provider_catalog::apply, its
-- capabilities section) and the seed below, which is the baseline catalog's own
-- capabilities section (src-tauri/catalog/provider-catalog.json): a test applies
-- the baseline to a migrated database and requires a no-op. Nothing at run time
-- writes it, and no foreign key points at it or from it, so a row for a model the
-- release no longer curates is simply ignored.
--
-- SEED. Jason's read of the variant calibration run (decisions.id=942 addendum 3
-- b, 2026-11-13: 4 samples of each of three local models on each of five
-- synthetic briefs). Bars are PROVISIONAL and are re-read after the voice
-- parameters are tuned and when the full harness (items.id=899) has run. Cells
-- Jason did not rate are left out on purpose: qwen2.5:7b on the RMA letter and the
-- complaint email (unrated), and creative_story (not run). The note holds what
-- the bar cannot, such as which model Jason preferred. MARGINAL in a note means
-- the bar was reached only just (the cover letter was poor but acceptable, the
-- essay section uneven between samples).
--
-- ALTER precedent: none (a new table). CREATE TABLE IF NOT EXISTS and INSERT OR
-- IGNORE keep the file safe to run again on a database that already has it.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

CREATE TABLE IF NOT EXISTS model_capabilities (
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('local', 'hosted')),
    provider_id TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL,
    task_type TEXT NOT NULL,
    profile TEXT NOT NULL DEFAULT '',
    attained_bar TEXT NOT NULL
        CHECK (attained_bar IN ('below_basic', 'basic', 'standard', 'high')),
    model_class TEXT NOT NULL,
    sample_count INTEGER NOT NULL CHECK (sample_count >= 0),
    measured_at TEXT NOT NULL,
    provenance TEXT NOT NULL,
    note TEXT,
    PRIMARY KEY (subject_kind, provider_id, model_id, task_type, profile),
    CHECK ((subject_kind = 'local' AND provider_id = '')
        OR (subject_kind = 'hosted' AND provider_id <> ''))
);

INSERT OR IGNORE INTO model_capabilities
    (subject_kind, provider_id, model_id, task_type, profile, attained_bar,
     model_class, sample_count, measured_at, provenance, note)
VALUES
    ('local', '', 'llama3.2:3b', 'creative_writing', 'rma_letter', 'basic',
     'small', 4, '2026-11-13', 'jason_read',
     'Acceptable. Slightly preferred over the 8B for this type.'),
    ('local', '', 'llama3.1:8b', 'creative_writing', 'rma_letter', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'Acceptable.'),
    ('local', '', 'llama3.2:3b', 'creative_writing', 'complaint_email', 'basic',
     'small', 4, '2026-11-13', 'jason_read',
     'Acceptable.'),
    ('local', '', 'llama3.1:8b', 'creative_writing', 'complaint_email', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'Acceptable. Preferred over the 3B for this type.'),
    ('local', '', 'llama3.2:3b', 'creative_writing', 'personal_note', 'basic',
     'small', 4, '2026-11-13', 'jason_read',
     'Acceptable.'),
    ('local', '', 'llama3.1:8b', 'creative_writing', 'personal_note', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'Acceptable.'),
    ('local', '', 'qwen2.5:7b', 'creative_writing', 'personal_note', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'Acceptable.'),
    ('local', '', 'llama3.2:3b', 'creative_writing', 'cover_letter', 'basic',
     'small', 4, '2026-11-13', 'jason_read',
     'MARGINAL: poor but acceptable. Expected to change once the voice parameters are tuned.'),
    ('local', '', 'llama3.1:8b', 'creative_writing', 'cover_letter', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'MARGINAL: poor but acceptable. Expected to change once the voice parameters are tuned.'),
    ('local', '', 'llama3.2:3b', 'creative_writing', 'essay_section', 'basic',
     'small', 4, '2026-11-13', 'jason_read',
     'MARGINAL: uneven between samples and longer drafts were better. Order of preference was qwen2.5:7b, then the 3B, then the 8B.'),
    ('local', '', 'llama3.1:8b', 'creative_writing', 'essay_section', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'MARGINAL: uneven between samples and longer drafts were better. Order of preference was qwen2.5:7b, then the 3B, then the 8B.'),
    ('local', '', 'qwen2.5:7b', 'creative_writing', 'essay_section', 'basic',
     'medium', 4, '2026-11-13', 'jason_read',
     'MARGINAL: uneven between samples and longer drafts were better. Order of preference was qwen2.5:7b, then the 3B, then the 8B.');

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (45, datetime('now'),
    'items.id=896 (decisions.id=942): model_capabilities, measured capability per model, task type and document profile, seeded from the first calibration read');
