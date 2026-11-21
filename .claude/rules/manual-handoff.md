---
paths:
  - "src-tauri/src/conductor/handoff_*.rs"
  - "src-tauri/src/conductor/taint.rs"
  - "src-tauri/src/commands/handoff.rs"
  - "src-tauri/src/commands/ingest.rs"
  - "src-tauri/src/persistence/handoff_draft_store.rs"
  - "src-tauri/src/persistence/ingested_document_store.rs"
  - "src-tauri/src/persistence/session_store.rs"
  - "src-tauri/schema/shared_046.sql"
---

## Manual hand-off (items.id=387, decisions.id=942 point 7, decisions.id=846)
A person takes a starter to a provider by hand (a chat site, a model they hold an account
with) and pastes the answer back. It is TWO CHAT PASSES, not a parked step: nothing waits
in a run, so nothing depends on resume. The legacy requires_user_handoff pause is untouched.
- OUTBOUND (cb-15, BlockKind::Render, conductor/handoff_render.rs). A Focus row cites cb-15
  directly with `{display_name, output_var, template, declared_inputs}` (typed, every key
  required). It renders the template from its DECLARED inputs only (values brace-cleaned
  through seeding::clean, so one value cannot pull in another's token), records the starter
  as the pass's output and then sets handoff_pending. NO model call, no provider, no gate:
  the copy-time review (decisions.id=846: "Copy last response" and every copy path) is the
  gate, and there is no export command. The starter must be the pass's LAST output, because
  that button copies the newest assistant reply. A source scan pins that the render row never
  names execute_once, hosted_step_call, a provider, a gate or the spend gate, and the render
  dispatch sits before the tier computation in execute_step. Its only use of the executor is
  render_template (pub(crate) since this item) and find_tokens.
  items.id=883: its {user_input} is the user's own turns only (FocusRun.external_input, never the
  window that holds model turns), because a render row is an external step to the lint.
  - It is a block row to the registry and an EXTERNAL step to the declared-inputs lint
    (ActionDefinition::is_external_for_lint): declared inputs only, no persona_context,
    voice_profile, previous_output or focus_context (refused at parse, declared or not),
    only {user_input} among the system tokens, no field_requirements. STORE_NAMES_EXTERNAL_ALLOWED
    is true (items.id=884 flipped it), so a starter may declare a store-backed name, content_brief
    included, and {user_input} even in a Focus that also has a handoff_ingest row (taint below).
    It may NOT name or declare the two device names (subject_context, device_library_brief), nor a
    variable derived from one: DEVICE_NAMES_NEVER_IN_STARTER refuses them whatever the flag says
    (items.id=924 lifted the refusal for external MODEL steps and kept it for starters, which have no
    external rendering, carry scan or outbound review; see "Device facts in external steps and allowed
    answers"). The real starter naming content_brief is therefore no longer blocked by the flag;
    it is built with the Writing Assistant (still not built).
  - Its record (handoff_render::inherit_from_inputs), over each DECLARED input: (1) takes the
    highest sensitivity_severity and ORs personal_flagged and audit_unavailable of the step that
    produced it (the last producer of the variable); (2) a STORE-BACKED declared input counts as
    PERSONAL: the record's sensitivity_severity is at least 2 (Sensitivity::Personal) and
    personal_flagged is true, whether or not a producer exists and whatever it recorded (a floor,
    it never lowers rule 1; audit_unavailable still comes only from producers). Store-backed =
    a registered bound name, a variable a step the taint closure marks as tainted produced, or
    user_input in a Focus that has a handoff_ingest row (handoff_render::store_backed_inputs);
    (3) any other declared name (the {user_input} token with no return row, a field name)
    contributes nothing. Items.id=884's rider; only declared inputs count. routing_tier_used
    stays 1: the render itself calls nothing. (An EXTERNAL MODEL step's record used to score only
    Gate 1 projected fields, so a store-backed input it names was not marked personal: items.id=922
    closed this gap, see "Persona context, the output scan and external-step scoring".)
  - handoff_pending (Session bound name, `h_` plus 12 lowercase hex of SHA-256 over run and
    row) is written by the render through the typed session store: latch-gated (a one-time
    chat still shows the starter and records nothing), NOT collection-consent gated (no user
    content). A run with no chat renders the starter and records nothing.
  - EXTERNAL_MODEL_BLOCKS_ALLOWED does not govern this: it is one boolean for a MODEL block's
    external destinations (hosted_api, manual_handoff) and still waits for B10. A ManualHandoff
    range of a model block is still refused. items.id=901 does not change that: the run plan
    LISTS manual hand-off as a strategy only when every reachable path is a cb-15 starter with no
    model call, and it never selects one.
- RETURN (handoff_ingest, conductor/handoff_writer.rs, the 17th store op). The pasted reply
  is the NEXT chat turn. The row is guarded on handoff_pending `present` (LOAD refuses it
  otherwise), has NO slots and a required store_target (the pass variable the H- handle is
  published under), and is gated by the collection consent. Order: consent, the one-time
  latch (a latched chat stores no blob), a turn must exist, the paste caps, a hand-off must
  be pending, store_draft, then session_store::settle_handoff_return clears handoff_pending
  and sets handoff_received in ONE BEGIN IMMEDIATE transaction. A crash between the draft and
  that transaction leaves a stored reply the chat does not point at and the hand-off still
  pending, so the retry stores a second reply: accepted, both are the user's Library documents.
  - THE ONE EXCEPTION to "raw user text is never a store argument" (recorded in the store_op.rs
    header): handoff_ingest reads the raw turn from WriterContext.turn_text, because a
    variable would be a model's copy of the paste. The turn is FocusRun.current_turn (a
    CurrentTurn: not Clone, not Serialize, not Display, Debug redacts it), set only by
    send_message through set_current_turn, lent as TurnText whose accessor is private to
    handoff_writer.rs, NEVER persisted, checkpointed, snapshotted, serialized or logged
    (the one log line carries a byte count). rehydrate_focus_run never sets it, so a resumed
    run has none and the op fails closed. Scans in conductor/handoff_writer_tests.rs pin
    that no other file reads turn_text and that there is one sender, one field and one lender.
  - The reply is stored through persistence/ingested_document_store.rs (the three ingest writes
    moved out of commands/ingest.rs, behaviour unchanged) by persistence/handoff_draft_store.rs
    as an ingested output of type handoff_draft filed under the Focus: sensitivity personal,
    PASTE_MAX_BYTES 256 KiB, non-blank, no NUL, no lease (the run holds one), exact text. It
    is UNTRUSTED EXTERNAL CONTENT read ONLY by its H- handle (handles::Kind::Draft, resolved
    among the Focus's own non-deleted replies) through read_draft_by_handle; no template names
    a handle and no seeded name carries the text (a Derived handoff_draft name is deferred
    until items.id=907 states its size needs: derived text caps at 6000 bytes, a reply at 256
    KiB). The chat turn that carried the paste has no byte cap of its own (commands::messages).
  - TAINT (conductor/taint.rs): in a Focus that has a handoff_ingest row, `user_input` is a
    tainting name, because the blended chat window ({user_input}, focus_runs.user_input) carries
    the paste as its newest message for up to HISTORY_WINDOW messages. A step naming it is left
    out of extraction (and so is a step naming its output). Since items.id=884 flipped
    STORE_NAMES_EXTERNAL_ALLOWED an external step of that Focus MAY name and declare it, behind
    the outbound review; a render row that declares it counts as personal (the rule above). A
    pasted reply is not device content: the device closure leaves user_input out.
  - handoff_pending and handoff_received are reserved: never a session_put target, never a
    writer's argument, never an output_var. Both are seeded at pass start and are ordinary
    store-backed names otherwise (restricted to the step whose template names them, tainting
    it, and allowed in an external step only when declared, since the flag is true). Their
    normal use is a guard.
- CONSENT, FOR A FOCUS AUTHOR. In a focus_level Focus WITHOUT the current consent,
  handoff_ingest is SILENTLY SKIPPED by the collection-consent gate (declining is not a block:
  the Focus keeps working and saves nothing). A reply row such as an acknowledgement would
  still say "received" when nothing was saved, so a Focus author MUST GUARD THE REPLY ON
  CONSENT STATE, and the hand-off panel reads get_collection_consent_state next to
  get_handoff_state. A per_field Focus (the Writing Assistant) is always Granted.
- STATE FOR THE PANEL (commands/handoff.rs, IPC Group 33). get_handoff_state(persona_id,
  context_key, focus_id) is READ ONLY and never returns a reply's text: pending
  `{id, since}`, received `{handle, byte_count, received_at}` (None when the reply was deleted
  from the Library) and one_time_chat. Errors are the codes in handoff::ERROR_CODES
  (persona_not_found, invalid_focus_id, invalid_context_key, handoff_state_unavailable), each
  with a placeholder key handoff.error.<code> in frontend/src/i18n/en.json (Chat-BRAND owns the
  wording; a test requires a key per code). There is no write command, no export command and
  no paste command: the starter is copied with the existing copy review and the paste is sent
  through send_message. A cancel control for an abandoned hand-off is items.id=900's design and
  must be a pass (a guarded typed row), never a command that writes session_variables.
- WHAT items.id=912 EXPECTS: get_handoff_state, the two session names for guards, the fixture
  Focus pattern (a render row guarded on handoff_pending absent, a reply row and handoff_ingest
  guarded on present; the reply row sits BEFORE the ingest row in the fixture because the ingest
  row records nothing on the track and a real Focus follows it with model steps), and
  items.id=643 (below) landed before the panel offers any delete.
- RETENTION (for items.id=888, which must NOT repeat the pass_artifacts gap): the two session
  names are rows of session_variables, so clear_chat and the 30-day sweep cover them with no
  change. The pasted reply is NOT chat scratch: it is a durable Library "Imported" document the
  user owns, removed only through the Library, so clear_chat does NOT delete it (a deliberate
  ruling, items.id=387 plan Q1). The Library's soft delete zeroes `content` and sets deleted_at
  but leaves the encrypted blob file on disk: items.id=643 (it now blocks the panel). It is
  existing behaviour for every imported document and was left alone here.
- SCHEMA: shared_046.sql (data only, the cb-15 mirror row; the migration tests re-pinned to 46).
  No new table or column. items.id=633 must absorb shared_046.sql with the other shared files.
- NOT BUILT: the real Writing Assistant starter (the flag flip it waited for has landed), a
  Derived handoff_draft name, multiple hand-offs inside one pass (the return pass's row order is
  the Focus's), file or document replies beyond a plain paste, any model step after the return
  row (B7c delta validation reads the reply by handle).
