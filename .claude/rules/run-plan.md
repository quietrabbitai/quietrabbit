---
paths:
  - "src-tauri/src/conductor/run_plan*.rs"
  - "src-tauri/src/conductor/plan_*.rs"
  - "src-tauri/src/conductor/run_park*.rs"
  - "src-tauri/src/commands/run_plan*.rs"
  - "src-tauri/src/commands/turn_marker.rs"
  - "src-tauri/src/persistence/plan_approval_store.rs"
  - "src-tauri/src/persistence/run_park_store.rs"
  - "src-tauri/src/spend/**"
  - "src-tauri/schema/outputs_017.sql"
---

## Run plan and strategy approval (items.id=901, decisions.id=942 Q2 and Q3)
Before a run that has model-block steps starts, the user can see what it could do and approve
what goes beyond the resolver's local picks. Built: the plan, the approval stores and the two
homes, enforcement at selection and at the spend gate, the park reason, five IPC commands. NOT
built: any screen (items.id=902 or 904).
- SCOPE. Only steps that carry a `requirement` (expanded from a model block, kind model_block).
  A Focus with none has NO plan: run_plan::has_planned_step is false and nothing is read or
  written for it. Ordinary external steps and API Chat keep today's spend gate and outbound review
  and are NOT plan-gated (see the pending pointers). A bound API Chat run skips the gate.
- THE PLAN (conductor/run_plan.rs, computed, never stored). Reachable paths are the LOAD guard
  enumeration (MAX_PATHS and the analysis untouched) filtered by the run's launch values; a guard
  on any other name keeps both branches. Per step: the resolver's answer, and the OFFERS (one
  resolver run per candidate: an unmeasured local model or a hosted model is offered, never picked
  by the resolver; only an explicit pin selects it). Strategies local, paid_api and manual_handoff
  each carry a reason id (manual hand-off is listed only when every reachable path is a cb-15
  starter with no model call). Cost is the spend gate's own worst case (spend::estimate::
  worst_case), the dearest reachable path; PlanIncluded is never produced. More than MAX_PATHS is
  plan_too_many_paths and no plan is offered. The module names no model or provider id and holds
  ids, closed words and numbers only (scans pin both).
- THE DIGEST (tag rp1, SHA-256) is STRUCTURE ONLY: the Focus, the document profile, per reachable
  planned step its destinations, highest tier, quality range and hosted_possible, whether manual
  hand-off is listed, and any pins. Not prices, readiness, installed models, the resolver's answer
  or any text. get_run_plan shows the PIN-FREE digest (`RunPlan.digest`); an approval stores
  `digest_with(&pins)`. approve() compares the digest the user was shown with the plan the server
  just recomputed (mismatch: plan_changed, nothing approved); verify() at run time recomputes
  `digest_with(stored pins)` from the run's own plan. A digest the CLIENT sends is never used by a
  run. A narrower mode changes the digest and re-asks (over-asking, never under-asking).
