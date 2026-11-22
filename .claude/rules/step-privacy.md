---
paths:
  - "src-tauri/src/conductor/step_treatment.rs"
  - "src-tauri/src/conductor/executor.rs"
  - "src-tauri/src/conductor/lifecycle.rs"
  - "src-tauri/src/conductor/types.rs"
  - "src-tauri/src/conductor/tokens.rs"
---

## Privacy treatment of a step attempt (items.id=921, decisions.id=942 addendum 5 (e), decisions.id=943 as amended)
A model-block step is gated by where it GOES, not by what it may reach. conductor/step_treatment.rs
is the one pure function (`derive`, no I/O); execute_once calls it once per attempt, right after
selection and before Gate 1, through `derive_treatment(ctx, resolver_req, &selected_model)`.
- INPUT: the ceiling values lifecycle computed into StepContext (effective_access, execution_tier,
  abstraction_tier with the floor clamp, raw_abstraction) and the basis: Ordinary (no requirement,
  or a bound API Chat run) or Resolved (the requirement's highest tier and whether the pick is
  local or hosted).
- RULE: Ordinary passes the ceiling values through UNCHANGED (hosted = access != LocalOnly). A
  local pick under any ceiling is the LOCAL treatment: LocalOnly, tier 1, raw and abstraction 1,
  nothing clamped. A hosted pick is EXTERNAL at the reach, min(ceiling, requirement highest
  tier), with each tier the max of the derived and the ceiling value (never looser than an
  ordinary external step). Disagreements are refused, never skipped: a hosted pick under a
  local-only ceiling (the wording items.id=896 shipped), a hosted pick from a range that reaches
  only the device, a selection that is half local and half hosted, a broken floor invariant. The
  refusal goes through the failure handler as a TierBoundaryViolation before any gate.
- WHAT READS THE TREATMENT: Gate 1 (its arguments, and so its fatal-versus-swallowed disclosure
  log split), the floor consent block (the prompt, the floor_consent_auto row, the prompt
  metadata), scan_voice_profile (a local pick strips a contaminated attribute and continues, a
  hosted call halts), the render branch, hosted_step, reply_gate2, compute_step_sensitivity,
  completed_step (routing_tier_used) and Gate 3. `treatment.hosted` is the ONE predicate for the
  network branch, the render and, for items.id=884, the outbound review: 884 triggers on
  `treatment.hosted` and takes its ceiling argument from `treatment.access`, not
  ctx.effective_access. The disagreement 884's amendment names is already an error before its hook.
- THE DOCUMENT'S CEILING (items.id=903): before the ceiling values are computed, resolve_tier_config
  replaces the user's focus_settings tier with document_session_store::effective_ceiling for a run of
  a chat whose Focus selected a document type: a medical, financial or legal document flag makes it
  local_only, and a one-document raise of the same document type lifts it to the raised tier (see
  "Writing Assistant"). The Focus's own ceiling still clamps it, and derive is unchanged: a hosted pick
  under a raised ceiling is external at min(ceiling, requirement tier), with none under local_only.
- WHAT STILL READS THE CEILING (pinned by the_ceiling_values_are_read_only_where_reviewed): the
  bound-run stop, Step 4.5 and select_model (before selection), the resolver's candidate set, the
  floor-invariant check on the ceiling values, derive_treatment's own input, step_streams.
  A new read of ctx.effective_access, execution_tier, abstraction_tier or raw_abstraction fails
  that test and needs a decision.
- DISCLOSURE LOG (open question a): a local pick writes its gate1_pass row like any local step
  (tier 1, no provider, fields shared, non-fatal on failure), so every executed step keeps a row
  and the row says nothing left the device. The log still records no model tag.
- FLOOR CONSENT (open question b): the prompt exists because the clamp changed what leaves. A local
  pick has no clamp, so it never prompts and the saved "modified" preference is not consulted.
- TIER OF A HOSTED PICK (open question c): the lower of the ceiling and the requirement's highest
  tier. In production this equals the step's own override (model-block expansion sets
  routing_tier and external_access_override to the clamped highest tier), so the choice only fixes
  the pure function's contract.
- NOT STORED: the treatment is a local of one execute_once attempt (a scan pins that nothing in
  lifecycle.rs, types.rs, tokens.rs or StepContext holds it), so each retry and each resumed run
  selects and derives again; the disclosure buffer is overwritten each attempt. Characterized and
  pinned: a resumed hosted pick parks on floor consent exactly like an ordinary external step, and
  proceeds exactly like one when the saved preference is present.
- GATE 3: a local pick records tier 1, so Gate 3 reviews its output when the next step may go out
  (next_execution_tier > 1). next_execution_tier is the static ceiling look-ahead and cannot know
  the next step's own pick, so Gate 3 may also fire when that step resolves local. Refined by
  items.id=901 (see "Run plan and strategy approval"): under a verified approval a following
  model-block step with no PAID pin counts as tier 1, because the executor then stops any hosted
  pick of it. With no approval or on a bound API Chat run it is still the ceiling look-ahead
  (over-prompt, never under-prompt). A rehydrated run of a chat holds the approval its own
  plan_gate verified on rehydrate (items.id=888 stage 4) and uses it like a fresh run.
- STREAMING: a resolved step never streams (step_streams has `!is_resolved(ctx)`, because
  retry_loop asks it without a selection). R1 streams only bound API Chat runs, which are never
  resolved. Streaming a resolved step needs a decision.
- THREE BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. A one-time floor-consent "proceed" is written to consent_decisions but nothing reads it on
     resume: only the saved preference in personal.db is read (lifecycle.rs, "modified" when
     stored_tier <= abstraction_tier). A resumed external step, resolved or ordinary, therefore
     parks on floor consent again until the user saves the preference. Existing behaviour,
     left alone; the hosted pick matches it.
  2. Two run-level reads did key off the Focus ceiling, not the pick (items.id=922 re-checked
     both: neither leaks). scan_output at output creation now logs the highest tier a step used.
     The persona_context memory slice is still assembled at the Focus's max tier (a LOWER tier
     gets MORE scopes, so the ceiling slice is strict, and its only reader is the local render),
     but only when a step names {persona_context}. See "Persona context, the output scan and
     external-step scoring".
  3. A floor-consent "use local" answer cannot re-route a resolved step: the resolver never reads
     that preference. A hosted pick that parks on floor consent stays a hosted pick on resume
     unless readiness changes.
