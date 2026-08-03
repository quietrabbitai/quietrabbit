# Privacy Filter Confidence Threshold Calibration — Results

Date: 2026-08-03
Scope: items.id=36 remaining item ("run the confidence threshold testing... record/review
results"). Full architecture: decisions.id=405 (Q2, tier routing). Gate screen spec:
PRIVACY_GUARDIAN_GATE_SPEC.md.

This is an empirical calibration run of the **live** Privacy Filter model
(privacy-filter.cpp FFI, real GGUF weights) against a hand-built corpus of
representative QR field content. It is separate from `golden_vectors.rs`,
which exercises gate3's fallback path only (`app_handle: None`) and never
invokes the live model.

---

## Method

- Harness: [`src-tauri/examples/privacy_filter_calibration.rs`](../../examples/privacy_filter_calibration.rs)
- Model: `~/.local/share/quietrabbit/models/privacy-filter-q8.gguf`
- Library: `/home/kulaga/privacy-filter.cpp/build/release-portable` (built per build.rs instructions)
- Threshold param passed to `pf_classify`: `0.0` (all spans returned uncut — mirrors
  how gate3.rs calls it; gate3's own thresholds are applied afterward, not by the model call)
- Thresholds evaluated (copied from `gate3.rs`, current values as of this run):
  - `EASY_SCORE_THRESHOLD = 0.90`
  - `MEDIUM_SCORE_THRESHOLD = 0.70`
  - `EASY_TIER_CATEGORIES = ["private_email", "private_phone", "account_number"]`
- Corpus: 29 hand-written cases across 8 groups — structural PII expected-Easy
  (email/phone/account), structural PII expected-non-Easy (person/address),
  ambiguous names (common-word name traps: "Rose", "Bill", "Grace", nickname-only),
  contextual content outside the PF base taxonomy (financial, medical, dietary/health,
  personal history), secret, URL/date, and negative controls (no PII).
- Full raw run output (table + summary produced directly by the harness):
  [`pf_calibration_raw_20260803.md`](./pf_calibration_raw_20260803.md)

### Infrastructure note (not part of the threshold question, but required to get a run at all)

`pf_classify` initially failed with `CPU backend init failed`. Root cause: ggml's
runtime backend loader (`ggml_backend_load_all`) searches, in order, a compile-time
`GGML_BACKEND_DIR`, the **calling executable's own directory**, then the current
working directory — none of which contained the CPU-arch-dispatched backend `.so`
files (`bin/libggml-cpu-*.so`) when the harness binary ran from
`target/release/examples/`. `GGML_BACKEND_PATH` does not help — it loads one named
file, not a directory. Fix used for this run: set the process's current working
directory to `privacy-filter.cpp/build/release-portable/bin/` before invoking the
harness binary (confirmed via log: `load_backend: loaded CPU backend from
.../bin/libggml-cpu-zen4.so`). **This same gap will affect the Tauri app in
production** — nothing in `src-tauri/src` or `build.rs` currently arranges for the
app's cwd or binary directory to contain those backend `.so` files. Flagged
separately below; out of scope for this testing pass.

---

## Findings

### 1. Easy-tier categories (email, phone) — threshold is well-calibrated

Both `private_email` and `private_phone` spans scored **1.000** confidence across
all variants tested (plain address, plus-addressed email; dashed and international
phone formats). Comfortably clears `EASY_SCORE_THRESHOLD = 0.90`. No change needed.

`account_number` also scored 1.000 in both cases tested, but both test cases carried
`content_sensitivity_severity = 3` (bank/routing numbers, correctly classified as
financial), which forces High tier regardless of PF confidence — so this run did not
actually exercise the Easy-tier path for `account_number` (a non-financial reference/
account number at severity < 3 would be needed to observe that; not included in this
corpus).

### 2. `private_person` / `private_address` — confirmed high confidence, confirmed deliberate Medium routing

Both categories scored high (person: 0.998–1.000; address: 0.850–0.992) — the model
*is* confident on these. `gate3.rs` deliberately excludes them from
`EASY_TIER_CATEGORIES`, so they route to Medium regardless of score. Calibration
confirms this is a **policy choice**, not a scoring accuracy gap: the model would
support Easy-tier routing on confidence alone, QR adds friction anyway. No action
needed — noting this explicitly in case a future session revisits the category list
and wants to know whether the exclusion was ever confidence-justified.

### 3. Ambiguous / common-word names — mixed

"Rose" (as in "my sister Rose"), "Bill" ("Bill called me"), and "Grace" ("my daughter
Grace") — the classic NER traps where a name doubles as a common word — were **all
correctly detected** as `private_person`, scores 0.998–1.000. No false negatives here.

Two genuine misses (PF returned **zero spans**, not just a low-confidence span):
  - `"Everyone just calls me Kool-Aid at the gym."` — nickname-only self-reference, not detected.
  - `"Our anniversary is June 15th."` — relative/partial date format, not detected.

These are **model recall gaps**, not threshold-tunable: there is no score to raise or
lower a bar against when the model returns nothing at all. Per decisions.id=405 this
is explicitly the failure mode called "credibility-destroying" and "not acceptable."
Recommend expanding the calibration corpus with more nickname/partial-date phrasing
in a follow-up pass, and tracking whether this is worth a decision-level discussion —
not something this testing pass can resolve by adjusting `EASY_SCORE_THRESHOLD` /
`MEDIUM_SCORE_THRESHOLD`.

### 4. Financial / medical / dietary / personal-history context — confirms the known base-taxonomy gap

5 of 6 contextual-content cases (household income, credit card debt, medication
dosage, diet-due-to-diagnosis, divorce) returned **zero spans** — consistent with
decisions.id=405's Q2 finding that these content types are outside the Privacy
Filter's 8-category base taxonomy (fine-tuning for this domain was explicitly
deferred post-R1). One case ("I was diagnosed with Type 2 diabetes back in 2019")
incidentally matched a `private_date` span on "2019" — not the diagnosis itself.

### 5. ⚠️ Most significant finding: zero-span auto-approve bypasses the severity-forced-High rule

`gate3_with_pf` in `gate3.rs` (lines 327–346) auto-approves immediately whenever PF
returns zero entities — **before `assign_review_tier` (and its
`severity >= 3 → High` check) ever runs.** decisions.id=405 states the High tier
"always applies to... Medical content (regardless of Privacy Filter confidence)"
and "any content where Privacy Filter confidence is low." Zero detections is the
extreme case of low confidence, yet the current code path treats it as "nothing to
review" and ships the content.

This run reproduced it concretely: `"My household income is around $85,000 a
year."` and `"I'm currently paying off about $12,000 in credit card debt."` — both
tagged `content_sensitivity_severity = 3` (financial) in this test's ground truth —
returned zero PF spans and, under the current gate3.rs logic, would be **silently
approved and sent to the external destination with no consent modal at all.**

This is a logic gap surfaced by empirical testing, not a threshold value to retune.
Flagging for follow-up rather than fixing here — out of scope for "run the
confidence threshold testing," and `gate3.rs` is production gate logic that
shouldn't change without its own review per the project's approval workflow.

### 6. Minor: SSN categorized as `account_number`, not `secret`

`"My SSN is 123-45-6789."` was detected (not missed) but labeled `account_number`
(score 1.000) rather than `secret`. Behaviorally this mostly doesn't matter in this
run's test setup (both categories can reach Easy tier on confidence, and severity
forces High here regardless), but `taxonomy_label()` in gate3.rs would render this
to the user as "Account number" rather than "Sensitive value." Worth a note for
whoever next touches taxonomy label mapping — not a threshold issue.

---

## Summary table (from harness output)

| Metric | Count |
|---|---|
| Total cases | 29 |
| Cases with expected detection | 20 |
| Negative-control cases | 9 |
| False negatives (expected span, PF found nothing) | 2 — `name-nickname-only`, `date-anniversary` |
| Wrong-category detections | 1 — `secret-ssn` → `account_number` |
| Unexpected detections on non-PII-focused text | 1 — `medical-diagnosis` (incidental date match) |

Full per-case table with raw scores: see
[`pf_calibration_raw_20260803.md`](./pf_calibration_raw_20260803.md).

---

## Conclusion on the two tunable thresholds

- `EASY_SCORE_THRESHOLD = 0.90` — supported by this run. Every Easy-eligible-category
  span (email, phone) that was detected scored 1.000, well clear of the bar. No
  evidence to lower it; no false negatives observed in the range that would justify
  raising it further (raising it wouldn't have changed any outcome in this corpus,
  since nothing landed close to the line).
- `MEDIUM_SCORE_THRESHOLD = 0.70` — supported, with one caveat: the address case
  `"742 Evergreen Terrace, Springfield."` scored 0.850, meaningfully closer to this
  line than the other structural cases (0.99–1.00). Not a failure — it's still
  comfortably above 0.70 — but it's the lowest-margin data point in the corpus and
  worth re-testing with more address phrasing variety before treating 0.70 as fully
  proven for `private_address`.
- Neither threshold value is contradicted by this run. The failure modes found
  (§3, §5) are not threshold-shaped — they are recall gaps (zero detections) and a
  control-flow gap (zero-span short-circuit skipping severity check), and retuning
  `EASY_SCORE_THRESHOLD`/`MEDIUM_SCORE_THRESHOLD` would not address either.
