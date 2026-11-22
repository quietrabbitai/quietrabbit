---
paths:
  - "src-tauri/focus_specs/writing-assistant.json"
  - "src-tauri/schema/shared_048.sql"
  - "src-tauri/schema/outputs_020.sql"
  - "app/core_artifacts/focuses/writing-assistant.focus"
  - "src-tauri/src/conductor/sections.rs"
  - "src-tauri/src/conductor/check_block.rs"
  - "src-tauri/src/conductor/document_raise.rs"
  - "src-tauri/src/conductor/writing_assistant_tests.rs"
  - "src-tauri/src/conductor/model_block/elicit.rs"
  - "src-tauri/src/persistence/document_session_store.rs"
  - "src-tauri/src/commands/document_session.rs"
  - "src-tauri/src/commands/document_raise.rs"
---

## Writing Assistant (items.id=903, decisions.id=941 point 1, 942, 944, 947, 963)
An end-stage Focus made only of blocks and configuration. Every step is LOCAL in this item:
EXTERNAL_MODEL_BLOCKS_ALLOWED is false (items.id=913), so the document privacy flag and the
one-document raise are built, audited and read, but can send nothing out until 913.
- SOURCE OF TRUTH: src-tauri/focus_specs/writing-assistant.json, validated through the real
  loader and emitted by focus_dev as schema/shared_048.sql (data only, DELETE then INSERT; it removed
  the shared_038 rows, the external draft_writing step that named {user_input} among them). Edit the
  spec and emit again; the drift test checks the header hash. The .focus file is display metadata
  plus ONE local step (the fallback, and the stricter-tier cross-check keeps its max_routing_tier 2).
- PASSES (each user message is one pass; 33 rows, 78 guard paths pinned, cap 256):
  new document (content_brief absent, not quick): pc-03 brief, brief_put, a reply guarded on the
  published word (stored, invalid); quick (launch_mode quick, no draft yet): cb-12 from the request,
  the plain advisory cb-06, cb-13 apply, artifact_put working_draft; draft (brief present, no draft,
  not quick or high): cb-12 from content_brief, cb-06 verdict, a second cb-12 and a second check
  (attempt 1) only on `regenerate` and only in medium, the content-free report (artifact_put
  validation_report), cb-13 apply, artifact_put working_draft; revise (a draft exists, not high):
  cb-14 on working_draft with the request, report, apply, store; high: one section per pass
  (cb-12 from next_section, a section report, apply, artifact_append) and, once sections_state is
  done, the revise rows again. A stored value is seen by the NEXT pass only. A new document needs a
  new chat (once working_draft exists every turn revises it).
- LAUNCH: launch_mode (quick, low, medium default, high; high narrows the quality floor to standard)
  is the only guarded launch slot (constraint 5); document_type (six shipped types, personal_note
  default) changes the expansion, never a guard. The rows name essay_section (no sender or context
  fields) and the run's type replaces it, so sender details and writer background are not supplied
  in R1 (B7a).
- CONSTRAINTS PINNED (writing_assistant_tests.rs, lifecycle tests): every step local; {user_input}
  is read only by brief, quick_draft, revise and high_revise (the window byte cap stays deferred);
  no step names {persona_context} or a launch name; no row is guarded on model text; every model
  step resolves on the shipped capability cells in every mode and type, and the brief and voice
  steps park TooWeak without their quick_response cells. The pass harness runs the real composition,
  seeding, guards, cb-06 and store writers over an encrypted outputs.db with SCRIPTED model outputs:
  no prompt, gate, resolver or provider runs there.
- DOCUMENT LIMIT (ruling 6): working_draft is a pass artifact, capped at 16384 bytes (schema CHECK,
  ARTIFACT_MAX_BYTES). The pass context agrees: the largest curated local window is 8192 tokens and a
  revise pass carries the draft in and out, about 15.6 KB at an ESTIMATED 4 bytes per token.
  artifact_append past the cap publishes too_long and a reply says so; an artifact_put over it fails
  the pass closed.
- DOCUMENT SESSIONS (outputs_020.sql, document_session_store.rs): one row per chat and Focus:
  document_type, document_privacy_flag (general, personal, medical, financial, legal; a closed enum,
  no CHECK), raised_ceiling and consent_audit_id; no content column. set_privacy_flag is latch-gated
  and CAS; a changed document type voids a raise; the flag is stamped on the saved output
  (outputs.document_privacy_flag). The archive hook and the 30-day store sweep remove rows.
  Command: set_document_privacy_flag (a one-time chat is refused; Chat-PM's rider on items.id=913).
  The user may loosen the flag (it is their label); a flag change never lifts the run above the
  user's or the Focus's ceiling (pinned).
- RUN CEILING: resolve_tier_config takes effective_ceiling(session, run type, focus_settings tier):
  the flag makes it local_only, a one-document raise of THIS type lifts it to the raised tier
  (overriding the flag for this document), then the Focus ceiling clamps. focus_settings is never
  edited. A read failure ends the run start.
- ONE-DOCUMENT RAISE (Path B of decisions.id=942 Q14; conductor/document_raise.rs,
  commands/document_raise.rs): request_document_ceiling_raise checks, on fresh reads and in order,
  protected_persona (persona_type OR focus_profile protected), one_time_chat, no_document,
  above_focus_ceiling, not_a_raise, then returns a challenge: a single-use 128-bit token in memory
  only (RaiseChallenges, cleared at logout or lock), the fixed disclosure fields, a 5 s minimum wait,
  10 min expiry and an earlier raise's time. confirm_document_ceiling_raise refuses an invalid, used,
  foreign or expired token, too_early (the token stays valid) and not_acknowledged, re-checks the
  rules, appends the consent audit row FIRST (personal.db, document_ceiling_raised, document type,
  fixed summary, consent_reference_id = the earlier raise, text version) and writes the raise
  SECOND (set_raise, its only caller). A fault after the audit leaves the ceiling unraised. A raise
  approves no hosted or metered step: the run plan, spend gate, resolver and outbound review do not
  name it (scan). Wording (document.raise_error.*) is placeholder for Chat-BRAND; no screen
  (items.id=912).
- NOT BUILT: hand-off rows (no selector without a second guarded slot or the run plan), multi-round
  elicitation and brief adjustment (items.id=906), a refine step (nothing consumes voice guidance),
  sender details and writer background (B7a), the screens (items.id=902, 912), a full executor run
  with real models (by hand), Windows and macOS.
