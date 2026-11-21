---
paths:
  - "src-tauri/src/conductor/launch_config.rs"
  - "src-tauri/src/conductor/run_settings.rs"
  - "src-tauri/src/conductor/focus_display.rs"
  - "src-tauri/src/commands/launch.rs"
  - "src-tauri/src/persistence/launch_answer_store.rs"
  - "src-tauri/schema/shared_047.sql"
  - "src-tauri/src/conductor/bound_spec.rs"
---

## Launch questions and run-start settings (items.id=897, decisions.id=942 point 8 and addendum 3 (g), decisions.id=948)
A Focus may ask up to five closed-choice questions before a run, or none. The answers feed
guards, the model resolver's document profile and the settings of every model block. No real
question or mode ships for any Focus yet (items.id=903 ships Writing Assistant's); the
mechanism is exercised by fixture Focuses in tests.
- DATA (schema/shared_047.sql, shipped data only, identical for every install, no foreign keys,
  classified GlobalCatalog in shared_db_residual.rs; items.id=633 must absorb it): focus_launch_
  questions (slot, kind mode or choice, asked, default_option, prompt), focus_launch_options
  (label, hint), focus_mode_profiles (envelope JSON) and focus_launch_display (ui_one_liner, at
  most 80 characters). Prompt, label, hint and ui_one_liner are placeholder wording owned by
  Chat-BRAND (items.id=506). ui_one_liner rows exist for writing-assistant, research-and-buy and
  role-assessment only. `asked` = 0 is a preset: the default is the value and nothing is submitted.
  The mode list of a Focus is the options of its one question of kind `mode` (slot launch_mode).
- DISPLAY DATA IS NOT COPIED. A Focus's display name and description stay in their one existing
  home and conductor/focus_display.rs read() is the single reader (load_focus_definition: the
  focuses row, else the .focus file; plus the one-liner). Writing Assistant's name and description
  are in both shared_038.sql and its .focus file and are pinned equal by a drift test; neither copy is
  removed. FocusInfo is unchanged.
- THE ONLY READER (conductor/launch_config.rs): validate() is the one rule set (at most 5
  questions, one question per slot, only the mode question on launch_mode, every question has at
  least one option and a default that is one of them, an asked question has two options, a
  document_type question offers shipped document types only, exactly one profile per mode option
  and none elsewhere, no empty destination set, no brace or {token} in wording). The reader of the
  rows is private (read_unvalidated); read_validated() runs validate() and is the only way out,
  pinned by a source scan (nothing else names the private reader or reads the three tables, LOAD
  calls read_validated and lint). A row this build cannot read is an error, never skipped.
- SIX BOUND NAMES (bound_spec.rs, Source::Launch): document_type, launch_mode, launch_answer_1 to 3
  (Presence::Always, Literals::Authored, a shape-only validator of 1 to 40 characters) and run_change
  (Presence::Optional, parses only as a SettingsPatch, at most 2048 bytes, so an envelope key, a
  tier, a destination or a gate does not parse). Rules: answers are stored as session variables of
  the chat but a run reads them ONCE at LOAD and every pass seeds that resolution; a Focus row
  never writes one (refused as a session_put target, a writer argument and an output_var); each
  is a registered store-backed name for an external step (it must declare one it names, a step that
  names one is tainted and so is its variable and every step downstream, so it stays out of
  extraction, and a cb-15 starter that declares one counts as personal); none is in
  DEVICE_NAMES_NEVER_IN_STARTER. DOCUMENT_TYPE_NAME (model_resolver_input.rs) equals DOCUMENT_TYPE.
- LOAD LINT (launch_config::lint, both load paths): a step that names a slot needs a question on it, a
  guard literal on a slot is one of that question's options, and a step with a `requirement` needs a
  document_type question (asked or fixed). run_change needs no question. On the DB path
  run_settings::trial_expand also expands every model row against every document-type option and
  every mode profile, so a combination that cannot be applied refuses the Focus.
- GUARD ANALYSIS. A slot is Authored, so a guard on it supplies its literals plus one implicit
  OTHER, exactly as for a model-produced router: a Focus guarded on a slot needs a fallback row (an
  `ne` or `present` guard), even though the run's value is always one of the options, and every
  guarded slot multiplies the enumerated paths, which count toward the 256-path cap
  (guard::MAX_PATHS).
- RUN-START HOOK (FocusRun::load() calls apply_launch_settings(), before authorize() and
  resolve_tier_config(), so the tier check sees the expanded steps; rehydrate_focus_run reaches it
  through load()). It resolves, in conductor/run_settings.rs: every question's value (the stored
  answer, else the default; always the default for a question that is not asked), the mode and its
  envelope override, the selected document type and the settings. It then re-expands every
  ModelOrigin (kept on FocusDefinition.model_origins) with the selected type (ExpandInput::
  document_type replaces the type the row names), the settings and the mode's envelope override,
  keeping each row's guards and schedule trigger. The mode profile is applied through tighten()
  inside the block's own finish(), after the Focus-ceiling clamp, and applies to EVERY model row of
  its Focus; a profile that removes a declared input or leaves no destination is refused (at LOAD by
  the trial expansion). The resolution lives on the FocusRun in memory only, so a retry and a
  resumed run resolve again.
