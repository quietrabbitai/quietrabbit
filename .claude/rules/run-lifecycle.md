---
paths:
  - "src-tauri/src/conductor/run_retention.rs"
  - "src-tauri/src/conductor/resume_*.rs"
  - "src-tauri/src/conductor/scheduled_sweep.rs"
  - "src-tauri/src/conductor/extract.rs"
  - "src-tauri/src/conductor/reentry.rs"
  - "src-tauri/src/conductor/lifecycle.rs"
  - "src-tauri/src/commands/execution.rs"
  - "src-tauri/src/commands/chats.rs"
  - "src-tauri/src/persistence/output_store.rs"
  - "src-tauri/src/persistence/chat_clear*.rs"
  - "src-tauri/src/persistence/store_sweep*.rs"
  - "src-tauri/schema/outputs_019.sql"
  - "frontend/src/stepReview/**"
---

## Parked-run retention and resume staleness (items.id=681, decisions.id=865)
A parked run's snapshot holds step outputs and disclosure buffers with the values approved for each
step (raw at tier 1, abstracted from tier 2 up), encrypted at rest in the persona's outputs.db.
A run keeps snapshots while it runs (the periodic checkpoints, until output() purges them) and while it is
parked in awaiting_user, awaiting_schedule or paused. complete, cancelled and
failed delete them when the status is written (cleanup, fail_run, the cancel writers; best effort, and no
sweep selects a finished run). That was FALSE for a run failing through execute_full's error arms until
items.id=888 stage 1 (see "Run failure, resume claim and post-output runs"). This item bounds how long a
parked run keeps them, and refuses a resume on changed data. Runs whose reply was saved and that wait for
nothing (awaiting_extract_confirm, awaiting_feedback) are not parked: items.id=888's post-output sweep,
in the same tick and login pass, settles them (same section).
- PURGE_AFTER (conductor/run_retention.rs, CheckpointPark). Written when a run parks, by
  FocusRun::write_checkpoint_for (the one writer; write_checkpoint is the periodic one and stores NULL):
  awaiting_user = checkpoint time + AWAITING_USER_RETENTION_DAYS (30; each new checkpoint is a new row, so
  it refreshes), awaiting_schedule = the step's scheduled_for + AWAITING_SCHEDULE_RETENTION_DAYS (30). One
  fixed text format (whole seconds, UTC, Z). A scan pins the four parking sites.
- THE SWEEP. PARKED_STATUSES = awaiting_user, awaiting_schedule, paused. output_store::expire_parked_run
  runs per run in ONE BEGIN IMMEDIATE transaction (decide, then cancel_parked_in_tx): the run's latest
  snapshot is expired when its purge_after has passed; a snapshot with NO purge_after (the periodic one a
  crash left, a run demote_interrupted_runs turned into paused) expires at created_at + 30 days; a parked run
  with NO snapshot at all expires at started_at + 30 days (it cannot resume and still holds user_input); an
  unparseable time is never expired. The cancel is UPDATE focus_runs SET status='cancelled', user_input and
  external_input NULL, notes.cancel_reason WHERE id=? AND status IN (the parked set), and only if exactly one
  row changed are the snapshots, the park reason and a pending step review removed: a run that resumed
  meanwhile is left alone. This is the FOURTH site that clears focus_runs.user_input (beside the lifecycle
  writer, set_focus_run_status and cancel_focus_run). The message is "expired while waiting"; focus_runs has
  no message column, so it is in notes and in the event's step_display_name.
- WHEN IT RUNS. Hourly supervised task parked_run_expiry (main.rs, pinned in writer_gate MAIN_TASKS) and one
  pass per persona inside the login recovery task right after the demote. Only while the owner's key is
  resident (decisions.id=847): the tick does nothing without one. Lease kind Sweep (short, not visible work: it
  neither defers the demote nor counts as a run for a rotation to wait for; no FocusRun lease because it
  cancels, never resumes). The injected clock is `now`; only the supervised tick reads Utc::now. The login pass
  has no AppHandle and emits no event (the UI reads run state after login); the hourly tick emits
  run-status-update.
