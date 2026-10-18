// Pure logic behind the key-rotation progress overlay (items.id=769 stage 1).
// No React, no Tauri: RotationProgress.tsx feeds it events and status reads
// and renders what it returns, so every rule below runs on plain Node in
// rotationProgress.test.ts.
//
// The "task-progress" payload is NOT in bindings.ts (tauri-specta only
// exports command and type signatures that appear in a command, and the event
// payload is emitted straight from src-tauri/src/task_progress.rs), so it is
// typed by hand here to match TaskProgressPayload in that file.
//
// WHAT THE OVERLAY MAY TRUST, in order:
//   1. a live "task-progress" event with kind "key_rotation";
//   2. commands.getRotationStatus(), but its `last_outcome` ONLY when this
//      webview saw the rotation go from running to idle. last_outcome is
//      process-wide and derived from the last final event, so on a cold mount
//      it describes some earlier rotation and must never be shown.
// `message` on the payload is a backend display label and is never rendered.

import type { LastRotationOutcome, RotationStatus } from '../bindings.ts'

export const TASK_PROGRESS_EVENT = 'task-progress'
export const KEY_ROTATION_KIND = 'key_rotation'
/** How often the overlay re-reads getRotationStatus while a rotation runs. */
export const STATUS_POLL_INTERVAL_MS = 1000
/** How long a form that started a rotation keeps "owning" its outcome after it
 *  lets go. The invoke response and the final event travel separately and can
 *  arrive in either order, so without this grace the overlay could announce an
 *  outcome the form has already shown. */
export const OWNER_RELEASE_GRACE_MS = 3000

/** Hand-typed to match src-tauri/src/task_progress.rs::TaskProgressPayload.
 *  current/total are BYTES; both are null before byte-level progress exists. */
export interface TaskProgressPayload {
  task_id: string
  kind: string
  phase: string
  current: number | null
  total: number | null
  message?: string | null
}

export type RotationOutcome = 'complete' | 'rolled_back' | 'failed'
export type RotationKind = 'password_change' | 'restore' | 'new_phrase'

const FINAL_PHASES: readonly string[] = ['complete', 'rolled_back', 'failed']

export function isFinalPhase(phase: string): phase is RotationOutcome {
  return FINAL_PHASES.includes(phase)
}

/** Narrowing guard for the untyped event payload. */
export function isKeyRotationProgress(value: unknown): value is TaskProgressPayload {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Record<string, unknown>
  const isByteCount = (n: unknown) => n === null || n === undefined || typeof n === 'number'
  return (
    v.kind === KEY_ROTATION_KIND &&
    typeof v.task_id === 'string' &&
    typeof v.phase === 'string' &&
    isByteCount(v.current) &&
    isByteCount(v.total)
  )
}

/** Whole percent 0 to 100 from byte counts, or null when there is nothing to
 *  divide by (the bar then shows as indeterminate). */
export function percentOf(
  current: number | null | undefined,
  total: number | null | undefined,
): number | null {
  if (typeof current !== 'number' || typeof total !== 'number' || total <= 0) return null
  return Math.min(100, Math.max(0, Math.floor((current * 100) / total)))
}

/** Suffix of an `auth.rotation.phase.*` i18n key. Backend phases are an open
 *  vocabulary, so anything unrecognised falls back to a neutral label. */
export type PhaseLabelSuffix =
  | 'preparing'
  | 'rekeying'
  | 'verifying'
  | 'finishing'
  | 'resuming'
  | 'rollingBack'
  | 'working'

export function phaseLabelSuffix(phase: string | null): PhaseLabelSuffix {
  switch (phase) {
    case 'preparing':
      return 'preparing'
    case 'rekeying':
      return 'rekeying'
    case 'verifying':
      return 'verifying'
    case 'finishing':
      return 'finishing'
    case 'resuming':
      return 'resuming'
    case 'rolling_back':
      return 'rollingBack'
    default:
      return 'working'
  }
}

/** Suffix of an `auth.rotation.title.*` i18n key. The kind is known only when
 *  a form in this webview started the rotation (after a reload it is not). */
export type TitleSuffix = 'generic' | 'passwordChange' | 'restore' | 'newPhrase'

export function titleSuffix(kind: RotationKind | null): TitleSuffix {
  switch (kind) {
    case 'password_change':
      return 'passwordChange'
    case 'restore':
      return 'restore'
    case 'new_phrase':
      return 'newPhrase'
    default:
      return 'generic'
  }
}

/** Suffix of an `auth.rotation.outcome.*` i18n key ('none' is never rendered). */
export type OutcomeSuffix = 'complete' | 'rolledBack' | 'failed' | 'none'

export function outcomeSuffix(outcome: RotationOutcome | null): OutcomeSuffix {
  if (outcome === null) return 'none'
  return outcome === 'rolled_back' ? 'rolledBack' : outcome
}

function outcomeOfLast(last: LastRotationOutcome): RotationOutcome | null {
  return last === 'none' ? null : last
}

export interface OverlayState {
  /** A rotation is running right now, by the last event or status read. */
  running: boolean
  phase: string | null
  /** 0 to 100, or null when indeterminate. */
  percent: number | null
  /** A form in this webview started the rotation and shows its own outcome. */
  owned: boolean
  /** The kind that form registered, for the title only. */
  ownerKind: RotationKind | null
  /** An outcome the overlay itself announces (never set while owned). */
  outcome: RotationOutcome | null
}

export const INITIAL_OVERLAY_STATE: OverlayState = {
  running: false,
  phase: null,
  percent: null,
  owned: false,
  ownerKind: null,
  outcome: null,
}

export type OverlayAction =
  | { type: 'event'; payload: TaskProgressPayload }
  | { type: 'status'; status: Pick<RotationStatus, 'running' | 'phase' | 'percent' | 'last_outcome'> }
  | { type: 'owner'; kind: RotationKind | null }
  | { type: 'dismiss' }

export function reduceOverlay(state: OverlayState, action: OverlayAction): OverlayState {
  switch (action.type) {
    case 'event': {
      const { payload } = action
      if (isFinalPhase(payload.phase)) {
        // A live final event is authoritative about how this rotation ended.
        return {
          ...state,
          running: false,
          phase: null,
          percent: null,
          outcome: state.owned ? null : payload.phase,
        }
      }
      return {
        ...state,
        running: true,
        phase: payload.phase,
        percent: percentOf(payload.current, payload.total),
        outcome: null,
      }
    }
    case 'status': {
      const { status } = action
      if (status.running) {
        return {
          ...state,
          running: true,
          phase: status.phase,
          percent: status.percent,
          outcome: null,
        }
      }
      // Idle. last_outcome is only meaningful if we watched it run.
      if (!state.running) return state
      return {
        ...state,
        running: false,
        phase: null,
        percent: null,
        outcome: state.owned ? null : outcomeOfLast(status.last_outcome),
      }
    }
    case 'owner':
      return { ...state, owned: action.kind !== null, ownerKind: action.kind }
    case 'dismiss':
      return { ...state, outcome: null }
  }
}

/** Whether the overlay should be on screen (and hold the blocking layer). */
export function overlayVisible(state: OverlayState): boolean {
  return state.running || state.outcome !== null
}
