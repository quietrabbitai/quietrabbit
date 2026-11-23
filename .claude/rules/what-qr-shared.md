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
  (1) ONE grouped personal.db query over disclosure_log (local day, run, event type, provider, declined;
  routing_tier > 1 only, so tier-1 rows are never listed), (2) outputs.db focus_runs.id -> focus_id for the
  DISTINCT runs only (chunks of 500), (3) shared.db provider name and mode and Focus display name. The view
  loads from the summary and never pages every row; a group's items load on open. Days are the user's local
  days (the caller passes the UTC offset). fields_abstracted is read ONLY as json_each keys: no field VALUE
  leaves the store (sentinel test plus a source scan). Measured, synthetic, in memory: 27.8k in-window rows
  grouped in ~220 ms (the cost is per-row JSON work, a created_at index did not help, so none exists); a heavy
  month of that data is 66 summary rows (~16 KB JSON). A real month is a few hundred rows.
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
  filters, empty state, cleared_on, record count) and get_shared_log_group_items (labels and sentence keys). A
  provider filter keeps only Sent items to that provider. Sends count gate1_pass, promotion and no-review rows;
  a reapplied choice is shown but is not a send. "Reviewed" in a summary line means every counted send was a
  gate3 promotion row: the link from a reviewed send to its provider is NOT verified (gate3 rows write provider
  None, so a reviewed send reads "an outside service" and is absent under a provider filter).
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
