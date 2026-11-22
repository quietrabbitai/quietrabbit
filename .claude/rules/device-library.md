---
paths:
  - "src-tauri/src/conductor/device_*.rs"
  - "src-tauri/src/conductor/entity_writer*.rs"
  - "src-tauri/src/conductor/tech_support_draft.rs"
  - "src-tauri/src/persistence/owned_entity_store.rs"
  - "src-tauri/src/persistence/entity_purge.rs"
  - "src-tauri/src/persistence/entity_store.rs"
  - "src-tauri/src/persistence/handles.rs"
  - "src-tauri/src/commands/device_library.rs"
  - "src-tauri/block_data/device_fields.json"
  - "src-tauri/schema/personal_011.sql"
  - "frontend/src/chat/*evice*"
  - "frontend/src/chat/DeleteEverythingControl.tsx"
---

## Focus-owned entities and the device library (items.id=891, decisions.id=507, 509, 940, 944)
Tech Support keeps DIRECT-SUBJECT devices only (relationships, entity_relationships and the
decisions.id=507/512/516 traversal stay deferred). Nothing here is enabled until items.id=915 seeds the
Focus and items.id=916 flips it; the hard purge ships before that.
- Ownership (personal_011.sql): entities.owner_focus (NULL = unowned, every pre-existing entity).
  entity_store::OwnerFilter (Any, Unowned, Focus) is a field of EntityFilter; a source scan
  (entity_store tests) pins that every non-test list_entities/search_entities caller outside the file
  names `owner:` and never uses the Any-by-default constructors. Persona share, sync, view-only sync,
  load_entity_facts_for_context (every Focus's PersonalTrack and the PF entity-fact resolution) and
  list_pending_cross_persona_exports exclude an owned entity and its facts. create_owned_entity_conn is
  the only writer of owner_focus. Entity serializes WITHOUT owner_focus (payload hashes unchanged).
- Facts (conductor/device_fields.rs, block_data/device_fields.json): a CLOSED catalogue, 7 categories
  and 20 fields, each with label, sensitivity, a disclosure (never, ask or pass) and for ask a
  suggestion (items.id=924, replacing the tier-2/3 abstraction policies), byte cap, and the
  optional access_credential_flag (no shipped field has it since decisions.id=956/items.id=936: the serial is
  an ordinary ask field; validate() still refuses a flagged field as pass). validate() refuses a field NAME containing a credential
  word (password, pin, token, secret, login...). known_issues (400 bytes) and hardware_specs (300) are
  free text in practice: nothing in code stops a typed secret, so the exclusion that covers them is
  prompt-level (the items.id=915 seed and its templates; decisions.id=507 credentials invariant) and
  no secret-shape detector exists (Jason's ruling). Subject class uses COLON separators. Facts are
  written through create_entity_fact_with_policy_conn (writes the abstraction policies; the older
  writer defaulted both to pass).
- Store layer: persistence/owned_entity_store.rs (find by replay key or exact normalized name or
  alias, create_subject, idempotent superseding put_owned_fact, rename_owned, list_active_facts).
  Everything is scoped by the owning Focus: another Focus's, unowned, archived and purge-marked
  entities are all NotFound. It has no delete.
- Writer ops (conductor/entity_writer.rs, store_op.rs): entity_find_or_create (replay key
  "{run}:{action}", then an E- handle if the Handle slot is handle-SHAPED (a handle-shaped value that
  resolves to nothing is an error; text like "none" is ignored), then the exact name, else create;
  PUBLISHES the entity id under a required store_target) and entity_fact_put (closed catalogue;
  an unknown field fails the pass). Both are in requires_collection_consent. They read the one-time
  latch (outputs.db) first and delete nothing; a latch set between the read and the write is an
  accepted window (no compensation). LOAD (store_op::check_composition(steps, row_vars, posture)):
  entity ops are refused in a per_field Focus; the entity slot of case_open, case_link,
  delta_changed, entity_fact_put and the entities slot of delta_record must name an id an EARLIER
  entity_find_or_create row published (a model never names an id; this tightened the 887/889
  contract); a Handle slot is model output only. At run time run_store_op takes Entities (Verified
  = the persona's personal.db, NotNeeded, Unchecked = tests only, a scan pins that production never
  names it) and verifies every entity id (owned by the running Focus, active) before the write.
  delta_record accepts ONE published id as a list of one. WriterContext carries persona_id.
- E- handles (persistence/handles.rs Kind::Entity): hash of (kind, focus, id), resolved among the
  Focus's own ACTIVE owned entities; the model and the UI see handles, never ids.
- Derived names (conductor/device_seeding.rs; bound_spec Derived, Always, text; read by seed_pass,
  which opens personal.db only for a pass that names one; read_seeds is outputs-only and fails closed
  if handed a device name): subject_context (the ACTIVE case's primary entity, only if owned by the
  running Focus and active, else ""; "Device: name (class)" then one "Label: value" line per
  catalogued fact in catalogue order, 200 bytes a value, 1800 total, whole lines dropped) and
  device_library_brief ("- E-XXXXXXXX: name (class) | Label: value ..." up to 8 devices newest first,
  up to 3 headline facts whose disclosure is pass, 60 bytes a value, "- and N more").
  subject_context's LOCAL text renders EVERY catalogued fact, serial number and address included. A
  hosted step receives the EXTERNAL rendering instead (items.id=924, "Device facts in external steps and
  allowed answers"). THE OLD CLOSED LIST WAS TEMPORARY FOR EXTERNAL MODEL STEPS AND IS LIFTED: items.id=884
  refused both names in every external step (DEVICE_NAMES_NEVER_EXTERNAL, DeviceNameInExternalStep) until
  a per-field policy and a tier-aware render existed; items.id=924 built both, so an external model step
  may name and declare them like any store-backed name. A NARROWER LIST STAYS FOR cb-15 STARTERS
  (DEVICE_NAMES_NEVER_IN_STARTER, DeviceNameInStarter: a starter, a variable derived from a device name,
  or {previous_output} after one) until its own design. They stay tainted like every store
  name. Seeds are read once per pass, so a device's facts appear one pass
  late. The wording is provisional (Chat-PM reviewed). items.id=914 set the other three texts
  (seeding.rs: BRIEF_CASES 3, DELTA_LINES 5, STRATEGY_LINES 5, body caps 600/2400/1200 bytes, per-field clips,
  a trailing "and N more" or "and N earlier not shown" line; the numbers are unmeasured, items.id=916 measures them).
- HARD PURGE (persistence/entity_purge.rs, the ONLY module that deletes the entity, case and delta
  tables; a scan pins it, and the case_store/session_store no-delete scans still hold). Order:
  1 MARK (entities.status = user_deleted, the commit point; the entity is invisible to every active-
  filtered reader), 2 outputs.db in one BEGIN IMMEDIATE (active_case_id pointers to deleted cases;
  cases whose PRIMARY subject it is plus cases left with no link, cascading links and strategies;
  its remaining case_entities; its config_delta_entities; deltas left with no link; reads are
  unscoped by Focus), 3 personal.db in one BEGIN IMMEDIATE (all entity_facts versions,
  entity_relationships, dedup_candidates, the row; only an owned entity marked user_deleted is
  deleted). reconcile (no journal) finishes MARKED entities and sweeps ORPHAN ids (an id in
  case_entities/config_delta_entities with no entities row at all); it runs at login for every
  persona (supervised, Sweep lease, never awaited, creates no files), at the start of list_devices
  and in purge_all_devices. A purge stops the Focus's live runs first
  (writer_gate::request_stop_for_focus + wait_focus_runs_ended, bound by attach_lease to
  persona+Focus) and REFUSES with nothing changed if one ignores the stop for 60 s
  (RunsStillRunning). Both purge_device (since items.id=888, F15) and purge_all_devices cancel the
  Focus's parked runs (every one: the code cannot tell which run read which device). NOT reached: chat
  messages, run outputs, focus_runs text of other runs, disclosure_log, fact-hash preference rows.
  Fail-injection hooks (PurgeStep) exist for tests only.
- Commands (commands/device_library.rs, Group 32, the consent-command pattern, none consent-gated):
  list_devices, get_device_sheet (read-only view; also addable_facts), preview_device_purge,
  purge_device, purge_all_devices (the separate withdraw-with-purge control), rename_device (device
  first, then refresh_subject_label of PRIMARY-subject cases; a retry completes it),
  set_device_fact, clear_device_fact. Devices are named by E- handle. Codes: persona_not_found,
  invalid_focus_id, device_not_found, invalid_device_name, name_in_use, unknown_field, invalid_value,
  runs_still_running, catalogue_unavailable. NOT in R1: archive, manual create, per-case/delta delete,
  editing the case and change parts of the sheet, ANY visibility setter or flag inheritance (the
  decisions.id=513 flags exist on entities and nothing in R1 reads or writes them for owned entities).
- Frontend (frontend/src/chat/DeviceLibrary.tsx, DeleteEverythingControl.tsx, useDeviceLibrary.ts,
  deviceLibraryApi.ts): a Devices entry beside the memory entry in CollectionConsentSection (not
  shown while the consent confirm is showing), the delete-everything control on the memory screen.
  The purge confirmations say what they delete (what QuietRabbit recorded: facts, tickets and
  configuration changes) and that chat history, earlier replies and the choice record are NOT
  deleted (tests pin it). Dev simulation: localStorage['qr.devSimulateDeviceLibrary'] = empty,
  devices or runs_blocked, behind import.meta.env.DEV, absent from the production bundle.
- Six-intent fixture (conductor/tech_support_draft.rs): 35 rows, 138 of 256 paths (items.id=914: the
  event and closure routers fall back on ne, with an "unclear" value answered by its own reply row; it
  was 33 rows and 126 paths), 2..6 local calls
  per path; its device rows are entity_find_or_create writers and its templates use both device names.
- items.id=633 must absorb personal_011.sql (the owner_focus column and its index into the entities
  baseline CREATE); no shared or outputs file was added. items.id=888 built the archive hook, the store
  sweep, context_key on rehydrate and the resume pin, which CHECKPOINTS the pass-local published ids
  and re-verifies each on resume (an entity id against the running Focus's active owned entities); it
  built no entity purge.
