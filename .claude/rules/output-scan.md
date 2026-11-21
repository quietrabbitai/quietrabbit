---
paths:
  - "src-tauri/src/conductor/privacy/**"
  - "src-tauri/src/conductor/step_review.rs"
  - "src-tauri/src/commands/step_review.rs"
  - "src-tauri/src/persistence/step_review_store.rs"
  - "src-tauri/src/persistence/domain_context_store.rs"
  - "src-tauri/src/persistence/plan_state_store.rs"
  - "src-tauri/src/conductor/declared_inputs.rs"
  - "src-tauri/src/conductor/executor.rs"
---

## Persona context, the output scan and external-step scoring (items.id=922, decisions.id=947, 952, 953)
Two reads that keyed off the run ceiling, and three gaps found with them, built in five stages.
- DIRECTION CHECK (live source, neither leaks). (1) persona_context: the tier only picks visibility
  scopes (domain_context_store::get_eligible_scopes, plan_state_store::eligible_scopes_for_tier) and a
  HIGHER tier gets FEWER scopes, so the ceiling slice is a subset of a tier-1 slice; its only reader is
  the local render (the external one inserts an empty string, items.id=882). The tier was left as the
  ceiling on purpose (a tier-1 slice would widen what a local step sees, ruled out). (2) scan_output:
  execution_tier reaches only the disclosure row; the block decision is the Privacy Filter result, or
  without it the severity (output_scan.rs legacy path), never the tier. A test pins that.
- TOKEN SHAPE AT LOAD (declared_inputs::lint_with, OutputVarNotTokenShaped, both loaders). Every
  recorded variable (ordinary output_var, block handler, render row, store writer's published variable) must
  be [a-z_][a-z0-9_]*. Before this the renderer resolved `{Draft}` (substitute_once looks up ANY
  brace-free key) while find_tokens, which the declared-inputs rule and both taint closures read, did not
  see it: an external step could take a local step's output undeclared. Shipped Focuses had no hits.
  NOT SHAPE-CHECKED: the field_requirements names of a LOCAL step (an external step's are checked against
  the catalogue by items.id=931); a local name with other characters is not looked up by find_tokens.
- PERSONA CARRY REFUSED AT LOAD (declared_inputs::lint_persona_carry, PersonaContextCarriedToExternalStep).
  An external step may not name or declare a variable derived from a LOCAL step that names
  {persona_context}, nor read {previous_output} after such a row: taint_closure_of over ["persona_context"],
  transitive, with the {previous_output} over-approximation (it can refuse a clean Focus, never admit a
  carrying one). Refused, not tainted: persona_context is a system token, not a bound name, and tainting
  would restrict it and drop steps from extraction. A local step may read such a variable. NO runtime
  backstop for this carry. items.id=903's record names no step that carries it.
- STORE-BACKED FLOOR ON EXTERNAL STEPS (executor.rs recorded_step, apply_store_backed_floor). A step that
  went to a hosted destination (`treatment.hosted`) and DECLARES a store-backed input (a bound name, a
  variable a tainted step produced, or user_input in a Focus that ingests a pasted reply:
  StoreTokens::is_store_backed, pinned equal to handoff_render::store_backed_inputs) records severity at
  least 2 and personal_flagged true. A floor only: it never lowers a severity and never sets
  audit_unavailable; a local step is unchanged; undeclared names do not count. StoreTokens carries
  user_input_backed beside restricted, both from seeding::store_state, set by every pass in execute(), so a
  resumed run scores the same as a fresh one (a real rehydrate_focus_run is tested).
  `personal_flagged` ON A FLOORED STEP MEANS "STORE-BACKED INPUT", NOT "GATE 2 FOUND A VALUE". Its only
  consumer is the status payload field, which nothing renders yet; a screen must not read it as a Gate 2 hit.
  An omitted bridged field (items.id=931 default) is not projected, so a hosted step that sent nothing still
  records 1.
- KNOWN GAP, SEVERITY LOST THROUGH A LOCAL STEP (characterized, not fixed). Gate 3 reads the record of the
  step it JUST completed, never output_sensitivity() or the track's sensitivity_ceiling (a scan pins that
  executor.rs never names the ceiling); output_sensitivity() is read only at output creation (the legacy
  output scan and the output's label). For A (local, severity 3) -> C (local, reads A's variable) -> B
  (external): no Gate 3 at A (next tier 1 is not above 1); at C Gate 3 runs on C's reply with C's own
  severity, 1, because a step's severity comes from the PersonalTrack fields its template names, not from the
  variables it consumed, so the legacy gate approves what it would block at severity 3; the outbound review
  at B screens the prompt with a placeholder severity (REVIEW_SEVERITY 1, not authoritative), so a prompt with
  no Privacy Filter span passes. The path is not ungated, but the severity-3 rule never sees A's label.
  Since items.id=933 a bridged personal VALUE that A or C carries into B's prompt is caught by B's carry
  scan (raised in the review, or removed when not_permitted), within its text-match limits; the severity
  label is still lost.
  PRODUCER-SEVERITY INHERITANCE (rule 1 of handoff_render::inherit_from_inputs, for external model steps) is
  DEFERRED: an inherited 3 or 4 would block at Gate 3 (legacy) or force High review (Privacy Filter), label
  the output medical or financial and let the legacy output scan block export, so it needs its own decision.