- THE APPROVAL (conductor/plan_approval.rs): the digest, a per-run ceiling in micro-USD (at least
  the plan's estimate under the pins, at most PLAN_CEILING_MAX_MICROS = 25,000,000) and the pins
  (only an unmeasured local model or a paid model, one per step, each one of the plan's own
  selectable offers). A launch answer, mode or run_change never counts as an approval.
- TABLES (schema/outputs_017.sql, the persona's encrypted outputs.db, no foreign keys or
  triggers, no text columns; items.id=633 must absorb it with the other outputs files, and
  tests/golden/schema_shape.txt already carries it): plan_approvals (PK context_key + focus_id:
  digest, ceiling_micros, choices_json of the pins, version) and run_parks (run_id: action_id, a
  registered code, params_json at most 4096 bytes). Stores: persistence/plan_approval_store.rs
  (put_approval is the only writer, latch checked first inside one BEGIN IMMEDIATE; delete is NOT
  latch-gated) and persistence/run_park_store.rs.
- TWO HOMES OF AN APPROVAL (conductor::plan_approval::save_chat_approval is the ONLY caller of
  put_approval). An unlatched chat: persisted. A latched chat, a latch that cannot be read, or a
  database that cannot be opened: NOTHING is written and the approval is held in memory
  (PlanMemory). A run with no chat: only the run-scoped offer in memory, by an opaque 128-bit
  token, single use, bound to user, Persona and Focus, consumed with the run (a token is spent by
  the run it was presented to, even if verification then fails). Reads prefer memory (it can only
  be newer). Revocation removes BOTH homes and is not latch-gated. An approval persisted before a
  latch still reads.
- MEMORY HOME (conductor/plan_memory.rs, managed state, never serialized, logged or shown in
  Debug): approvals 64 entries and 2 hours (absolute from approval, not sliding), chatless offers
  16 and 10 minutes, duplicate-turn markers 64 and 120 seconds (a hash only), run parks 64 and 2
  hours; the oldest are evicted on overflow and expired entries are dropped on every access.
  CLEARED: at logout or lock (main.rs, KeyRegistry on_key_leave -> clear_user, everything of the
  user), when a chat is archived (archive_chat -> PlanMemory::clear_chat: that chat's approvals and
  marker; a park or an offer has no chat key and goes by TTL, logout or a later row write) and by
  TTL. The PERSISTED approvals go with the archive hook and the 30-day store sweep (items.id=888).
- plan.exceeded (spend::codes::Code::PlanExceeded; the IPC string for a start that needs an
  approval is the literal `plan_required`, a different thing). Parks the step (F10, AwaitUser)
  with nothing sent. In the executor, at selection and before derive_treatment and every gate:
  `no_approval` (a hosted pick and the run holds no approval: a rehydrated or swept run, or a run
  that needed none), `model_not_planned` (an approval that does not name this model for this step:
  a zero-pin approval permits resolver LOCAL picks only) and `pin_unavailable` (the pinned model is
  not a ready candidate of its kind any more, or measured below the bar; no silent fallback). At
  the spend gate, check 5b (SpendCall.plan_cap_micros, Some only for a resolved step and zero when
  it has no approval, after every existing check and before the reserve, inside the same
  transaction): the run's ledger total (held plus settled) plus this call's worst case over the
  ceiling is `over_ceiling`, a cap with no run id is `no_run_id`. It can only REFUSE.
- PLAN GATE (conductor/plan_gate.rs). load_and_authorize_run runs load(), authorize_checks(),
  then plan_gate(), then open_record(), so a run that needs an approval it lacks writes no
  focus_runs row (authorize() is exactly authorize_checks() then open_record(), pinned). It
  returns LoadOutcome::PlanRequired and every caller maps it to Err("plan_required"). A plan that
  needs no approval proceeds with none (only local resolver picks can then run).
- PARK REASON (conductor/run_park.rs, items.id=901 stage 7). A run that parks leaves its reason in
  run_parks, written by lifecycle::write_run_row in the same function and on the same connection
  as the focus_runs row: every status write stores the park it is given or removes any earlier
  one, so a park exists exactly when the last status written for the run carried one. Which
  failures: a code in Code::ALL of the resolver, spend_block, plan, step_review or promotion family
  (by Code variant; promotion since items.id=923). What it carries: sanitize_params keeps keys of [a-z0-9_] and values that are numbers,
  bools, null or identifier-shaped words (no spaces), at most three deep, and DROPS anything else,
  so prose cannot ride in a park; a TooWeak or NoCandidate park carries the resolver's ranked
  options, and when they would exceed 4096 bytes whole options are dropped from the worst-ranked
  end (options_total and options_omitted say how many) so the JSON is always complete. If the row
  or the park cannot be stored it is held in memory instead. A failed park write or clear never
  changes the failure or fails a cancel or status write. Cleared by any later status write,
  cancel_focus_run and set_focus_run_status (best effort). get_run_park's `params_json` is clean
  BY CONSTRUCTION at write time (sanitize_params); the DTO shape test in commands/
  run_plan_commands_tests.rs checks a fixture park and does not prove real park content.
- WHO READS AND WRITES WHAT (a new reader fails the allow-list scan
  the_plan_the_approval_and_the_memory_home_are_read_only_where_reviewed in
  conductor/plan_gate_tests.rs and needs review):
  - the plan gate (plan_gate.rs) builds the plan (the one run_plan::build call, shared with
    plan_offer), reads a chat's approval or takes an offer, and sets `FocusRun.plan`;
  - the executor reads ctx.plan at exactly three sites: plan_pin, check_plan_envelope and
    plan_cap_micros (executor tests pin it);
  - lifecycle reads the approval for the Gate 3 look-ahead (has_paid_pin, one predicate, with a
    coupling test against the executor's own rule);
  - send_message (commands/messages.rs) runs a plan preflight BEFORE the user's turn is saved,
    through execution::plan_probe_run, the one constructor of the probe run that get_run_plan uses
    too, so the digest a screen is shown is the digest the run recomputes; commands/turn_marker.rs
    sets and takes the duplicate-turn marker; archive_chat and main.rs clear;
  - IPC Group 39 of HANDOFF_IPC_SURFACE.md (Group 36 in ipc.rs's own list), commands/run_plan.rs:
    get_run_plan(persona_id, context_key?, focus_id) (read only; with no chat it registers the
    offer and returns the plan_token), approve_run_plan(request {persona_id, context_key?,
    plan_token?, focus_id, digest, ceiling_micros, choices}) (exactly one of context_key and
    plan_token), get_run_plan_approval(persona_id, context_key, focus_id) (read only),
    revoke_run_plan_approval(persona_id, context_key?, plan_token?, focus_id) and
    get_run_park(persona_id, run_id) (memory first, then run_parks). Micro-USD travel as decimal
    strings. Each takes writer_gate::command first and the user from the session (no user_id
    argument); the Persona id is a canonical UUID owned by the user (persona_not_found for malformed
    and foreign alike). Codes (ERROR_CODES, each with a placeholder plan.error.<code> key in en.json
    held in the orphan allowlist of styleAndI18nConsistency.test.ts until items.id=902 or 904
    shows them): persona_not_found, invalid_focus_id, invalid_context_key, invalid_plan_token,
    invalid_run_id, no_plan, plan_changed, invalid_choice, invalid_ceiling, plan_too_many_paths,
    plan_unavailable. The wording is placeholder, Chat-BRAND's. A step's label is not in the plan:
    the screen maps an action id to the Focus's own data.
- KNOWN BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. The preflight of a chat send costs ONE indexed shared.db read (focus_has_model_block) for a
     Focus with no model block, and nothing else: no Focus loaded, no persona database opened.
  2. An in-run `plan_required` can still occur AFTER the user's turn was saved (the plan changed
     between the preflight and the load). The memory marker (persona, chat, saved message id, a
     hash of the content, 120 s) narrows the duplicate-turn gap: the next send skips the save
     only if the hash matches, the marked message is still the newest and a user turn. The marker
     is never persisted and is lost at an app restart, when the turn saves once more.
  3. Since items.id=888 a rehydrated or swept run of a chat reads and verifies its approval through
     plan_gate (the app's memory home when there is an app handle, else the persisted row only). A run
     whose approval is missing, unreadable or no longer verifies still stops a hosted pick with
     plan.exceeded `no_approval`, and so does a resumed run with no chat.
  4. The memory TTLs use a monotonic clock that may not advance while the machine is suspended, so
     an entry can outlive its nominal time across a suspend. Bounded by logout and the entry caps.
  5. The ceiling bounds the WHOLE run ledger (every metered step of the run, ordinary steps'
     spend included), not only planned steps.
  6. A restart re-asks every approval held in memory (a latched chat's, a chat whose latch or
     database could not be read, a chatless run's); a persisted approval of an unlatched chat
     survives it.
  7. Persisted approvals are deleted by the archive hook and after STORE_RETENTION_DAYS unwritten
     (items.id=888's store sweep).
  8. PlanIncluded is never produced: a hosted model is Free or Metered. (The open design question
     of recording which plan a user holds is not built.)
  9. EXTERNAL_MODEL_BLOCKS_ALLOWED is false, so no loadable Focus has a hosted range and the hosted
     enforcement runs ONLY in fixtures: the executor pin tests (hosted_pick_ctx with an approved
     pin), the spend gate's call_capped tests and the plan gate tests against the loopback mock
     listener at zero connections. Those are the live test items.id=913 must keep green when it
     turns hosted blocks on. The real hosted client is HTTPS only.
  10. An approval and an offer never widen a gate: the executor, the spend gate and the outbound
      review still run for every approved step.
- PENDING POINTERS:
  - items.id=888: BUILT (see "Resume state, archive hook and store sweeps").
  - items.id=902 or 904 (the screens): show the plan, the approval and the park reason from the
    five commands, and the plan.error keys and the plan.stop.exceeded key (orphan-allowlisted
    today). items.id=906: named for this pass; its scope is not recorded in this repo and nothing
    here depends on it.
  - items.id=913 (hosted blocks on): see known behaviour 9; its dependency on 888 (context_key on
    rehydrate) is built.
  - ORDINARY EXTERNAL STEPS ARE NOT PLAN-GATED: Q3's "always" is open for them. A new item,
    opened by Chat-PM (number not named here), adds a second step kind to the plan list (the
    envelope: kind, action id, the provider the user prefers, the ceiling tier), the same
    ceiling accounting and the same executor check keyed by (action id, kind). A bump of the
    digest tag re-asks and never allows.
  - items.id=633: must absorb outputs_017.sql with the other outputs files.
