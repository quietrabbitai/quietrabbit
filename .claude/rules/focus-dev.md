---
paths:
  - "src-tauri/src/conductor/focus_dev/**"
  - "src-tauri/examples/focus_dev.rs"
  - "src-tauri/focus_specs/**"
  - "src-tauri/block_data/focus_presets.json"
  - "src-tauri/src/persistence/focus_composition_store.rs"
  - "src-tauri/src/persistence/migrations.rs"
---

## Focus development tool (items.id=898, decisions.id=951)
Seeds, validates and re-seeds a Focus in a DEVELOPMENT database, emits a migration and expands a
preset. Never part of the app: a lib module with a thin example, debug and test builds only.
- LAYOUT. conductor/focus_dev/: spec.rs (the Focus spec, serde deny_unknown_fields), preset.rs,
  shape.rs, render.rs (SQL text) and drift.rs are PURE (no sqlx, no file, no network; a scan pins it)
  and always compiled. devroot.rs and tool.rs (and the Focus writers write_focus, write_launch and
  delete_focus in persistence/focus_composition_store.rs, and migrate_shared_db_at and
  shared_schema_versions in migrations.rs) are compiled ONLY under cfg(any(test, debug_assertions)):
  `test` is there because `cargo test --release --lib -- release_` compiles the test modules. A
  release build of the app contains no code that opens, writes or deletes a Focus row and no marker
  writer (a test pins the attributes, the release build compiles; symbols were not inspected).
  examples/focus_dev.rs only parses arguments and prints (a stub in release). Nothing outside
  focus_dev, its tests and the example names it (scan test); tauri.conf.json is pinned unchanged.
- THE SPEC is the source of truth, a checked-in JSON file (src-tauri/focus_specs/<focus_id>.json,
  none exists yet: the first arrives with items.id=903 or 915). It states the focuses row, ordered
  rows (an ordinary step, or a block id plus its configuration), typed guards (decisions.id=938: the
  columns, no expression), store-writer columns, a schedule trigger, launch questions and a
  one-liner. The DB is derived from it. ONE WRITER: write_focus is used by the admission tests, by
  validate and reseed; emit-migration renders the same row structs as SQL, and a test applies that
  text to a scratch database and compares every seeded table with the writer's.
- COMMANDS (cargo run --example focus_dev --, from src-tauri/): validate <spec> (scratch database, the
  REAL loader: it is the guard-path, placement, declared-inputs and launch lint), reseed <spec>
  --data-root <dir> [--apply] (dry run unless --apply), reseed --mark-dev-root <dir>, emit-migration
  <spec> [--number N] [--baseline] [--out file] and expand-preset <id> --as <prefix> --param k=v.
  emit-migration prints the SchemaFile line to add to migrations.rs by hand and never edits it.
- DEV-ROOT GUARD. reseed writes only <dir>/instance/shared.db and only of a directory that is not,
  does not contain and is not inside the real app data dirs ($XDG_DATA_HOME and ~/.local/share, then
  ai.quietrabbit.app or quietrabbit), that holds the marker .qr_dev_root written by
  --mark-dev-root (it names the canonical path, so a copied or moved directory loses it), and that
  has instance/shared.db or is empty. instance/ and shared.db must not be symlinks and must resolve
  inside the root: DevRoot::shared_db_path re-verifies on every use, immediately before the database
  is opened or migrated. Linux only (items.id=700, 701). If XDG_DATA_HOME in the shell points at the
  dev root the tool refuses it: unset it. The scratch directories it makes are mode 0700. Encrypted
  databases and users/ are never opened.
- RESEED. validate on a scratch database first, then the loader runs on a VACUUM INTO copy of the dev
  database (the loader takes a pool), then ONE transaction on the dev database (delete the Focus,
  write it, read it back, commit; any failure rolls back). A dry run never migrates.
- EMIT. A data-only shared_NNN.sql: DELETE then plain INSERT (never INSERT OR IGNORE, which cannot
  repair a stale row), header line `-- focus_dev: <focus_id> spec-sha256 <hex>`, then the
  schema_version row. --baseline omits that row: a fragment for the one-baseline file (items.id=633,
  decisions.id=857). A text value with a semicolon is REFUSED; multi-line values whose lines
  parse_statements would drop (blank, "--", "create trigger", "end") are written with char(10) joins.
  The text is parsed back and must equal what was rendered. The next number comes from SCHEMA_FILES
  and the schema directory, which must agree. items.id=633 folds the emitted files in.
- DRIFT. A test requires every focus_specs/*.json to have an emitted file (a schema/shared_*.sql or
  focus_specs/<id>.baseline.sql) whose header hash is the spec's current SHA-256. Vacuous until the
  first spec.
- PRESETS (decisions.id=942 Q7) are shipped parameterised fragments of rows in
  block_data/focus_presets.json (EMPTY until items.id=918), typed params (slug, enum, int), %name%
  slots, brace-free values, locals renamed with the use's prefix, expanded at seed time into ordinary
  rows. The runtime never sees one. A preset admission test expands every preset for every param
  combination (vacuous today, a fixture proves it bites).
- LOADER RULE (the rider). build_action_definition fails at LOAD on a present unparsable step_type
  and on a present task_type that is not one of ModelTaskType::ALL or "general" (exact match); an
  absent value keeps its default. A test pins the list equal to providers/evaluation.rs's tasks.
  RawStep and RawStepFromDb still have no deny_unknown_fields: shape.rs reports an unknown key in an
  ordinary step (ORDINARY_STEP_KEYS, pinned to the struct), and the check over every shipped Focus
  (the shared.db seeds and the six .focus files) found 0 unknown keys in 16 steps.
- NOT BUILT: a strict loader, preset launch rows, the first real spec and preset, Windows and macOS.
