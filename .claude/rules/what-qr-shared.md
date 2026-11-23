---
paths:
  - "src-tauri/src/persistence/shared_log_read.rs"
  - "src-tauri/src/persistence/consent_history_purge.rs"
  - "src-tauri/src/commands/shared_log.rs"
  - "src-tauri/src/commands/shared_log_deck.rs"
  - "frontend/src/navShell/SharedLogView.tsx"
  - "frontend/src/navShell/SharedLogView.css"
  - "frontend/src/navShell/ClearConsentHistoryDialog.tsx"
  - "frontend/src/navShell/sharedLogFormat.ts"
---

## What QR shared (items.id=957, decisions.id=967, 434, 944; design record items.id=956)
A per-Persona view over disclosure_log and consent_audit_records, a sibling view button on the
Library screen after Chat History (HistoryScreen.tsx). Placeholder copy under navShell.shared.*
(Chat-BRAND owns the wording). There is no Persona hub screen.
- READ PATH (persistence/shared_log_read.rs, read only, source-scan pinned). Three steps, no ATTACH:
  (1) ONE personal.db statement over disclosure_log, in two parts (items.id=958 unit 2): EVENT rows grouped by
  (local day, run, event type, provider, declined) for every row that does not make a send, and ACTION rows,
  one per (run, action) with flags (gate1_pass, approval, review mark, held) and the most a gate1_pass row
  withheld. Only routing_tier > 1 rows are read, so tier-1 rows are never listed. (2) outputs.db focus_runs.id -> focus_id for the
  DISTINCT runs only (chunks of 500), (3) shared.db provider name and mode and Focus display name. The view
  loads from the summary and never pages every row; a group's items load on open. Days are the user's local
  days (the caller passes the UTC offset). fields_abstracted is read ONLY as json_each keys: no field VALUE
  leaves the store (sentinel test plus a source scan). The cost is per-row JSON work (a created_at index did
  not help, so none exists); see MEASURED below. A real month is a few hundred rows.
