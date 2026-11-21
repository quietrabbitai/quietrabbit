---
paths:
  - "src-tauri/src/commands/messages.rs"
  - "src-tauri/src/commands/api_chat.rs"
  - "src-tauri/src/api_chat/**"
  - "src-tauri/src/conductor/executor.rs"
  - "src-tauri/src/conductor/lifecycle.rs"
  - "src-tauri/schema/outputs_018.sql"
---

## External steps and chat history (items.id=883, decisions.id=937 point 3 and decisions.id=952)
An external destination never receives a model turn that a chat folded into {user_input}: a model
turn can carry persona-derived text from an earlier local run. The local render is unchanged.
- TWO FORMS, ONE FOLD. commands/messages.rs fold_window() builds both: build_conversation_prompt
  (`User: ...` and `Assistant: ...` lines, the LOCAL form, FocusRun.user_input) and
  build_user_turns_prompt (the same last HISTORY_WINDOW messages, the same `User: ` label and skips,
  user rows only, FocusRun.external_input). The second is a strict subset of the first by construction
  (a test pins it). A user turn is kept verbatim, including one the user pasted from a model: it cannot
  be told apart from user text, and the outbound review (items.id=884) is its control.
- WHAT READS WHICH. render_prompt (local) inserts ctx.user_input; render_prompt_with_disclosure (every
  step with `treatment.hosted`) and the cb-15 starter (lifecycle.rs execute_handoff_render) insert
  ctx.external_input and never name the full window (scan tests in executor.rs). new() defaults
  external_input to user_input, which is right for a run with no history (submit_focus_run's typed
  text). send_message passes Some(build_user_turns_prompt) and send_api_chat_message Some(its API
  history) as the new load_and_authorize_run parameter `external_input` (server-side, never a DTO
  field): it must be set BEFORE the focus_runs row is written, because that row stores it.
- RESUME. focus_runs.external_input (schema/outputs_018.sql, items.id=633 must absorb it) is written beside
  user_input and cleared at EVERY site that clears user_input (the lifecycle writer, output_store::
  set_focus_run_status and cancel_focus_run; the same keep_input rule: a terminal status, the one-time
  latch, focus_level consent). rehydrate_focus_run reads it and NEVER derives it from user_input (a model
  reply can contain a line that starts with `User: `, so parsing the window would pass model text off as a
  user turn; a scan pins it). A NULL (a row from before outputs_018, or one written without a copy)
  resumes with an EMPTY external input: an external step renders {user_input} empty while a local step
  still sees the stored window. Resume and the scheduled sweep both go through rehydrate_focus_run.
- API CHAT, RULE C (amends decisions.id=888 point 8). build_api_history keeps an assistant turn ONLY
  when its provider_id is the bound destination's; a turn from another provider or with no recorded
  producer is left out, flagged or not (the earlier rule left out only a flagged one). With an immutable
  binding this never fires: it governs transfers and fan-out. A flagged one still counts in
  omitted_flagged, the rest in ApiHistory.omitted_other_producer (not an IPC field).
- NOT CHANGED: output variables and {previous_output} that carry local-step output into an external
  prompt (items.id=922's carry, not this item), and the declared-inputs lint (user_input is declared as
  before).
