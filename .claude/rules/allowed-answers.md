---
paths:
  - "src-tauri/src/conductor/privacy/allowed_answers.rs"
  - "src-tauri/src/conductor/privacy/gate3.rs"
  - "src-tauri/src/conductor/step_review.rs"
  - "src-tauri/src/conductor/carry_scan.rs"
  - "src-tauri/src/conductor/device_seeding.rs"
  - "src-tauri/src/conductor/resume_pin.rs"
  - "src-tauri/block_data/device_fields.json"
  - "frontend/src/cloudChatAccess/**"
  - "frontend/src/stepReview/**"
---

## Device facts in external steps and allowed answers (items.id=924, decisions.id=507, 513, 945, 953)
A device's serial number can now go into an RMA letter written by a hosted step, behind the outbound review,
while what must never leave cannot be released by any answer. Built on items.id=933's carry scan, not beside
it. Units: allowed answers (U1), catalogue and device spans (U2), external rendering and the lint (U3),
frontend and docs (U4).
- ALLOWED ANSWERS (conductor/privacy/allowed_answers.rs, pure). Every review span carries AllowedAnswers
  {release, standing_release} (ALL, NO_STANDING_RELEASE, NO_RELEASE). The one rule: keep private is always
  allowed; release needs `release`, and a REMEMBERED Persona-wide release also needs `standing_release`;
  where release is not allowed, a generalization whose replacement contains the original (case-insensitive)
  counts as keep private. gate3's partition admits every remembered decision through admit_remembered (a
  refused one is applied as keep private on a no-release span, asked again on a live-only span); an in-run
  consent row the user did not write (auto-reapplied) counts as standing. The outbound review applies the
  same rule to live answers (step_review::applied_decisions: a refused answer is removed, and the counts
  follow what is sent). AskedSpan and SettledSpan store `allowed` (absent = ALL, so old rows read as
  before). Only carry_scan restricts a span (scan only_the_carry_scan_restricts_a_span).
