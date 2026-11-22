---
paths:
  - "src-tauri/src/commands/collection_consent.rs"
  - "src-tauri/src/conductor/collection_consent_text.rs"
  - "src-tauri/block_data/collection_consent_text.json"
  - "src-tauri/src/persistence/consent_audit_store.rs"
  - "src-tauri/src/persistence/focus_settings_store.rs"
  - "src-tauri/schema/personal_010.sql"
  - "src-tauri/schema/shared_044.sql"
  - "frontend/src/chat/*ollectionConsent*"
---

## Collection consent (items.id=920 stage 1, decisions.id=944, decisions.id=509)
A focus_level Focus (Tech Support) collects silently after ONE informed consent at
setup. Stage 1 is the store and the gate, stage 2 the commands and the consent text
data, stage 3 the frontend confirm.
- State: focus_settings.collection_consent (not_asked, granted, declined, withdrawn)
  and collection_consent_version (shared_044.sql). Default not_asked, so a row never
  asked never opens the gate. focuses.collection_posture (per_field default,
  focus_level) is DECLARED BY THE FOCUS (the focuses row or the .focus key
  collection_posture) and is never "not required". focus_level also means the generic
  extraction pass is skipped (items.id=914: conductor::extract::extraction_pass_applies(screen_built,
  posture), a pure posture-only fn, called by execute_and_finish after the EXTRACT_CONFIRM_SCREEN_BUILT
  block; a missing definition counts as focus_level; the consent row is never read for it. While the
  constant is false nothing runs for any Focus, so extract.rs's truth-table test is the LIVE GUARD that
  items.id=38 inherits when it flips the constant).
- ONLY focus_settings_store::set_collection_consent writes the state (stage 2's
  command is its only intended caller). The columns are NOT in
  UpdateFocusSettingsRequest, update_focus_settings does not touch them, so
  submit_friction_gate_decision cannot move them. No model path records consent:
  source scans in conductor/store_op_tests.rs pin that store_writer.rs, store_op.rs
  and lifecycle.rs never name the audit table or the setter, and that nothing outside
  tests calls either except commands/collection_consent.rs. The set_collection_consent
  version rule: Granted and Declined store the text version shown (1 or more), NotAsked
  and Withdrawn store 0.
- Text: block_data/collection_consent_text.json holds text_version and promise ids ONLY
  (conductor/collection_consent_text.rs; the closed PROMISES list is what QR can keep
  today, so editing reference sheets, per-case or per-output delete and cross-Focus use
  are not promisable). Prose is placeholder i18n copy in the frontend, owned by
  Chat-BRAND. Bumping text_version forces a new decision from every Persona.
- Commands (commands/collection_consent.rs, Group 31): get_collection_consent_state,
  submit_collection_consent_decision (granted or declined, with the version SHOWN; a
  version that is not current is consent_text_stale), withdraw_collection_consent,
  list_consent_audit_events. Each takes writer_gate::command first, derives the user and
  key from the session (no user_id argument), checks the Persona id is a UUID the user
  belongs to (private require_persona, for items.id=638 to lift) and has a *_for_session
  body. Write order is fail-safe both ways: a GRANT appends the audit event FIRST and the
  flag second (a crash leaves collection off, the retry reuses the event); a WITHDRAWAL
  clears the flag FIRST and appends the event second, and errors if the event fails so the
  frontend retries. A decline appends no event of its own, but on EVERY decline call, if the
  newest collection event is still a grant it appends collection_consent_withdrawn
  referencing it (retry semantics as withdraw). A withdrawal keeps what was collected and
  deletes nothing. needs_decision is true for not_asked, and for granted or declined at a
  version other than the current one; a withdrawn user is never re-asked.
- Audit: consent_audit_records in each Persona's encrypted personal.db
  (personal_010.sql, next to disclosure_log; never shared.db, decisions.id=862).
  persistence/consent_audit_store.rs has append, list and latest_for_focus ONLY: no
  UPDATE, no DELETE (source-scan test). Event types are the Rust enum
  ConsentEventType (no SQL CHECK, round-trip test). content_summary is a fixed abstract
  description, never user content. A purge is a separate user-initiated function.
- Gate: WriterContext.collection (Granted or NotGranted) is built in
  FocusRun::current_collection() from a FRESH read of focus_settings immediately before
  EACH writer op (not the AUTHORIZE snapshot). A per_field Focus is always Granted and
  never reads the row. A focus_level Focus is Granted only if collection_consent =
  granted AND the version equals the current text version; a missing row, a missing
  definition or an unreadable state is NotGranted. run_store_op checks it FIRST (before
  the context key and the latch) for every Op::requires_collection_consent() op and
  returns Outcome::RefusedNoConsent, writing nothing and calling no store function.
  NOT gated: Latch (it suppresses writes) and SessionClear (it removes data). Declining
  is not a block: the Focus keeps working and saves nothing.
- Latch and consent are independent: granted and open writes, granted and latched is
  RefusedOneTime, not granted is RefusedNoConsent. Granting mid-chat never lifts a latch.
- write_focus_run_record's keep_input is also consent-aware: the blended chat window is
  not persisted in focus_runs.user_input for a focus_level Focus without the current
  consent (an unreadable state counts as not granted).
- Frontend (frontend/src/chat/CollectionConsent*.tsx, useCollectionConsent.ts): ChatPane
  renders CollectionConsentSection for the Focus ids in collectionConsent.ts (tech-support
  only). It asks ONLY when get_collection_consent_state says needs_decision (the frontend
  never recomputes who is asked; a test scans for it), with fixed local i18n text (placeholder
  copy, Chat-BRAND) and one confirm ('Allow Tech Support to remember' / 'Use without saving')
  that sends the DISPLAYED version. The memory screen (stop, or allow again) says plainly that
  what QuietRabbit already knows stays until the user deletes it. Dev simulation: in a Vite dev
  build, localStorage['qr.devSimulateCollectionConsent'] = not_asked, stale_granted,
  stale_declined, declined, granted or withdrawn fakes the state (and the missing
  focus_settings_row of tech-support) in memory; the fake is tree-shaken from a production build.
- items.id=633 will fold personal_010 and shared_044 into the baselines; both files are
  self-explanatory in their headers (decisions.id=857).