- RESUME STALENESS (conductor/resume_staleness.rs, pure `check`). rehydrate_focus_run reads the snapshot's
  personal_context_manifest and compares field_hashes ONLY with the fresh PersonalTrack. A resume is STALE when a
  field that a COMPLETED step used has changed or is gone. "Used" = a name in the disclosure buffer of a step in
  the restored task track, whether or not the step still declares it; the step re-run at resume (awaiting_user
  and awaiting_schedule resume AT the checkpointed step) is excluded. A field only in the fresh track (added) is
  not stale. A NULL, blank or hash-less manifest means no check; an unparseable non-empty manifest (or one that is
  not a JSON object) fails CLOSED. PersonalContextManifest::matches() is NOT used (it also compares
  source_versions, which a .focus edit or app update would change) and has no caller outside its own tests.
  Re-running Gate 1 is not the remedy: execute_once already runs Gate 1 on the fresh track before every step
  attempt and rewrites that step's buffer, and Gate 1 needs no AppHandle. What goes stale is the OUTPUT text of
  completed steps, which Gate 1 cannot change.
- HANDLING. rehydrate_focus_run returns LifecycleError::ResumeStale and writes NOTHING (it runs before the run
  is promoted: since items.id=888 the guarded claim writes `running`). The one helper resume_staleness::cancel_stale_resume is called
  by resume_run, reenter_step (commands/execution.rs) and scheduled_sweep: it cancels with "personal information
  changed while waiting" through the same guarded UPDATE (only if the run is still parked) and emits the event.
  A scan test pins the three callers.
- STATE PLAINLY (limits, accepted):
  (a) A confirmed cross-Persona fact released raw to a local step makes a resume cancel that run: both
      resume paths (execution.rs and scheduled_sweep) pass an empty confirmed-id set, so the fact is absent on
      resume and reads as deleted.
  (b) A policy tightened while a run is parked is NOT detected for completed output: the manifest holds no
      policy. The executor still re-reads fact_disclosure_policy before each later hosted step (decisions.id=953).
  (c) Entity (device), group and persona_context facts are NOT covered by this check: only the four
      catalogued singleton facts of items.id=931 are in the manifest. Device values a pass seeded are pinned
      by the resume pin's device_seed fingerprint instead (items.id=924, "Device facts in external steps").
- NOT BUILT: a rewind-and-rerun of the affected steps (items.id=574's reenter_step could do it; deferred
  as option B on items.id=888), a "continue anyway" resume, a Library hook, a banner for a stale cancel (the
  user sees the cancelled run and its message). The archive hook is built (items.id=888).

## Run failure, resume claim and post-output runs (items.id=888 pass 1, privacy review 2026-11-20 F5 to F8 and F15)
Pass 1 of items.id=888 (stages 1 to 3). Pass 2 (stages 4 to 8) is built: see "Resume state, archive hook
and store sweeps" below.
- FAIL_RUN (lifecycle.rs, F5 and F6). The one way an Err ends a run: drop the tracks, purge the run's
  snapshots BEFORE writing `failed` (write_run_row clears both inputs), emit. Both error arms of
  execute_full (the F_SYSTEM one too) and resume_claimed_run use it; a source scan pins that neither
  function writes `failed` itself. A resumed run that errors is therefore `failed`, not left `running`,
  and loses its parked state (before, the next login demoted it to paused and it could be retried).
- THE GUARDED CLAIM (F7). rehydrate_focus_run records a ResumeBasis (the status it read and the id of the
  latest snapshot, ordered created_at DESC, rowid DESC). output_store::claim_run is ONE UPDATE:
  status := to WHERE id AND status = basis.status AND status IN (allowed) AND latest snapshot id =
  basis.snapshot_id ('' for none); true only if one row changed. It changes the status and nothing else.
  - resume_execution = claim_resume()? (OUTSIDE any error arm) then resume_claimed_run(). A loser gets
    LifecycleError::ResumeRaced and has written nothing: no status, no purge, no event. resume_claimed_run
    refuses a FocusRun that did not win a claim. allowed = run_retention::RESUMABLE_STATUSES
    (awaiting_user, awaiting_schedule, paused).
  - resume_run (commands/execution.rs) refuses `running` and `initializing` (run_live:<status>), claims
    BEFORE it spawns (a lost claim: run_not_resumable) and spawns resume_claimed_run. reenter_step refuses
    the two live statuses and claims to awaiting_user (REENTERABLE_STATUSES adds awaiting_feedback) after
    plan_reentry and before any change; after a re-entry it carries its new checkpoint as its basis. The
    scheduled sweep skips a lost claim before finalize_chat_reply. A scan pins the two callers outside
    lifecycle.rs.
  - write_checkpoint_for returns the new snapshot id.
  - Checked with the claim: the step review's needs_resume auto-resume (at most 2, stepReview.ts; a repeat
    loses and takes resume_failed), Try again (ChatPane), lateReplyRecovery (reads status only).
