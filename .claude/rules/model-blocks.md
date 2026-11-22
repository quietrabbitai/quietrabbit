---
paths:
  - "src-tauri/src/conductor/model_block/**"
  - "src-tauri/src/conductor/block_dispatch.rs"
  - "src-tauri/src/conductor/draft_check.rs"
  - "src-tauri/block_data/document_types.json"
  - "src-tauri/tests/fixtures/building_blocks_catalog.json"
  - "src-tauri/examples/calibration_harness.rs"
  - "src-tauri/examples/support/**"
  - "src-tauri/examples/harness/**"
  - "src-tauri/examples/calibration_data/**"
---

## Model blocks (items.id=894, decisions.id=941/942)
Four block kinds share one registry (conductor/block_dispatch.rs,
registered_block()): Pure (a BlockHandler over the TaskTrack, cb-06), Model
(conductor/model_block/: cb-12 draft document, cb-13 voice apply or refine,
cb-14 revise), Render (cb-15, the manual hand-off starter: see "Manual hand-off"
below) and Seed (declared, none registered yet). A model block has no
handler: at LOAD a composition row citing it is parsed (typed, deny_unknown_fields,
configuration `{common, spec}`) and EXPANDED into an ordinary ActionDefinition with
block_stable_id None and a `requirement` (task need, range clamped to the Focus
ceiling, optional model-class preference, never a model id). Gate 1, the spend gate,
Gate 2, Gate 3 and the declared-inputs lint all apply on that one path. The executor
resolves `requirement` through the model resolver (see "Model resolver" below); a step
with no requirement still goes through select_model.
- EXTERNAL_MODEL_BLOCKS_ALLOWED = false (model_block/mod.rs): LOAD refuses any
  destination but local and any tier above local_only. A block allowing hosted_api
  names neither {user_input} nor {persona_context}. B10 flips the constant (the outbound
  review of items.id=884 has landed).
- tighten() (model_block/config.rs) is the one function that narrows an envelope
  (quality floor, highest tier, destinations, declared inputs); only those four are
  tighten-only. Latitude and the voice settings are the user's to move both ways
  (document_type::resolve_settings). flag_unsupplied_references is tighten-only
  against based_on (decisions.id=548). A run re-expands every model block when it starts, with
  the user's launch answers and the three setting layers, and applies a mode profile through
  tighten() (see "Launch questions and run-start settings").
- Every sentence a block emits is DATA in src-tauri/block_data/document_types.json
  (phrases, scales, shipped document types), validated by the same function a custom
  type will use, with no {token} allowed. Rust holds only the template skeleton.
  Tuning wording is a data edit.
- Adding a block: register it, add its mirror row (a numbered data-only shared_*.sql),
  and add its row to tests/fixtures/building_blocks_catalog.json. CODE re-exports that
  file (items.id=895 moved it from Chat-PM): once Chat-PM has written the building_blocks
  row in qr_docs.db, read it READ-ONLY with db_query.py (03_ProjectDocs/scripts/db_utilities,
  SELECT json_object(...) gives exact text), copy the columns unchanged, set the
  informational `kind` (pure, model, render or seed: the database has no such column and the
  admission test checks it against the registry), and update `exported` and the row's
  `source`. Never edit qr_docs.db or the vault from here. conductor/model_block/
  admission_tests.rs checks the rest, and PINNED_KEYS there fails on any new
  configuration key until it is reviewed.
