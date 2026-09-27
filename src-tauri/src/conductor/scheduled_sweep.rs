// src-tauri/src/conductor/scheduled_sweep.rs
//
// items.id=496 (point 3): the scheduled-trigger firing mechanism. A
// composition-authored step's schedule_trigger (tokens.rs::StepDefinition,
// execute()'s gate in lifecycle.rs) parks its run at
// focus_runs.status='awaiting_schedule' with focus_run_steps.scheduled_for
// set to the trigger's resolved fire time. This module is the other half:
// a generic spawn_supervised background loop (main.rs, same idiom as
// idle_timeout_check/group_persona_sync_sweep -- decisions.id=839, a
// scheduler is runner infrastructure, not a building block) that notices
// when scheduled_for has passed and nudges the run forward.
//
// NOT A BYPASS OF FocusRun/resume_run (items.id=496 point 3, confirmed):
// run_periodic_sweep() calls the exact same internal functions
// items.id=245 built -- conductor::lifecycle::rehydrate_focus_run() +
// FocusRun::resume_execution() -- the same way commands::execution::
// resume_run's own IPC handler does. No new IPC command, no direct block
// invoke(). resume_execution() re-enters execute() at self.current_step,
// which the gate parked at the trigger step's own index (see
// rehydrate_focus_run()'s resume-index derivation for why 'awaiting_schedule'
// resumes AT that index, not past it); the gate re-evaluates
// is_active() and, now true, falls through into ordinary execution.
//
// KEY RESIDENCY SCOPE (items.id=496 point 3, deliberate): this only works
// while a key happens to be resident -- KeyRegistry is a single account
// slot (auth::registry module header), so one tick can service at most one
// logged-in account's personas. No key resident is a no-op tick, not an
// error. True background execution (app closed, key cleared) is out of
// scope -- tracked separately as items.id=576, blocked on this item, not
// folded in here.

use std::sync::Arc;

use crate::auth::registry::{key_hex, KeyRegistry};
use crate::conductor::concurrency::ConductorScheduler;
use crate::conductor::lifecycle::{rehydrate_focus_run, FocusRun};
use crate::persistence::output_store::list_due_scheduled_focus_runs;
use crate::persistence::persona_store::list_personas_for_user;
use crate::providers::utils::now;

/// One sweep tick. Called on an interval by main.rs's spawn_supervised
/// loop -- the loop itself lives there, matching every other periodic
/// checker's split (idle_timeout_check, group_persona_sync_sweep).
pub async fn run_periodic_sweep(
    pool: &sqlx::SqlitePool,
    key_registry: &KeyRegistry,
    scheduler: &Arc<ConductorScheduler>,
    app_handle: Option<tauri::AppHandle<tauri::Wry>>,
) {
    let Some((user_id, key_hex_str)) = key_registry
        .with_key(|k| (k.user_id.clone(), key_hex(&k.master_key)))
        .await
    else {
        return; // no account resident -- nothing this tick can check against
    };

    let personas = match list_personas_for_user(pool, &user_id).await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("scheduled_sweep: could not list personas for resident user: {e}");
            return;
        }
    };

    let now_ts = now();

    for persona in personas {
        let due = match list_due_scheduled_focus_runs(&user_id, &persona.id, &key_hex_str, &now_ts)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                // Expected/benign for a persona that has never opened
                // outputs.db this session, or has no scheduled steps at
                // all -- same "best-effort, log and move on" posture
                // get_focus_run_last_used (this module's sibling reads)
                // already takes, not escalated to a hard failure for one
                // persona in a multi-persona sweep.
                log::debug!(
                    "scheduled_sweep: could not check persona '{}' for due steps: {e}",
                    persona.id
                );
                continue;
            }
        };

        for focus_run_id in due {
            let result: Result<FocusRun, _> = rehydrate_focus_run(
                user_id.clone(),
                persona.id.clone(),
                focus_run_id.clone(),
                pool.clone(),
                Arc::clone(scheduler),
                Some(key_hex_str.clone()),
                // No user in the loop at fire time -- same
                // graceful-degrade precedent a user-initiated resume_run
                // already establishes for a request with no such field
                // (commands::execution::resume_run, items.id=245).
                std::collections::HashSet::new(),
                app_handle.clone(),
            )
            .await;

            let mut run = match result {
                Ok(run) => run,
                Err(e) => {
                    log::warn!(
                        "scheduled_sweep: rehydrate_focus_run failed for run '{focus_run_id}': {e}"
                    );
                    continue;
                }
            };

            if let Err(e) = run.resume_execution().await {
                log::warn!(
                    "scheduled_sweep: resume_execution failed for run '{focus_run_id}': {e}"
                );
            }
        }
    }
}
