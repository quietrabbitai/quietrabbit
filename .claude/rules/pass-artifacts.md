---
paths:
  - "src-tauri/src/persistence/pass_artifact_store.rs"
  - "src-tauri/src/conductor/seeding.rs"
  - "src-tauri/src/conductor/bound_spec.rs"
  - "src-tauri/src/conductor/store_op.rs"
  - "src-tauri/src/conductor/store_writer.rs"
  - "src-tauri/src/conductor/declared_inputs.rs"
  - "src-tauri/schema/outputs_015.sql"
---

## Per-pass artifact store (items.id=895, decisions.id=942 point 5, decisions.id=938 ruling 2)
Multi-turn work rides on chat-level passes, so text a later pass needs from an earlier one
(a brief, voice guidance, a validation report, delta items, a working draft) that does not fit a 2048-byte
session variable lives in pass_artifacts, in the persona's encrypted outputs.db
(schema/outputs_015.sql, persistence/pass_artifact_store.rs). items.id=633 must absorb
outputs_015 with the other outputs files.
- Table: one row per (context_key, focus_id, kind), content 1 to 16384 bytes (the schema
  CHECK and ARTIFACT_MAX_BYTES), version compare-and-set, no foreign keys, no triggers, no
  revision history (a write replaces the text). kind is the closed Rust enum ArtifactKind
  (no SQL CHECK, a round-trip test pins it): content_brief, voice_guidance,
  validation_report, delta_items, working_draft (items.id=903, one enum line, no rebuild).
  session_intent is not a kind. The 16384-byte cap is the Writing Assistant's document limit
  (see "Writing Assistant").
- Store module: put_artifact, get_artifact, clear_artifact ONLY, over session_store's
  BEGIN IMMEDIATE transaction and one-time latch (a latched chat refuses every write and the
  clear, an unreadable latch fails closed). Stored text is exact: nothing rewrites it.
- RESERVED BOUND NAMES: content_brief, voice_guidance, validation_report, delta_items,
  working_draft (bound_spec.rs ARTIFACT_NAMES, Source::Artifact, Presence::Optional, Authored). A name
  equals its kind, a test pins it, and the name is the artifact's alone: LOAD refuses it as
  a row's output_var (declared-inputs lint), as a session_put target and as the argument of
  any store writer. A step that produces one of these texts uses another output_var (the
  cb-13 refine example uses refined_guidance) and an artifact_put row stores it under the
  name. cb-12's brief_var content_brief is the artifact, read in a later pass.
- Writers (store_op.rs, store_writer.rs; no second write path): artifact_put (store_target is
  the artifact name, one slot `value` from an earlier step's output variable) is gated by the
  collection consent like session_put (a per_field Focus is always Granted). artifact_clear
  (store_target only, no slots) removes one kind of the running Focus and chat and is not
  gated, like session_clear. Both are latch-gated and scoped to the running chat and Focus,
  never a row's choice. The writer reads the current version and writes with it, as
  put_session does.
- Reads: ONLY as bound names, seeded once at pass start (seeding.rs read_artifact). They are
  store-backed like every bound name: restricted to the step whose own template names them,
  tainting a step that names one (so it stays out of extraction), and, since items.id=884
  flipped STORE_NAMES_EXTERNAL_ALLOWED to true (declared_inputs.rs, pin test
  the_store_name_flag_ships_true_and_the_starter_device_list_does_not_depend_on_it), allowed in an
  external step that DECLARES it (the declaration rule decides, and the outbound review sees
  the text). A stored text that fails its validator fails the pass closed.
- TWO BEHAVIOURS LATER BUILDS MUST RESPECT (items.id=903 and 907 carry the same note):
  1. Seeded artifact text has `{` and `}` turned into `(` and `)` (seeding::clean, as for the
     derived names, so stored text cannot become a prompt token). The stored text is exact,
     so a JSON delta_items renders with parentheses in a prompt. Read it through the store
     function, not a prompt, when the exact text matters.
  2. An absent artifact renders as NOTHING: Optional presence means no seed entry, so a
     guard on it is false and its token is stripped. A step that needs an artifact must be
     guarded on `present`, or it runs with an empty input. A value written in a pass is seen
     by the NEXT pass (seeds are read once at pass start), never by a later row of the same
     pass.
- Logging: artifact text never reaches a log or an error. The one log line is
  store_writer::log_artifact_write (kind, outcome, a Fingerprint of byte count and a short
  SHA-256 prefix), pass_artifact_store and seeding log nothing, Debug on PassArtifact and
  ArtifactWrite redacts the text, and source-scan tests pin all of it. Use fingerprint() if
  you must log one.
- RETENTION AND PURGE (items.id=888, see "Resume state, archive hook and store sweeps"): the
  archive hook deletes a chat's artifacts (pass_artifact_store::clear_chat_in_tx) and the
  30-day sweep deletes any not written for STORE_RETENTION_DAYS (sweep_older_than_in_tx, on
  idx_pass_artifacts_updated_at). clear_artifact stays the only delete a pass can ask for.
  Not reached by the entity purge (an artifact names no entity id).
- items.id=903 WRITERS: brief_put (target = the pass variable an outcome word is published
  under; slot value) parses one StructuredBrief JSON object (first { to last }, no unknown key,
  validated), renders it with the shipped labels and stores content_brief, publishing `stored` or
  `invalid`. artifact_append (slot value) appends ONE section under its heading to working_draft;
  the heading is computed in the writer from the stored brief and draft (conductor::sections),
  never taken from a variable; it publishes `appended`, `too_long` (past the cap, nothing written)
  or `no_section`. Both read the one-time latch FIRST, so a latched chat is RefusedOneTime, never
  an input refusal; both are consent-gated. A derived name (next_section, section_heading,
  sections_state) may not be a writer's argument (the loader refuses it).