- THE CATALOGUE (block_data/device_fields.json, version 2). Each field has a disclosure: never (removed
  before the review, never shown), ask (every occurrence in a hosted prompt is raised) or pass (sent); an
  ask field has a suggestion (a fixed label such as "[serial number]", placeholder wording, Chat-BRAND's).
  The device NAME (entities.display_name, not a fact) has device_name_disclosure ask, "[device]". THE
  SHIPPED DEFAULT IS CONSERVATIVE AND PER-FIELD REVERSIBLE: the ten fields that were pass stay pass, the
  other ten (serial number, warranty contact and address included) are ask, none is never; any field can
  move either way by a data edit. validate(): an access-credential field (the serial) may not be pass, an
  ask field needs a suggestion, a pass or never field takes none, the credential-word refusal stays. The
  owned-fact writer stores never/ask/pass as not_permitted/omit/pass in entity_facts.abstraction_tier2/3;
  NOTHING READS THOSE COLUMNS for an owned fact (scan), and rows written before keep not_permitted. No
  migration.
- THE DEVICE SEED (conductor/device_seeding.rs). A pass that names a device name keeps a DeviceSeed on
  StoreTokens: every seeded device's name and catalogued facts at their EFFECTIVE disclosure, the flag and
  the suggestion, and what the external rendering needs. In memory only (Debug prints counts), rebuilt each
  pass. decisions.id=513 is evaluated per device at seeding (evaluate_object_visibility, Boundary): a device
  that is not Visible keeps its pass facts and its name and every other fact become never (Boundary is
  invariant: no answer may release them).
- THE EXTERNAL RENDERING (StoreTokens::apply_external_devices, called only by render_prompt_with_disclosure,
  after the seeds; scan). A hosted step that names subject_context or device_library_brief receives the
  same skeleton with never facts dropped, a never name shown as "[device]", and a suppressed device left
  out. The local render is unchanged.
- THE CARRY SCAN, WIDENED (carry_scan::classify_devices and classify_asked, executor execute_once on every
  step with treatment.hosted and no bound API Chat run). A device value the pass seeded that is not pass is
  raised or removed at EVERY occurrence in the final prompt (the derived text, a carried output,
  {previous_output}, the user's own words), stored or brace-cleaned form: never -> "[removed]" before the
  review and a carry_not_permitted_removed row naming the field (no value); ask -> a qr_device_field span
  ("Device detail", placeholder) with its suggestion, its own fact key `device_fact:` + SHA-256 of
  (entity, field, value), which gate3 uses in place of the Layer 1 to 3 cascade, and its allowed answers:
  ALL, except the access-credential field, NO_STANDING_RELEASE (D3: a standing release is asked again, a
  release in this run counts).
- PERSONAL ask (D7). A bridged personal field the user set to ask (set_fact_policy accepts it; no screen)
  is released RAW only to a hosted step that gets the outbound review and only when its value has 4 or more
  characters (executor::to_gate_track_asks); classify_asked then raises every occurrence with every answer.
  Anywhere else ask is omit; at tier 3 it follows pass's rule. Re-read order: not_permitted > omit >
  summarize > ask > pass.
- THE RESUME PIN. A checkpoint whose pass seeded a device records device_seed, the DeviceSeed fingerprint
  (entity, field, value and effective disclosure of every seeded value, and the brief's device count; a hash
  only). A resume seeds the steps' device names again and refuses on a mismatch or a read failure
  (ResumeStoreChanged, "saved chat state changed while waiting"): A DEVICE EDITED, RENAMED, ADDED,
  RE-DISCLOSED OR RE-FLAGGED BETWEEN THE PARK AND THE RESUME REFUSES THE RESUME. No entry when the pass
  seeded no device; a snapshot without one resumes as before.
- THE LINT. The old closed list was temporary for external MODEL steps and is lifted: they may name and
  declare both device names like any store-backed name (declared, tainting, the items.id=922 personal
  floor), and a derived variable or {previous_output}. A narrower refusal stays for cb-15 hand-off STARTERS
  (DEVICE_NAMES_NEVER_IN_STARTER, DeviceNameInStarter) until its own design: a starter makes no model call,
  so it gets no external rendering, carry scan or outbound review, only the copy-time review.
- FRONTEND (cloudChatAccess/spanAnswers.ts, PrivacyGuardianModal.tsx, stepReview.ts). DISPLAY ONLY (the
  server applies the rule whatever is sent): a release the span does not allow is shown disabled with
  privacyGuardianModal.releaseNotAvailable; "Remember this decision" for a release of a live-only span is
  disabled with privacyGuardianModal.rememberReleaseNotAvailable and save_for_persona is not sent; a
  generalization carrying the original where no release is allowed is flagged with
  privacyGuardianModal.editContainsOriginal and does not count as a decision. The three keys are placeholder
  wording, Chat-BRAND's (in use, so not orphans). A span without `allowed` (the copy review) allows all.
- ACCEPTED LIMITS:
  (a) TEXT MATCH. A paraphrase is missed, a value under 4 characters is not scanned, a reformatted value
      (a serial with spaces or dashes added, a zero-padded address) is missed, a substring inside a longer
      word is caught, a device name made of common words over-asks. Aliases are not scanned.
  (b) THE GENERALIZE CHECK IS A TEXT MATCH: a replacement that carries the original reformatted (a serial
      with spaces or dashes) passes as a generalization.
  (c) A SERIAL THE USER TYPES in a format the Privacy Filter misses (a Lenovo "S/N PF2ABC3D") is caught
      only when that device is seeded in the pass.
  (d) Device values are scanned only in a pass that seeds the device: a value an earlier pass left in a
      session variable or artifact is missed in a pass that does not seed it again.
  (e) THE PIN IS THE PASS'S OWN SEED (fail closed): a pass whose own writer (entity_fact_put) changes a
      seeded device fact before it parks cannot resume. Hashing the store at checkpoint time would not fix
      it (completed steps saw the old values); the fix is keeping the seeded values in the checkpoint, a
      later design for the Tech Support build (items.id=915, 916).
  (f) decisions.id=513's flags have no setter (decisions.id=945): redaction and suppression are tested with
      entities built in memory and do nothing in R1.
  (g) THE MODAL DOES NOT GROUP spans that share a fact_key: every occurrence (the serial in the device text
      and in the user's note) is its own row with its own answer and its own "remember" box.
  (h) The RMA use case is a fixture (an ordinary tier-2 step declaring subject_context). The Writing
      Assistant cannot read Tech Support's devices: decisions.id=945 ruling 1 keeps owned entities out of
      every cross-Focus loader, and narrowing it is a separate decision. Tech Support is not built.
  (i) No shipped field is never; a never field is tested on a fixture catalogue.
  (j) Not verified: the live app, an end-to-end hosted call (the loopback client is not HTTPS; hosted model
      blocks stay off, items.id=913), Windows and macOS.