- SCAN TIER. output() passes lifecycle::highest_tier_used (the highest routing_tier_used over the track, 1
  when no step ran, clamped to 1..3) as scan_output's execution_tier; it was the Focus ceiling.
- PERSONA SLICE ONLY WHEN NAMED. assemble_persona_context (both call sites: run start and
  rehydrate_focus_run) returns an empty string unless steps_name_persona_context finds {persona_context} in a
  step's own template; no shipped Focus names it, so no shipped run reads the persona's memory stores for it.
  domain_context_store::test_set_standing_summary is test-only.
- UNCHANGED, stated: a one-time floor-consent "proceed" is still not read on resume, and a "use local"
  answer still cannot re-route a resolved step (items.id=921's notes). Not verified: any hosted call end to
  end (blocks stay off, items.id=913; the floor is tested through the pure function), the rehydrate call
  site of the persona guard under a real resume, user-installed Focuses against the new LOAD refusals, and
  Windows or macOS.

## Long-prompt review and the Gate 3 riders (items.id=923, privacy review 2026-11-20 F1 to F3, F13, F14)
The Privacy Filter review of long text, and five Gate 3 and outbound-review defects. Units 1 to 3
are code; the spec text is Chat-PM's (PRIVACY_GUARDIAN_GATE_SPEC.md disagreed with itself on
keep-private before this item: line 148 "run stops at this step", lines 421, 487 and 551 "run
stops if all content kept private").
- LONG PROMPTS (privacy/mod.rs, privacy/privacy_filter.rs). privacy::MAX_REVIEW_BYTES = 128 KiB,
  in BYTES of the text. The filter's time follows characters, so non-ASCII text reaches the cap
  in fewer characters. Text over it is refused BEFORE any chunk is scanned: the outbound review
  parks with step_review.too_long (Park::TooLong, a blocked step_reviews row with reason
  prompt_too_long, nothing sent) and gate3 itself blocks with gate3_pf_too_long for every other
  caller. Below it, classify_chunked runs CHUNK_TARGET_BYTES = 16 KiB chunks with
  CHUNK_OVERLAP_BYTES = 1 KiB, cut after whitespace, ONE AFTER ANOTHER (the filter holds one
  mutex), each under PF_TIMEOUT_SECS; spans come back in whole-text offsets (span_text re-cut from
  the whole text) and overlapping spans from different chunks merge (union extent, lowest score,
  the label of the member farther from a cut). Text of one chunk returns the classifier output
  unchanged. The FIRST chunk that times out, panics or fails ends the review through gate3's
  existing arms; later chunks are not scanned and nothing partial is ever sent. Measured with the
  real filter: 16 KiB 1.4 s, 20 KiB 2.5 s, 24 KiB 2.8 s, 32 KiB 4.3 s, 48 KiB 7.2 s; 128 KiB in 9
  chunks 11.9 s (432 of 432 seeded emails found, none twice). The user waits about 12 to 20 s for
  a prompt near the cap, per review. Not changed: output_scan.rs still makes one call under the
  timeout (a follow-up can reuse classify_chunked), and PF_TIMEOUT_SECS.
- F13, REVIEW REUSE (step_review::ReviewReuse). A FINAL step_reviews row is reused only when THIS
  retry loop produced or applied it: the set is owned by RealAttempt, built once per execute(),
  passed to every attempt and never stored. A resume after a park is a new loop, so the step is
  reviewed again and a standing decision tightened while the run was parked applies. A PENDING
  review the user answered is still applied on resume (it keeps the settled spans recorded at
  scan time).
- F3, WHAT A GATE 3 RESULT CHANGES (comments corrected, no behaviour change). In Step 13 only
  `blocked` and `pending_consent` act. An approval, the user's span decisions and auto_resolved
  change nothing the next step receives; SharedStateTrack's "promotion" is an audit record in the
  checkpoint with no production reader. A hosted next step is reviewed again by the outbound
  review; a cb-15 starter has only the copy-time review. The run-level gate3 consent row is
  written and never read.
- F14, KEPT PRIVATE (gate3::KeptPrivate). gate3 takes StopsContent or RemovesSpans. ONLY the
  outbound review (Gate3Scanner) and API Chat (review_new_turn) pass RemovesSpans: a REMEMBERED
  keep_private on every span is then sent with the spans removed, as a live answer already was.
  Step 13 and the copy review pass StopsContent and keep "stops if all kept private". A scan pins
  both sets. The copy review is therefore still asymmetric: a live keep_private on every span
  copies with [removed], a remembered one refuses the copy (outside the ruling, left alone).
- F1, FORCED-HIGH PROVENANCE (gate3::partition_with_rule, output_store::find_review_answer_for_fact).
  forced_high = !zero_spans_safe_to_auto_approve(severity, destination risk, authoritative), the same
  rule as the zero-span check, decided before the partition. Under it a STANDING PREFERENCE NEVER
  SETTLES A SPAN and never overrides the user's answer. A span is settled only by an element_consent
  row that (1) the user wrote (save_preference NOT NULL and original_text NULL; the auto-reapplied
  writer sets the reverse, and a scan keeps the two writers distinguishable), (2) is later than this
  step's latest PARKING checkpoint (focus_run_snapshots with purge_after set; a periodic checkpoint
  does not count) and (3) is not an answer to the step's own outbound review (its asked span ids in
  step_reviews). Everything else is asked again. The outbound review, API Chat and the copy review
  pass a placeholder severity with severity_authoritative false, so they are never forced High and
  their partition is unchanged.