- THE EXTRACTION FLAG (F8). conductor/extract.rs EXTRACT_CONFIRM_SCREEN_BUILT = false, pinned by a test:
  execute_and_finish skips the WHOLE pass after output() (no personal.db read, no model call, no
  candidate, no park) and completes the run. items.id=38 flips it with its confirm screen.
  items.id=914 adds a second block right after it: extraction_pass_applies(built, posture) skips the
  pass for a focus_level (or missing-definition) Focus even once the constant is true; see "Collection
  consent". resume_run's
  replay branch and submit_extract_confirm are unchanged. awaiting_extract_confirm keeps no input
  (USER_INPUT_CLEARED_STATUSES).
- THE POST-OUTPUT SWEEP (F8). run_retention::POST_OUTPUT_STATUSES = awaiting_extract_confirm,
  awaiting_feedback (disjoint from RESUMABLE_STATUSES and PARKED_STATUSES, pinned).
  output_store::sweep_post_output_runs, in the hourly tick and the login pass, key resident only:
  every pass NULLs both inputs of every awaiting_extract_confirm row, any age; at started_at +
  POST_OUTPUT_RETENTION_DAYS (30) settle_post_output_in_tx sets `complete`, both inputs NULL and
  notes.cancel_reason "expired while waiting" WHERE id AND status IN (the set), and only if one row changed
  deletes EVERY candidate row of the run (pending, declined, confirmed), any snapshot, the park and a
  pending review. Settle and claim cannot both act: both are single UPDATEs guarded on disjoint status sets.
  The tick emits run-status-update `complete` for a settled run (Swept::Settled).
- F15. purge_device cancels the Focus's parked runs, as purge_all_devices does.
- KNOWN LIMITS, accepted:
  (a) A re-entry that fails after its claim (a database error mid-way) leaves the run at awaiting_user with
      its EARLIER checkpoint, so a resume of a run that was paused re-runs the checkpointed step.
  (b) A crashed `running` run cannot be resumed in-session: it becomes paused at the next login
      (demote_interrupted_runs).
  (c) The raw codes run_not_resumable and run_live:<status> reach the user (Try again notice, review modal
      error), like run_already_finished:. Wording is Chat-BRAND's.
  (d) Extraction is off everywhere (chat, Board, resume, scheduled sweep) until items.id=38.
  (e) Existing rows: the first login after the upgrade clears the inputs of every awaiting_extract_confirm
      run and settles every post-output run older than 30 days, deleting its candidates.
  (f) The purge confirmations do not say that parked runs are cancelled (Chat-BRAND).

## Resume state, archive hook and store sweeps (items.id=888 pass 2, decisions.id=939 sections 4, 6 and 7)
Pass 2 of items.id=888 (stages 4 to 8). Option B (rewind and re-run the affected steps on a stale resume)
is deferred. The byte cap on the chat history window is not built and stays deferred on the item.
- CONTEXT_KEY ON REHYDRATE (stage 4). focus_runs.context_key (schema/outputs_019.sql, nullable; items.id=633
  must absorb it) is written at the INSERT of every run that has a chat (write_run_row), not only runs that
  park: a crashed run becomes paused with no park write, and the archive hook needs every run of the chat.
  It is the chat id, not user content, and is never cleared. rehydrate_focus_run reads it and, BEFORE
  anything else is built, reads the chat's one-time latch: suppressed, or a latch that cannot be read,
  returns LifecycleError::ResumeOneTimeChat and writes nothing; the shared helper
  resume_staleness::cancel_stale_resume (resume_run, reenter_step, the scheduled sweep) then cancels the
  run through the guarded cancel with "a one-time chat cannot be resumed". The key is set before load(),
  so the run reads the chat's launch answers and run_change. After the ceiling is resolved, a run of a chat
  runs the read-only plan_gate (the app's PlanMemory when there is an app handle, else an empty one, so
  only the persisted row), and the Gate 3 look-ahead no longer excludes a rehydrated run (only plan_gate.rs
  sets `plan`).