- THREE SETTING LAYERS (resolve_settings, field by field, either direction): (1) the shipped default
  of the selected type, (2) the user's saved adjustments, (3) run_change. Layer 2 is one function,
  run_settings::saved_settings_patch(), which returns nothing until items.id=905. items.id=905
  provides: a per-persona ENCRYPTED store (never shared.db), validation through SettingsPatch and
  Scales::check_settings, custom document types fed into resolve_type and run_profile (executor.rs
  passes none today), the persona's custom ids in the submit command's allowed document_type set,
  and this function.
- SEEDING. seed_pass takes the run's LaunchResolution and seeds the launch names from it, opening no
  database for them (braces in run_change become parentheses). A caller that supplies none for a name a
  step needs fails the pass closed (SeedError::LaunchNotResolved), and so does the store reader if asked
  for a launch name. required_names adds document_type for any step with a `requirement`.
- STALE OR UNREADABLE. A stored answer that is not one of the question's options, a stored run_change
  that does not parse, an unreadable store or a config that does not validate fails the run
  (LifecycleError::LaunchAnswers), naming the question and never the value; no default replaces it.
- RESUME. Since items.id=888 rehydrate_focus_run carries the run's chat (focus_runs.context_key), so a
  resumed run reads the chat's stored answers and run_change exactly as a fresh run does. A resumed run
  with NO chat (a row from before outputs_019, or a run that never had one) cannot read them, and the
  defaults could be looser than what the user chose: if its Focus has an ASKED launch question it fails
  with LifecycleError::LaunchAnswersUnavailableOnResume. A Focus whose questions are all fixed, and a
  run with no chat (submit_focus_run), use the defaults.
  items.id=883 adds the same shape for the external text: focus_runs.external_input is stored beside
  user_input and a NULL resumes empty (see "External steps and chat history").
- ANSWER STORE AND THE ONE-TIME LATCH (persistence/launch_answer_store.rs; no new table: rows of
  session_variables under the six names). The only writer is put_answers(), which accepts only the
  ValidatedAnswers that launch_config::validate_submission() builds (every answer one of its
  question's options, a question that is not asked refused, a run_change parsed as a SettingsPatch
  whose named steps exist) and writes all of a submit in one BEGIN IMMEDIATE transaction. The
  latch is obeyed with no exemption: a latched chat refuses the whole submit (PutError::OneTimeChat,
  code one_time_chat), a latch that cannot be read fails it closed, and the run uses the Focus
  defaults. Answers stored BEFORE the latch was set stay (setting it deletes nothing), reads are not
  latch-gated, and so they keep seeding later passes of that chat. They go with the chat's archive
  hook, or after STORE_RETENTION_DAYS unwritten (items.id=888's store sweep), like every session row.
- RETRYING OPENER. The run-start read (read_answers_for_run) and the launch commands open outputs.db
  with output_store::open_outputs_db_retrying (commit 8d8d66f measured lock failures on the plain
  opener under contention, and a failed read here fails the run start closed). The scan test
  only_the_review_paths_open_with_the_retry lists persistence/launch_answer_store.rs.
- NOTHING APPROVES A HOSTED OR METERED STEP (decisions.id=942 Q3). A mode profile is an
  EnvelopeOverride (four keys, pinned) and only narrows (property-tested); a source scan pins that
  spend, review, treatment, provider-selection and executor code never name a launch name or the
  run-start module (the resolver names document_type only). EXTERNAL_MODEL_BLOCKS_ALLOWED is
  unchanged.
- IPC (commands/launch.rs, Group 38 of HANDOFF_IPC_SURFACE.md; ipc.rs comments number it as the
  next group after the outbound review): get_focus_launch_config(focus_id) (shipped data only, never a
  mode profile), get_launch_state(persona_id, context_key, focus_id) (read only, works in a latched
  chat: per question the stored answer, the effective value, from_default and stale, plus run_change,
  one_time_chat and defaults_apply) and submit_launch_answers(persona_id, context_key, focus_id,
  answers [{question_id, value}], run_change string or null) (returns the new state). Error codes,
  each with a placeholder key launch.error.<code> in en.json that a test requires: persona_not_found,
  invalid_focus_id, invalid_context_key, no_launch_config, unknown_question, question_not_asked,
  invalid_answer, invalid_run_change, one_time_chat, launch_unavailable. The keys are in the orphan
  allowlist of styleAndI18nConsistency.test.ts until items.id=902 shows them. No screen is built.
- NOT BUILT: saved adjustments and custom document types (items.id=905), a per-row mode selector, any
  real Focus question (items.id=903), the generic launch screen (items.id=902).