- F2, A FORCED-HIGH REVIEW WITH NO SPAN (executor.rs step13_verdict, the one reader of a Gate3Result
  in Step 13). pending_consent with empty review_spans is Unanswerable: the step stops with
  promotion.sensitive_blocked (AwaitUser, F4, Stop, recoverable; spend::codes promotion.* prefix, the
  FIFTH run_park family; en.json promotion.stop.sensitive_blocked placeholder copy, orphan-allowlisted,
  Chat-BRAND's wording). gate3 still writes gate3_pf_no_spans_forced_review and returns
  pending_consent, but EMITS NO consent_request for it (emits_consent_request: only a payload with a
  span is surfaced). Only Step 13 reaches this case (see F1's last sentence).
  THIS DEPARTS FROM decisions.id=405 Q1 (the gate fires when the filter finds nothing and the user
  decides): here the user does not decide, the step is blocked. It is FAIL CLOSED (nothing leaves),
  and NO SHIPPED FOCUS REACHES IT (none has a severity-3 step ahead of an external step). Restoring
  the user's choice would mean reading the run-level gate3 consent row after the park, which nothing
  reads today (F3).
- ACCEPTED LIMITS:
  (a) No parking checkpoint, as when checkpointing is suspended after a snapshot write failure (F8),
      means a forced-High answer NEVER counts: the span is asked again on every resume.
  (b) A span with no fact identity (no fact_key) is never settled under forced-High and is asked
      on every resume (the decisions.id=888 point 5 limit, now without the standing escape).
  (c) A manual resume after promotion.sensitive_blocked re-runs the step once (a new model call)
      and stops again: no loop, no way through.
  (d) An answer written in the instant before its parking checkpoint is written is older than the
      park and is asked again (errs toward asking).
  (e) Not verified: a full Step 13 resume against the real filter (needs the running app; no
      shipped Focus reaches it), Windows and macOS.