- THE RESUME PIN (stage 5, conductor/resume_pin.rs). Every checkpoint (write_checkpoint_for, periodic and
  parking) writes focus_run_snapshots.extra_metadata: `seed_versions`, the version of each VERSIONED bound
  name the steps read (session names but the latch, launch names, artifacts; 0 for no row) in this chat and
  Focus, and `published`, the ids store writers published this pass (FocusRun.store_vars: entity, case,
  delta, hand-off draft handle). Names, integers and opaque ids only, never a stored value. A store that
  cannot be read at checkpoint time records `seed_versions_unreadable`. On rehydrate, after the
  personal-field staleness check and before the track is restored, check_resume_pin re-reads the versions
  and re-verifies each id in its own store, scoped to the running Focus (an active owned entity; a case or
  delta this Focus recorded; a draft handle that resolves among its replies), and refuses with
  LifecycleError::ResumeStoreChanged on a changed version, an id that does not verify, the unreadable
  marker, metadata that does not parse, or pinned versions with no chat. The ids are restored only when
  every check passes. The shared helper cancels with "saved chat state changed while waiting".
  Re-entry with writer rows is tested (lifecycle.rs reentry_with_writer_rows).
- THE ARCHIVE HOOK (stage 6, persistence/chat_clear.rs). archive_chat archives the chat, then calls
  clear_chat(context_key) best effort (a failure is logged; the archive stands) and PlanMemory::clear_chat.
  In ONE BEGIN IMMEDIATE transaction: the chat's PARKED runs are cancelled through the guarded cancel
  ("chat archived": snapshots, park, pending review go); user_input and external_input are NULLed on every
  run of the chat; the park reason of every run of the chat goes; the chat's plan approvals, pass artifacts
  and session variables go; and the reviews of the chat's ENDED runs (complete, failed, cancelled, after the
  cancels) go through step_review_store::delete_reviews_of_ended_chat_runs_in_tx, that store's one delete
  (a scan pins chat_clear as its only caller; nothing reads a review once its run has ended, and
  review_prompt reuses only a review its own retry loop made, items.id=923 F13). THE ONE-TIME LATCH goes only when no run of the chat is still
  non-terminal after the cancels; otherwise every session row but the latch goes. session_store::
  clear_chat_in_tx is the only delete that can reach the latch (a scan pins it). Each delete lives in the
  store that owns its table.
- THE STORE SWEEP (stage 7, persistence/store_sweep.rs). run_retention::STORE_RETENTION_DAYS = 30, one
  period for session_variables (the latch EXEMPT), pass_artifacts and plan_approvals, by updated_at, each
  delete in its owning store and on that table's updated_at index. The cutoff is now minus the period as
  RFC 3339 text and the compare is TEXTUAL: it is correct because all three stores stamp updated_at
  through providers::utils::now() (RFC 3339, UTC). The three deletes run in ONE transaction, so a sweep
  that fails deletes nothing (suppression). It runs in the hourly retention tick and the login pass,
  after the post-output sweep, only while the owner's key is resident.
- ACCEPTED LIMITS:
  (a) An unreadable latch cancels the parked run on resume: suppression, and destructive.
  (b) A row from before outputs_019 has no context_key: its latch cannot be checked on resume, it reads
      no launch answers or approval, and the archive hook does not reach it.
  (c) A resumed run's plan approval is read and verified through plan_gate; a missing one, and a resumed
      run with no chat, still give plan.exceeded `no_approval`.
  (d) A crashed run whose own store writer ran after its last checkpoint has a pin that no longer matches:
      it is cancelled on resume, not resumed.
  (e) A transient read failure at checkpoint time makes that snapshot unresumable.
  (f) Derived names (case and delta texts) have no version and are not pinned. The device names have none
      either, but a pass that seeded a device pins its DeviceSeed fingerprint (items.id=924).
  (g) A run parked before stage 5 has no pin and resumes unchecked.
  (h) The cancel messages are "a one-time chat cannot be resumed" and "saved chat state changed while
      waiting"; wording is Chat-BRAND's.
  (i) The archive hook does not stop a live run (no lease is bound to a chat): what it writes after the
      clear stays, and its reviews stay (they are not ended runs' reviews and the store sweep deletes no
      reviews); the latch stays for it.
  (j) A chat still in use loses any session value, artifact or approval nobody wrote for 30 days (a launch
      answer, a pending hand-off marker); a parked run that pinned one is then refused on resume.
  (k) run_parks and step_reviews are not swept by the store sweep (parks go with their run's status
      writes and the archive hook; reviews only with the archive hook, for ended runs).
  (l) An updated_at in any format other than RFC 3339 is outside what the sweep promises: a value that
      sorts after the cutoff is kept, one that sorts before it (a number) is swept.
  (m) The archive hook emits no run-status event for the parked runs it cancels (it has no app handle);
      the UI sees them cancelled when it next reads run state.
  (n) Not verified: archive_chat and resume through Tauri, the live app, Windows and macOS.
