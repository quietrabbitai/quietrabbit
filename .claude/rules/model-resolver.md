---
paths:
  - "src-tauri/src/conductor/model_resolver*.rs"
  - "src-tauri/src/conductor/model_capability.rs"
  - "src-tauri/src/conductor/model_selection.rs"
  - "src-tauri/src/persistence/model_capability_store.rs"
  - "src-tauri/src/provider_catalog/**"
  - "src-tauri/catalog/**"
  - "src-tauri/schema/shared_045.sql"
  - "src-tauri/src/spend/codes.rs"
  - "src-tauri/src/conductor/executor.rs"
---

## Model resolver (items.id=896, decisions.id=941 point 4, decisions.id=942 point 9)
A pure resolver chooses the model of a MODEL-BLOCK step: the lowest-cost model that
reaches the step's quality bar inside its ceiling. Rules are in the header of
conductor/model_resolver.rs; the gatherer is conductor/model_resolver_input.rs; the glue
is select_model_for_block in executor.rs.
- SCOPE. Only a step whose ActionDefinition carries `requirement`, on a run that is not
  a bound API Chat run (execute_once: resolver_req). A step with no requirement keeps
  Step 4.5 and select_model's hardcoded ids (decisions.id=870), and a bound API Chat run
  keeps its bound destination whatever the step carries. Both are pinned by tests in
  executor.rs.
- ANSWERS. Selected (with the other qualifying LOCAL models as ranked fallbacks), TooWeak
  (nothing reaches the bar: the best one below it, and every other candidate as below the
  bar or not measured) or NoCandidate (no destination, none ready, window too small).
  The last two park the step (F10, AwaitUser) with the codes resolver.too_weak and
  resolver.no_candidate in spend/codes.rs; en.json has placeholder keys under
  resolver.stop (Chat-BRAND owns the wording) and a Rust test requires a key per code.
  The required bar is quality_floor.min. Nothing below it is ever selected; quality_floor.max
  is not read by the resolver. The run plan (items.id=901) uses it as a HINT only: an offer
  already above it is flagged and sorted last, never filtered.
- RANK among qualifiers: local, then free, then plan-included, then metered; at
  anonymous_preferred an anonymous provider first inside a cost tier; then the smallest
  model class (or the class nearest the block's class_preference, which ranks and never
  filters); then estimated cost; then ids. A local model that qualifies always beats a
  hosted one. No model or provider id is named in the resolver (a scan test pins it).
- CAPABILITY DATA. shared.db model_capabilities (shared_045.sql): one measured cell per
  (subject local or hosted, provider, model, task type, document profile), holding the
  highest named bar reached (below_basic, basic, standard, high), the model class, the
  sample count and provenance. A cell with NO row is UNMEASURED, which is not below_basic:
  an unmeasured model, hosted or local, is NEVER chosen automatically and is listed as
  not measured in TooWeak's options (pinned in the resolver and in executor.rs tests).
  Rows come only from the signed catalog (its `capabilities` section, validated and
  undoable in provider_catalog/apply.rs, needing min_shared_schema 45) and from
  shared_045.sql's seed, which is the baseline catalog's own section; nothing at run time
  writes the table (persistence/model_capability_store.rs is read-only).
- BARS (Jason's read of the items.id=899 calibration run, 100 reads over run 20261116-213009;
  21 cells, every one jason_read, 12 drafts each, dated 2026-11-17; many rest on only 3 reads, so
  a cell with 2 reads at one bar and 1 below takes the higher bar: the cell's note says the
  split). Writing cells (creative_writing, per shipped type): rma_letter: llama3.2:3b
  below_basic, llama3.1:8b standard, qwen2.5:7b standard; complaint_email: 3b standard, 8b high,
  qwen standard; personal_note: 3b high, 8b high, qwen standard; cover_letter: 3b basic, 8b high,
  qwen high; essay_section: 3b high, 8b standard, qwen basic; creative_story: 3b high, 8b high,
  qwen basic. Voice (quick_response, task-type-wide, the lower of apply and refine): 3b standard,
  8b basic, qwen basic. Only Jason's reads set a bar; the judge never does.
  Consequences: llama3.2:3b is NEVER chosen for an rma_letter step (below_basic), so the next
  qualifier is; a floor of high still parks for rma_letter and several others; no shipped type
  or block task type is unrated any more (UNRATED_SHIPPED_TYPES and UNRATED_BLOCK_TASK_TYPES in
  model_block/admission_tests.rs are EMPTY and a new type or block forces a decision there
  again). Preferences live in `note` only. New measurements go through the harness
  ("Calibration harness"), never by hand-editing a bar.