- focus_runs.user_input holds the blended chat window so a parked run can resume (and
  focus_runs.external_input, items.id=883, its user-turns-only copy: see "External steps and
  chat history"; both are written together and cleared together). It
  is NULLed when the status becomes complete, cancelled, failed or awaiting_extract_confirm
  (output_store::USER_INPUT_CLEARED_STATUSES; the last since items.id=888, F8) and is not
  written for a latched one-time chat. A run
  parked and never finished keeps it until the retention sweep expires it (items.id=681,
  see "Parked-run retention and resume staleness": it clears both input columns) or the
  chat is archived (items.id=888's archive hook clears it on every run of the chat).
- Resuming a requires_user_handoff step re-parks it (pinned by a characterization
  test in lifecycle.rs). items.id=387 does NOT change this: the manual hand-off runs on
  chat passes and parks nothing (see "Manual hand-off"). rehydrate_focus_run restores
  context_key (items.id=888, focus_runs.context_key) but never the turn text, so a resumed
  run's store writers run, except handoff_ingest, which fails closed with no turn.
- The unsupported-additions measures live in ONE shipped module, conductor/draft_check.rs
  (items.id=899, moved unchanged from examples/focus_calibration.rs; do not add a second
  copy). It is pure (no I/O, no model or provider id, scans pin both): check(text, &CheckInput)
  returns a CheckReport (placeholders, opening and closing chatter, unsupported specifics,
  coverage) and decide(report, latitude, attempt) returns Accept, Regenerate, Mark or Skipped
  through CheckPolicy::for_latitude (strict: regenerate once then mark, framed: mark only,
  free: off). Coverage is keyed and used by the harness and tests only; production passes no
  points. items.id=903 consumes it through cb-06's draft_check section (below), never from the
  executor. CITATIONS (decisions.id=963 ruling 2, option A): CheckInput.citations is
  DocumentType.citation_markers (none, numeric; numeric needs flag_unsupplied_references and is
  tighten-only against based_on; essay_section only). Numeric markers ([1], [1, 3], [2-4], [2a]) are
  cut out before the placeholder and unsupported-specific scans, and a marker the allowed text does
  not carry itself is listed in unsupplied_references, which MARKS and never regenerates. The
  harness and focus_calibration pass none, so 893/894 measurements stay comparable.
  The check cannot catch soft embellishment, an invented cause, a reversed fact that keeps the
  keywords, tone or readability.

- BLOCK GUARD (items.id=903, decisions.id=963 ruling 1): a model block emits a `present` clause for
  every required input that is an Optional bound name (cb-12 brief_var, cb-13 apply input_var, cb-14
  draft_var and change_var; model_block::required_input_clauses). The DB loader MERGES it ahead of
  the row's own clauses (merge_block_clauses: a restated clause is not repeated, the merged set is
  capped by MAX_GUARD_CLAUSES); the run-start re-expansion keeps the merged list and refuses a row
  that lost the block clause. A pure block (cb-06) emits none: a composition guards its check rows
  on the brief itself.
- CB-06 DRAFT CHECK (decisions.id=942 point 3, ruling 3; conductor/check_block.rs, pure): an
  optional draft_check section (document_type, emit verdict|report, attempt 0|1, sources with
  coverage weights, section_var, voice_check). verdict records exactly accept, regenerate, mark or
  skipped (for guards); report records categories and counts only, never draft text. A missing
  source marks and never accepts or regenerates. Voice data (suppress phrases, sentence bands per
  pacing) is block_data CheckData. The run start fills draft_check.run (document type, latitude,
  pacing) from the same resolution the blocks re-expand with; an authored run is refused at LOAD.
  RULING 3 amends the pure-block contract: BlockHandler::declared_inputs names the stored values a
  row reads (its declared_inputs: linted real for block rows too, seeded at pass start) and run()
  takes BlockInputs (the track and ONLY those seeds, lifecycle::declared_seeds). No provider, store
  handle or write.
- PC-03 (ruling 4, model_block/elicit.rs): elicit, one round (rounds: single, a closed enum
  items.id=906 extends). It turns the request into one StructuredBrief JSON object (phrases.elicit,
  no brace); it reads {user_input}, so the hosted rule keeps it local (READS_THE_REQUEST in the
  admission tests). Its catalog row in the fixture is PROPOSED by Code until Chat-PM writes the three
  empty qr_docs.db columns.

## Calibration harness (items.id=899, decisions.id=950)
src-tauri/examples/calibration_harness.rs (with examples/support/, examples/harness/ and
examples/calibration_data/: synthetic briefs, voice cases, rubrics and bars.json, never
shipped) generates drafts through the SHIPPED cb-12 and cb-13 blocks on QR's own sidecar,
scores them with draft_check, has the OTHER local models cross-judge (temperature 0, a closed
JSON verdict, unparseable = no verdict), and writes read.html: a static page of at most 100
drafts (at least 3 reads per cell) that Jason reads and exports as ratings.json.
Flags: --approve-long-run (required for a real run, about 2 hours), --resume, --summarize,
--judge, --read-page, --apply-ratings <run> <file>, --emit-cells <run>, --dry-run.
- The judge only CHOOSES which drafts are read. A cell's bar comes only from Jason's reads
  (provenance jason_read, best bar that at least 66% of the reads reach, fewer than 3 reads =
  no cell); no harness_local cell is written, and the judge's agreement with Jason is reported
  (agreement.md), not trusted. Network is local Ollama only (local_url() accepts only http to
  127.0.0.1, localhost or ::1 with no credentials).
- Run 20261116-213009 (300 drafts, 600 judge calls) produced the 21 baseline cells. They
  REPLACE the first 12 seed rows in place in shared_045.sql and catalog/provider-catalog.json
  (decisions.id=857: no new schema file; the row keys collide, so INSERT OR IGNORE alone would
  have dropped them). A development database already at schema 45 does NOT get them (the seed
  only runs at migration): reseed it or apply a catalog.
- EXAMPLES ARE NOT LINTED IN CI (the CI clippy runs `--tests` only); they ARE compiled there
  (`cargo test --no-run` in .github/scripts/ci-test-shards.sh builds every target plain `cargo test`
  does, examples included, items.id=932 checked the script). Locally, from src-tauri/: `cargo clippy --tests --examples -- -D warnings` and
  `cargo test --examples`.
