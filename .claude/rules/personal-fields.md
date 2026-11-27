---
paths:
  - "src-tauri/src/conductor/personal_fact_*.rs"
  - "src-tauri/src/conductor/carry_scan.rs"
  - "src-tauri/src/conductor/executor.rs"
  - "src-tauri/src/conductor/lifecycle.rs"
  - "src-tauri/src/persistence/fact_policy_store.rs"
  - "src-tauri/src/commands/fact_policy.rs"
  - "src-tauri/block_data/personal_fact_fields.json"
  - "src-tauri/schema/personal_012.sql"
  - "app/core_artifacts/focuses/**"
  - "src-tauri/focus_specs/**"
---

## Key Architectural Decisions

- PersonalTrack is never serialized: only a manifest of field names and hashes goes into a
  snapshot, and it is re-fetched fresh on resume. This does NOT mean snapshots hold no
  personal values: step outputs and disclosure buffers hold the values approved for each step
  (raw at tier 1, abstracted from tier 2 up), encrypted at rest in the persona's outputs.db
  (decisions.id=865). Parked runs' snapshots are swept (items.id=681).
- Step 6: PG_GATE_1 writes approved/abstracted fields to step_disclosure_buffer
  Step 8: BOTH renders read the step's step_disclosure_buffer (NEVER PersonalTrack): raw at
  abstraction tier 1 for a local step, abstracted from tier 2 up (items.id=933; before it the local
  render did not read the buffer and a declared field rendered empty).
  Above tier 1 a raw personal value reaches an external prompt through the buffer only for a bridged
  field the user set to pass, or to ask (items.id=924), whose every occurrence the outbound review then
  raises. A bridged value
  an earlier step's output carries into a hosted prompt is caught by the carry scan, within its limits
  (items.id=933, "Local render of personal fields and the carry scan").

## Prompt Authoring Rule
render_template() (conductor/executor.rs) substitutes {token} in ONE pass
(substitute_once, items.id=932) and strips unresolved {token} patterns of the
TEMPLATE. Templates must not use {token_name} syntax for any string not intended
as a Conductor token. A substituted value is never re-scanned: a value that
holds {x} text appears literally. A step's output_var must be token-shaped,
[a-z_][a-z0-9_]*: LOAD refuses any other (items.id=922), because the renderer
resolves any brace-free span but find_tokens, which every lint reads, sees only
that shape.