- DECK (commands/shared_log_deck.rs, pure). The classification runs in Rust before IPC: raw event strings and
  raw field names never cross it. Placement: Sent, KeptBack, Choice, Hidden (background checks, behind "Show
  background checks"), Excluded (tier 1: provenance_cross_persona_omitted). Rulings: gate3_source_spans_unreviewed
  and voice_profile_contamination are KeptBack; gate2_contamination_detected is Hidden (the reply is kept and
  flagged, executor reply_gate2); output_scan_full is KeptBack only when override_declined, else Hidden; the other
  output_scan_* are Hidden; gate3_consent_pending (a writer the first deck missed) is Hidden. A test scans the
  writers' source for every event type and fails on one the deck does not list, so a NEW WRITER NEEDS A DECK ROW,
  a sentence key in sharedLogFormat.ts sentenceKey() and a string under navShell.shared.sentence. A frontend test
  reads the Rust deck so a key the backend can send but the frontend cannot translate fails.
- LABELS. Catalogued fields use personal_fact_fields.json labels (City, Job title, Employer, Industry); a key
  naming a draft or message reads "A draft"/"A message"; everything else is "Other details" (once). A raw name is
  never returned (sentinel test over the serialized output).
- COMMANDS (Group 40 in ipc.rs): get_shared_log_summary (one row per day and Focus, older days as month rows,
  filters, empty state, cleared_on, record count) and get_shared_log_group_items (labels and sentence keys).
- COUNTING (items.id=958 unit 2, decisions.id=967 addendum; deck::action_outcome is the one pure rule and
  the SEND_EVENTS, HOLD_EVENTS and REVIEW_MARK_EVENTS constants build the read query too). ONE send per
  (focus_run_id, action_id), never per row. A send is an action with a gate3_promotion_approved or
  gate3_pf_no_spans row, or a gate1_pass row and NO held gate3 row for it; a held row alone means 0 sends and
  the held row still counts as kept back, one each. Retries and resumes only repeat rows of one action, so
  they count once. REVIEWED (the summary line, sent.reviewed) = the action has gate3_promotion_approved,
  gate3_consent_pending or gate3_fact_reapplied; a send whose only gate3 row is gate3_pf_no_spans is a PLAIN
  send (sent.noReview, decisions.id=967 point 2): it counts and is not reviewed. kept_back: withheld counts
  once per send, from the gate1_pass row only (the most over retries), plus 1 per held row. Provider: the
  action's non-null provider; the provider filter keeps a send whose provider matches and hides held items,
  choices and background checks. A gate1_pass with no held row is a send even if the step later parked or the
  call failed: gate1 is written before the review and the call, and step_reviews is not read (accepted, D4).
  A retry, a resume or a re-entry (reenter_step) of one step in one run writes the same (run, action) and
  reads as ONE send, so the view can UNDER-report how many times a provider received content; a gate1_pass
  with no held row counts even if the step parked or the call failed (OVER-report).
  A reapplied choice is shown as an item and is not a send. The group's items (build_items) fold the same way:
  one item per send, held and background rows one each, an action across two days shown on the day of its
  newest sending or held row.
- API CHAT. The synthetic run that reviews one message (run id prefix API_SEND_REVIEW_RUN_ID_PREFIX or focus
  api-chat-send, action api-chat-send, output_store::API_SEND_REVIEW_FOCUS_ID) never counts as a send: its held and Hidden rows are shown in the
  api-chat group and its approval row is ignored. It is recognised by the run id PREFIX as well as by its
  focus_runs row, because a zero-span review writes no focus_runs row (write_chat_copy_consent_decisions
  writes it only when the user answers a span). The send is the api-chat run's own gate1_pass and reads
  plain (not reviewed): nothing in disclosure_log links the two runs and no column is added.
- LINK FROM A SEND TO ITS PROVIDER (items.id=958): gate1 and the outbound review run in one execute_once, so
  the gate3 rows carry the same provider as the gate1_pass row (unit 1: Gate3Scanner.provider_id, API Chat's
  bound destination). Step 13 and the copy review write None (the destination is not selected yet / there is
  none); Step 13 fires only after an external step, so its row shares that step's action and counts once.
  Rows written before unit 1 keep provider None and the fallback wording. The four output_scan writers write
  None on purpose (no send, Hidden rows).
- MEASURED (synthetic, in memory, 1e5 rows shaped as sends, three actions to a run, same machine and parsing):
  30-day window 151 ms against 174 ms for the pre-change statement, everything 558 ms against 625 ms; the
  window gives 13.9k action rows and 26 groups; a heavy 66-group month is 16121 bytes of JSON, unchanged in
  shape (the payload is built in Rust from the same groups).
- PURGE (persistence/consent_history_purge.rs, clear_consent_history). HOUSEKEEPING, NOT A PURGE OF THE
  CONSENTS (Jason's call, Chat-PM ruling): it never deletes a record showing a consent in force. KEPT:
  every history_cleared marker; per Focus the newest collection_consent_* record IF it is a grant; per Focus
  and document type the newest document_ceiling_raised record. DELETED: every other consent_audit_records
  row (superseded grants, withdrawals, older raises, all other types). "Newest" is created_at then rowid,
  the same order and rev/rfind idiom as latest_collection_event and previous_raise, so those two still see
  the kept record. ONE rule, deletable_ids, serves both the purge and the count the summary reports
  (consent_record_count is what a purge would delete, and the confirm text shows that number). One
  transaction: the deletes (one by id each), then a history_cleared marker (focus_slug "*") through
  consent_audit_store::append; if the marker cannot be written the deletes roll back. No purge removes a
  marker, so the "cleared on" line is permanent. Nothing to delete writes nothing (nothing_to_clear), so a
  purge that only keeps records leaves no second marker. A record of an unknown event type makes the store's
  list fail, and the purge then fails closed. The command needs confirm true, takes writer_gate first and
  checks the Persona. disclosure_log is never deleted and the module never names it (source scan).
  consent_audit_store.rs gained only the HistoryCleared variant.
- NOT VERIFIED: Windows and macOS rendering and font fallback (checked in the in-app browser only, at 375 px
  and desktop, with a stubbed backend); the real encrypted-file path was exercised by the command tests but not
  hand-checked in the running app; the sixth Library button wraps because the row is flex-wrap (read from the
  CSS, not seen in the real Library screen).
