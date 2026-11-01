// items.id=678: pure logic behind the nav shell's live Privacy Filter state. The
// Privacy Filter model is fetched in the background after login; the backend
// reports on the shared "task-progress" event with kind "privacy_model". No
// React, no Tauri: NavShell.tsx feeds this events and health reads, so each rule
// runs on plain Node in privacyModelProgress.test.ts.
//
// The payload is NOT in bindings.ts (events are not commands); its type is
// hand-typed in auth/rotationProgress.ts to match src-tauri/src/task_progress.rs.
//
// WHAT THE BANNER MAY TRUST:
//   1. a live "verifying" or "downloading" event: shown as it comes;
//   2. a final event ("complete", "failed", "cancelled"): carries no state of
//      its own, only a signal to re-read get_health, which is the single place
//      that knows the reason or that the filter is ready;
//   3. get_health on mount: applied only if no event has arrived since, so a
//      slow answer cannot overwrite newer progress.
// `message` on the payload is a backend reason code and is never rendered.
// There is no wording here: copy is Chat-BRAND's (items.id=839).

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { PrivacyFilterStatus } from '../bindings.ts'
import { percentOf, TASK_PROGRESS_EVENT, type TaskProgressPayload } from '../auth/rotationProgress.ts'

export { TASK_PROGRESS_EVENT }
export const PRIVACY_MODEL_KIND = 'privacy_model'

/** Narrowing guard for the untyped event payload. */
export function isPrivacyModelProgress(value: unknown): value is TaskProgressPayload {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Record<string, unknown>
  const isByteCount = (n: unknown) => n === null || n === undefined || typeof n === 'number'
  return (
    v.kind === PRIVACY_MODEL_KIND &&
    typeof v.task_id === 'string' &&
    typeof v.phase === 'string' &&
    isByteCount(v.current) &&
    isByteCount(v.total)
  )
}

/** What one progress event asks of the banner. */
export type ProgressEffect =
  | { kind: 'status'; status: PrivacyFilterStatus }
  | { kind: 'reread' }
  | { kind: 'ignore' }

export function effectOfProgress(payload: TaskProgressPayload): ProgressEffect {
  switch (payload.phase) {
    case 'verifying':
      return { kind: 'status', status: { kind: 'verifying' } }
    case 'downloading':
      return {
        kind: 'status',
        status: { kind: 'downloading', percent: percentOf(payload.current, payload.total) },
      }
    case 'complete':
    case 'failed':
    case 'cancelled':
      return { kind: 'reread' }
    default:
      // The phase vocabulary is open: an unknown phase changes nothing.
      return { kind: 'ignore' }
  }
}

/** The status after a get_health answer arrives: the answer, unless an event
 *  has already arrived (then the event is newer and the answer is stale). */
export function mergeHealthRead(
  eventSeen: boolean,
  current: PrivacyFilterStatus | null,
  read: PrivacyFilterStatus,
): PrivacyFilterStatus | null {
  return eventSeen ? current : read
}