## Personal fact bridge (items.id=931, decisions.id=953; the catalogue and store are items.id=930)
Before items.id=931 PersonalTrack.fields was empty in every real run, so a declared
field_requirements token rendered empty. Now a singleton entity fact in the closed catalogue
(block_data/personal_fact_fields.json: location_city, job_title, employer, industry, all personal,
default OMIT at tiers 2 and 3) reaches Gate 1 at the user's disclosure policy.
- ONE CONSTRUCTOR (conductor/personal_fact_bridge.rs, PURE, no I/O). lifecycle.rs
  bridge_personal_facts is its only caller and the only production add_field on a conductor
  PersonalTrack (executor.rs's gt.add_field is the separate privacy gate track): both are pinned by scans
  in the bridge's tests (T6). Input: facts that PASSED apply_entity_fact_provenance_check, so an
  unconfirmed cross-Persona fact never gets here; a confirmed one is bridged as not_permitted (v1).
  Entity-scoped and uncatalogued facts are skipped, a duplicate name is dropped whole, an unreadable
  catalogue bridges nothing. A bridged field is marked by source_id BRIDGE_SOURCE_ID, nothing else writes it.
- POLICY. abstraction_tier2/3 on the field carry the effective policy word (personal_fact_fields::
  effective_policy over fact_disclosure_policy, personal.db). The bridge NEVER reads the entity_facts
  abstraction columns (EntityFact does not carry them, the loader does not select them, a scan pins both).
  The default is omit, only an explicit user pass releases the raw value and only at tier 2 (tier 3 pass is
  not selectable for any v1 field), medical and financial (and an unknown sensitivity) are not_permitted
  whatever the user says. ask (items.id=924, D7) releases the raw value only to a hosted step that gets the
  outbound review and only for a value of 4 or more characters, and the carry scan raises every occurrence
  with every answer; anywhere else it is omit, and at tier 3 it follows pass's rule.
- WHICH CHOICE APPLIES (items.id=960 unit 0, decisions.id=968 addendum 2). The stored columns are tier2 (the
  anonymous route) and tier3 (the signed-in route), but Gate 1 and the carry scan read a column by the
  ABSTRACTION tier, max(min(Focus privacy tier, execution tier), 2) from tier 2 up. So for a BRIDGED field at
  execution_tier 3 or more (a step that may reach signed-in providers), executor::to_gate_track_asks puts the
  tier-3 word in BOTH columns, before ask is resolved, so both ask lists follow it: the destination's choice
  applies at every privacy tier, and no pass or ask reaches such a step (the resolver, for a field that is
  not tier3_selectable). gate1.rs and carry_scan.rs are unchanged. Consequences: with anonymous Keep back and
  signed-in Describe the label is sent where the default privacy tier used to send nothing; an anonymous
  Share has no effect on such a step, even when the provider picked is anonymous (the tier comes from the
  ceiling); at privacy tier 1 (raw 1, abstraction 2) a declared bridged field with an anonymous Share counts
  as floor clamped there and the step parks until the preference is saved; the gate1 row still records
  abstraction_tier 2 beside execution_tier 3.
- RE-READ. A run uses the INITIALIZE snapshot and execute_once reads fact_disclosure_policy AGAIN before
  Gate 1 for a hosted step that holds a bridged field, taking the STRICTER word of the two (not_permitted >
  omit, with unknown as omit > summarize > ask > pass, items.id=924; equal words keep the snapshot, so an explicit pass
  survives a new value of the same field). An unreadable table or database is omit for every bridged field.
  A resumed run builds its track again, so a policy tightened while it was parked is honoured (T9).
- GATE 1 report_scope. gate1 takes an optional set of declared names (execute_once passes the step's
  field_requirements keys): fields_shared, fields_abstracted, fields_withheld and floor_clamped_fields
  (the log row and the floor-consent prompt) cover declared fields only. approved_fields stays unfiltered and
  the declared-field filter in execute_once stays the live control (T3 proves it, step built bypassing the lint).
  A floor-consent prompt therefore fires only for a field the step declares.
- FAIL CLOSED MAPPING (executor.rs to_gate_track_with, bridged fields only): an unknown sensitivity maps to
  financial; a field outside the catalogue or outside general/personal is not_permitted at both tiers. An
  ordinary field keeps the old mapping (unknown is general).
- VALUE. A bridged value has `{` and `}` turned into `(` and `)`, because render_template WAS not single
  pass (it replaced token by token over a HashMap, so a value naming {user_input} could be expanded by a
  later substitution). items.id=932 made it single-pass; the neutralization STAYS as defence in depth (see
  "Single-pass render substitution"). A value that fails the field
  validator (length, content) keeps its CLEANED text in PersonalTrack.fields, which Gate 2, the voice scan and
  step sensitivity still read, but is not_permitted at tiers 2 and 3, so Gate 1 never releases it to an
  external step. At tier 1 Gate 1 approves every field raw, so a LOCAL step that declares it renders its
  cleaned text (items.id=933, accepted by Chat-PM, as is a confirmed cross-Persona fact; see "Local render
  of personal fields and the carry scan").
- LOAD LINT (declared_inputs::lint_field_requirements, both loaders): in an external step every
  field_requirements name must be a catalogue name, be named as a {token} in the template and equal no
  output_var, bound name or system token.
- ACCEPTED SIDE EFFECTS (decisions.id=953 ruling 11), for any persona holding a catalogued fact even with
  no policy: the first external run may park once on floor consent, voice-profile contamination can strip an
  attribute (local) or halt an external step, Gate 2 can flag a reply echoing a value (flag only), step and
  output sensitivity rise to personal, and checkpoint manifests carry name:value hashes. Local steps are
  otherwise unchanged.
- THE UI (items.id=960) is the Sharing block of My Facts: see "My Facts: sharing choices" in
  entity-model.md.
- NOT BUILT: group facts, bridging entity-scoped (device) facts into PersonalTrack (device
  values reach a hosted step through their own path since items.id=924), releasing medical,
  financial or cross-Persona facts, an end-to-end hosted call (the
  loopback client is not HTTPS). Hand checks for the live app are listed in the items.id=931 handoff.
  items.id=633 absorbs personal_012.sql (items.id=930).

## Single-pass render substitution (items.id=932, decisions.id=953 amendment of 2026-11-17)
render_template (conductor/executor.rs) is ONE call to substitute_once: a left-to-right pass over the
TEMPLATE. A substituted value is never scanned again, so a value holding {user_input}, {previous_output}
or any token text appears literally, and the result does not depend on HashMap order. Before this,
render_template did String::replace per token on the RUNNING result (so later tokens re-expanded earlier
values, in HashMap order) and then stripped every leftover {x} from the whole result, values included.
- SCANNING is find_tokens() exactly: at `{` look for `}`; a nested `{` or the end means the `{` is literal.
  A closed span whose text is a KEY of the map is replaced by its value (ANY brace-free key is looked up:
  cb-15 declared names and an ordinary output_var are not required to be snake_case, valid_name and
  valid_var_name allow uppercase, and an ordinary output_var is not shape-checked at LOAD). Else a span shaped
  [a-z_][a-z0-9_]* is an unresolved TEMPLATE token: removed, logged at DEBUG. Else the span is kept.
- EVERY RENDER PATH goes through it: render_prompt (local), render_prompt_with_disclosure (every step with
  treatment.hosted), fixed_tokens_of_step (empty map: strips every template token) and the cb-15 starter
  (handoff_render::render_starter, which also still cleans values: braces become parentheses). No other
  production code substitutes tokens (scan: token_substitution_has_one_site_and_it_is_single_pass).
  examples/support/mod.rs keeps its own replace loop (the engine is pub(crate)); its comment says so.
- BEHAVIOUR CHANGE, intended: braces in a VALUE (a user turn, a model output, a disclosure value) are no
  longer stripped or expanded, they reach the prompt as typed. The outbound review sees the final text as
  before. Gate 1, the disclosure buffer, report_scope and the treatment are untouched.
- PARITY. The retired Python oracle (_render_template, git a27a2b1^) did sequential replace and so DID
  re-expand; Rust deliberately differs on this one point. tests/golden_vectors.rs covers Gates 1 to 4 only
  and has no render vector, so no vector changed.
- NO SHIPPED TEMPLATE RELIES ON RE-EXPANSION: lifecycle.rs no_shipped_template_relies_on_re_expansion
  renders every template of the six .focus files and every Focus seeded in shared.db (Writing Assistant
  included) with brace-free canary values and requires the output equal to the old algorithm's in both key
  orders. A community .focus file that relied on it is unknowable.
- KEPT AS DEFENCE IN DEPTH: seeding::clean (braces to parentheses on seeded store names), the
  personal_fact_bridge brace neutralization, insert_projected_fields (the same clean_braces on every
  projected value, both renders, items.id=933), StoreTokens::apply (which decides which seeds a step is
  given).
- TESTS: render_template_never_re_expands_a_value, the_local_render_..., the_external_render_...,
  a_value_arrives_cleaned_and_never_expands_in_the_starter (the starter test cannot fail from the engine alone
  because clean() runs first), single_pass_against_the_pre_932_algorithm_by_class (every class of difference
  is a row with its explanation: value naming another key, value wrapped in template braces, value with
  unknown or malformed braces; nested {{a}}, unclosed braces, invalid shapes are identical). Mutations M1 to
  M5 (old loop, post-pass strip, per-path post-render replace, fixpoint, unresolved token left) each fail a test.

## Local render of personal fields and the carry scan (items.id=933, privacy surface review 2026-11-20 F4, Jason's ruling option A, Chat-PM M2)
A local step that declares a personal field now receives it, as an external step does; and since that
lets a local step's output carry a raw value into a hosted step, the hosted step scans what it carries.
- THE RENDER (unit 1, executor.rs). execute_once reads the step's disclosure buffer ONCE, before the
  `treatment.hosted` branch, and both render_prompt (local) and render_prompt_with_disclosure (external)
  insert it through ONE helper, insert_projected_fields (a scan pins one inserter, called by both). The
  buffer is Gate 1's approved values filtered to the step's declared field_requirements (the declared-field
  filter stays the control: an undeclared {field} still renders empty on both paths). At abstraction tier 1
  that is the RAW value whatever the policy (not_permitted, omit, a validator-failed value and a confirmed
  cross-Persona fact included: "Raw values permitted at Tier 1", accepted). Order: output_vars, then the
  projected fields, then the system tokens, then the store seeds, so a field wins over an output_var of the
  same name and a system token wins over a field. RIDER: there is NO LOAD lint against such a collision for a
  LOCAL step (an external step's field names are linted, items.id=931); the precedence above is the rule.
  Values are brace-cleaned with personal_fact_bridge::clean_braces on both paths (defence in depth;
  production values are already clean at the bridge).
- WHAT CHANGED MEANING (no code change): the local step's gate1_pass row (fields_shared now says what the
  model saw), the snapshot's disclosure buffers (they now hold values the step used; the buffer is rewritten
  before it is read in every attempt, so a restored one is never rendered), resume staleness (counting buffer
  keys as used is now correct, not over-inclusive), Gate 2 (shared fields are excluded from its scan, so no
  new flags). Step severity was and is scored from the field names the template mentions.
- A BRIDGED FIELD CAN BE SEVERITY 3: a fact whose STORED sensitivity is medical or financial bridges at that
  sensitivity (effective_sensitivity, severity_of), not_permitted at tiers 2 and 3. Its local step was already
  scored 3; it now also holds the value, and Gate 3 at the next promotion acts on that severity as before.
- THE CARRY SCAN (unit 2, conductor/carry_scan.rs, pure; called in execute_once for every step with
  `treatment.hosted` and no bound API Chat run, after the render and before the outbound review):
  - TRIGGER: gate2::scan_matches (Gate 2's rule: case-insensitive substring, at least 4 characters) over each
    CARRIED value the template names (an output variable or {previous_output}; never {user_input}, the
    projected fields or the seeds). Only bridged fields count (personal_fact_bridge::is_bridged_source).
    items.id=924 adds two triggers on the same path: a bridged field at ask (any occurrence) and a device
    value the pass seeded (any occurrence); see "Device facts in external steps and allowed answers".
    A field this step released RAW is skipped: g1.fields_shared holds only approved values equal to the raw
    value, so a summarize label is NOT in it and its raw value carried in is raised (tested). A pass field
    the step did not declare is raised (over-asking).
  - ACTION by the field's policy at the step's tier (tier 2 when the abstraction tier is 1): not_permitted ->
    every occurrence is replaced by "[removed]" BEFORE the review, a disclosure row event
    carry_not_permitted_removed names the fields (never a value; fatal on write failure), and it is never
    shown, so no answer releases it; anything else -> every occurrence becomes a gate3::SourceSpan (category
    qr_personal_field, label "Personal detail", placeholder wording Chat-BRAND's) whose suggestion is the
    field's abstraction at that tier when it differs from the raw value (a summarize label), else none.
  - EVERY OCCURRENCE in the final prompt is covered, THE USER'S OWN TYPED TEXT INCLUDED: a user-typed
    occurrence of a flagged value is raised (or removed) too, because leaving one would send the value
    anyway. Overlapping values: the longest wins.
  - THE REVIEW: ReviewInput.source_spans -> ReviewScanner::scan -> gate3's new `source_spans` argument,
    merged into the filter's spans BEFORE the partition (gate3::merge_source_spans; a filter span overlapping
    one is dropped), so remembered answers, the step_reviews record, the consent request and the removal take
    the ordinary path; a hit is an asked span, never a new park code. Only the outbound review passes any
    (scan only_the_outbound_review_passes_source_spans; every other gate3 caller passes &[]). Check 2c: a
    source span off the Privacy Filter path blocks (gate3_source_spans_unreviewed); the outbound review parks
    first when the filter is unavailable, so this is a backstop.
  - REMEMBERED ANSWERS: a carried span has no Layer-1 fact hash; Layer 3 resolves it to the bridged fact
    when its text equals the stored value EXACTLY (trimmed), so a standing or in-run answer on that fact
    applies. Another case ("LEEDS" for "Leeds") is asked again (errs toward asking).
  - LIMITS, stated plainly: a text match MISSES A PARAPHRASE ("the Leeds area" is caught, "West Yorkshire" is
    not); a value UNDER 4 CHARACTERS is not scanned and so not caught; a SUBSTRING inside a longer word is
    caught; case folding is per character. Out of scope: group facts, persona_context, a cb-15 starter
    (copy-time review only), the severity label (the A -> C -> B gap above). Device values: items.id=924.
- TESTS: a_local_step_renders_its_declared_personal_field (red "Location:", green "Location: Leeds"), the
  undeclared twin, both renders' precedence, a_summarize_field_whose_raw_value_is_carried_is_raised_in_the_review,
  a_field_the_step_released_raw_or_nothing_carried_raises_nothing, a_not_permitted_carried_value_is_removed_and_never_shown,
  a_value_a_local_step_carries_still_meets_gate_3_and_the_outbound_review (plan item 4: Gate 3 at the local
  step approves at severity 2, the hosted step raises the carried value), carry_scan's pure tests, gate3's
  merge, suggestion, Check 2c, remembered-answer and scan tests; mutations M1 to M5 and N1 to N10 killed.
- NOT VERIFIED: the merge inside gate3_with_pf against the real filter (it needs an app handle: tested
  through its pure parts and the forced scanner), the live app hand check (research-and-buy with a stored
  city, role-assessment with a stored employer, industry and job title), Windows and macOS.