- DOCUMENT PROFILE. The profile is the id of a SHIPPED document type and is read ONLY from
  the pass's seeds under the bound name `document_type` (DOCUMENT_TYPE_NAME in
  model_resolver_input.rs), never from the block's own configuration. A custom type
  resolves through `based_on` to its shipped type (profile_for_document_type; custom types
  are not built yet, items.id=905, so the glue passes none). The name is registered in
  bound_spec (DOCUMENT_TYPE, Source::Launch, "Launch questions and run-start settings"
  below) and a Focus with a model block always has a value: LOAD refuses one that declares no
  document_type question, and the question's default is one of its options. An absent or
  unmappable name (a caller that supplied no launch resolution fails the pass closed before
  this; an id that maps to no shipped profile) means no profile, the resolver reads only
  the task-type-wide cell, the shipped data has none, and the step parks with
  resolver.too_weak: fail closed on purpose. The exact profile's cell wins, then the
  task-type-wide cell, never another profile's.
- READINESS (the gatherer only marks Ready or not). Local: a curated focus-eligible row,
  its tag exactly one of CURATED_LOCAL_MODEL_TAGS, installed and not disabled by the user.
  Hosted: a cloud_inference_api provider the ceiling permits (providers_permitted_by_
  ceiling), with a stored key, a current consent and a readable price (the user's override
  first); a model of unknown price is never chosen automatically. The spend gate still
  decides at call time. Cloud Chat panes are never candidates (decisions.id=680).
- NETWORK PREDICATE (the privacy-critical diff, reviewed line by line by Chat-PM). For a
  resolved step, `hosted_step` and the hosted call's provider follow the CHOSEN model
  (selected_model.hosted_model, selected_model.provider_id), not the ceiling or the user's
  preference. SelectedModel::local and ::hosted are the only constructors the resolver glue
  uses, so a hosted selection always carries its provider and its row together, which is
  why the generation branch's `unreachable!` stays unreachable. A hosted choice under a
  LocalOnly ceiling is refused before any gate by step_treatment::derive (items.id=921
  moved the refusal there from select_model_for_block, wording unchanged). Since items.id=921
  the render branch also follows the destination (`treatment.hosted`, see "Privacy
  treatment of a step attempt" below). executor.rs has a source-scan test pinning the
  three sites.
- CONTEXT FALLBACK. If a resolved local step overflows its window after the render, it
  moves to the next ranked local candidate that fits. It does NOT re-render and does NOT
  re-run Gate 1 (every fallback is local, as is the model it replaces, so the provider
  Gate 1 recorded, none, is unchanged). The disclosure log records a provider and fields,
  not a model tag, so it is the same with or without a fallback; the switch is logged only
  as an info line with the two local tags. A step that sets its own num_ctx never falls back.
- KNOWN BEHAVIOURS LATER BUILDS MUST RESPECT:
  1. (items.id=921, landed) Gate 1, the floor consent gate, the render, the voice scan,
     Gate 2, the sensitivity score, the recorded tier and Gate 3 key off the SELECTED
     destination for a resolved step, not the ceiling. See "Privacy treatment of a step
     attempt" below. The ceiling still decides what a step MAY reach (AUTHORIZE, Step 3,
     the resolver's candidate set); it no longer decides how a chosen local model is gated.
  2. No shipped Focus declares a launch question yet (items.id=903 ships Writing
     Assistant's, with its rebuild), and no shipped Focus uses a model block, so nothing runs
     model blocks through the app today. A Focus that does must declare a document_type question,
     and its run resolves the value once at LOAD ("Launch questions and run-start settings").
  3. PlanIncluded is never produced (nothing records which plan a user holds): a hosted
     model is Free or Metered. A hosted model is never chosen until it has a measured cell
     (items.id=788, 726), and EXTERNAL_MODEL_BLOCKS_ALLOWED is still false (items.id=913).
     The hosted call through execute_once is not tested end to end (the real client is
     HTTPS only); hosted selection and the call site are tested separately.
  4. Local context windows still come from get_context_window's hardcoded tag map in
     executor.rs, which names models in code; the gatherer and the fallback reuse it.
  5. Each retry resolves again, and a resumed run resolves from its seeds as they are then.
  6. The resolver's context filter is a lower bound (the template's fixed text), so it
     never drops a model the post-render check would accept; the post-render check stays
     the authority.
  7. The baseline catalog's catalog_version is unchanged (2026101100) though its content
     gained capability cells dated 2026-11-13; Chat-PM records the bump on items.id=730 and
     the first signed catalog carries its own version. The baseline agreement test applies
     the baseline at a fixed later date for that reason.
  8. items.id=633 must absorb shared_045.sql and regenerate tests/golden/schema_shape.txt.
- Spec: PROVIDER_REGISTRY_AND_TIER_MODEL_SPEC.md (Chat-PM, rule 71) needs the resolver and
  the capability data, and Part 8h's per-user hosted ranking (verification status,
  qr_recommendation_rank, key-configured) is NOT used by the resolver, which ranks by cost
  tier, anonymity at anonymous_preferred, model class, estimated cost and ids.
