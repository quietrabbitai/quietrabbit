// Pure logic behind the "waiting to retry" row (items.id=817, decisions.id=923).
// No React, no Tauri: retryWaitListener.ts feeds events into the store below,
// useRetryWait.ts reads it, and every rule here runs on plain Node in
// retryWait.test.ts.
//
// The "retry-wait" payload is NOT in bindings.ts (tauri-specta exports only
// command and type signatures, and the event is emitted straight from
// src-tauri/src/retry_wait.rs), so it is typed by hand here to match
// RetryWaitPayload in that file, like RunStatusPayload in ChatPane.tsx.
//
// WHAT THE ROW SHOWS (decisions.id=923): what (the Focus and step), where (the
// provider and model), why (busy, slow, unavailable) and when ('Trying again
// in N seconds · Retry k of 3', a live local countdown). Names for the provider
// come from the provider row as data; nothing here names a provider
// (decisions.id=872, 875).
//
// THE STORE IS MODULE-LEVEL, keyed by run id. A wait event can arrive before
// the pane knows the run id (the event races the send command's response) or
// while the pane is unmounted and remounted; component state would lose it.
// Here it is kept until its own deadline, and a late subscriber still sees the
// row.

/** A wait shorter than this is not shown (a placeholder value, decisions.id=923). */
export const MIN_VISIBLE_WAIT_SECS = 2
/** A row whose clear event was dropped (a dropped emit is swallowed by design)
 *  removes itself this long after its own deadline. */
export const SELF_CLEAR_GRACE_MS = 3000
/** A run with no event for this long is forgotten (keeps the store bounded). */
export const STALE_RUN_MS = 30 * 60 * 1000

export const RETRY_WAIT_EVENT = 'retry-wait'

export type RetryWaitReason = 'busy' | 'slow' | 'unavailable'
export type RetryWaitEventState = 'waiting' | 'clear'

/** Hand-typed to match src-tauri/src/retry_wait.rs::RetryWaitPayload. */
export interface RetryWaitPayload {
  focus_run_id: string
  focus_name: string
  step_display_name: string
  provider_id: string
  model_id: string
  reason: RetryWaitReason
  /** The TOTAL wait, whole seconds rounded up. */
  seconds: number
  /** 1-based number of the retry this wait comes before. */
  attempt: number
  max_attempts: number
  state: RetryWaitEventState
}

const REASONS: readonly string[] = ['busy', 'slow', 'unavailable']

/** Narrowing guard for the untyped event payload. */
export function isRetryWaitPayload(value: unknown): value is RetryWaitPayload {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Record<string, unknown>
  const num = (n: unknown) => typeof n === 'number' && Number.isFinite(n)
  return (
    typeof v.focus_run_id === 'string' &&
    typeof v.focus_name === 'string' &&
    typeof v.step_display_name === 'string' &&
    typeof v.provider_id === 'string' &&
    typeof v.model_id === 'string' &&
    typeof v.reason === 'string' &&
    REASONS.includes(v.reason) &&
    num(v.seconds) &&
    num(v.attempt) &&
    num(v.max_attempts) &&
    (v.state === 'waiting' || v.state === 'clear')
  )
}

export interface ActiveWait {
  /** The local time the event arrived: the deadline is measured from here, so
   *  no clock of the backend's matters. */
  receivedAtMs: number
  seconds: number
  attempt: number
  maxAttempts: number
  reason: RetryWaitReason
  providerId: string
  modelId: string
  focusName: string
  stepDisplayName: string
}

export interface RunWaitState {
  runId: string
  /** A wait of MIN_VISIBLE_WAIT_SECS or more has been shown in this run, so a
   *  later shorter one keeps the row up as 'Trying again…' instead of flickering. */
  shownBefore: boolean
  wait: ActiveWait | null
  touchedAtMs: number
}

/** The state after an event. Returns `prev` itself when nothing changed, so a
 *  subscriber is not woken for nothing. */
export function applyRetryWaitEvent(
  prev: RunWaitState | undefined,
  p: RetryWaitPayload,
  nowMs: number,
): RunWaitState | undefined {
  if (p.state === 'clear') {
    // A clear ends the wait it names. One for an older attempt than the wait
    // on hand (a late clear after the next wait began) leaves it alone.
    if (!prev || !prev.wait || p.attempt < prev.wait.attempt) return prev
    return { ...prev, wait: null, touchedAtMs: nowMs }
  }
  return {
    runId: p.focus_run_id,
    shownBefore: (prev?.shownBefore ?? false) || p.seconds >= MIN_VISIBLE_WAIT_SECS,
    wait: {
      receivedAtMs: nowMs,
      seconds: p.seconds,
      attempt: p.attempt,
      maxAttempts: p.max_attempts,
      reason: p.reason,
      providerId: p.provider_id,
      modelId: p.model_id,
      focusName: p.focus_name,
      stepDisplayName: p.step_display_name,
    },
    touchedAtMs: nowMs,
  }
}

export type RetryWaitMode = 'countdown' | 'again'

export interface RetryWaitRow {
  runId: string
  /** 'countdown': 'Trying again in N seconds'. 'again': 'Trying again…', for a
   *  later wait too short to count down and for the moment between the
   *  countdown reaching 0 and the retry starting. */
  mode: RetryWaitMode
  secondsLeft: number
  attempt: number
  maxAttempts: number
  reason: RetryWaitReason
  providerId: string
  modelId: string
  focusName: string
  stepDisplayName: string
  /** Changes once per wait and never per tick: a screen reader announces the
   *  row when this changes, so it is announced once. */
  announcementKey: string
}

/** The row to show for a run at `nowMs`, or null for none. */
export function retryWaitRow(state: RunWaitState | undefined, nowMs: number): RetryWaitRow | null {
  const wait = state?.wait
  if (!state || !wait) return null
  const deadlineMs = wait.receivedAtMs + wait.seconds * 1000
  // The self-clear for a clear event that never arrived.
  if (nowMs >= deadlineMs + SELF_CLEAR_GRACE_MS) return null
  const longEnough = wait.seconds >= MIN_VISIBLE_WAIT_SECS
  if (!longEnough && !state.shownBefore) return null
  const secondsLeft = Math.max(0, Math.ceil((deadlineMs - nowMs) / 1000))
  return {
    runId: state.runId,
    mode: longEnough && secondsLeft >= 1 ? 'countdown' : 'again',
    secondsLeft,
    attempt: wait.attempt,
    maxAttempts: wait.maxAttempts,
    reason: wait.reason,
    providerId: wait.providerId,
    modelId: wait.modelId,
    focusName: wait.focusName,
    stepDisplayName: wait.stepDisplayName,
    announcementKey: `${state.runId}:${wait.attempt}`,
  }
}

/** Module-level store of wait state by run id; `useSyncExternalStore` shaped. */
export class RetryWaitStore {
  private runs = new Map<string, RunWaitState>()
  private listeners = new Set<() => void>()

  apply(p: RetryWaitPayload, nowMs: number): void {
    const prev = this.runs.get(p.focus_run_id)
    const next = applyRetryWaitEvent(prev, p, nowMs)
    let changed = false
    if (next !== prev && next) {
      this.runs.set(p.focus_run_id, next)
      changed = true
    }
    for (const [id, s] of this.runs) {
      if (nowMs - s.touchedAtMs > STALE_RUN_MS) {
        this.runs.delete(id)
        changed = true
      }
    }
    if (changed) this.emit()
  }

  /** The same object until the run's state changes. */
  get = (runId: string): RunWaitState | undefined => this.runs.get(runId)

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  forget(runId: string): void {
    if (this.runs.delete(runId)) this.emit()
  }

  get size(): number {
    return this.runs.size
  }

  private emit(): void {
    for (const l of [...this.listeners]) l()
  }
}

/** The one store the app uses. */
export const retryWaitStore = new RetryWaitStore()

type CancelRunResult = { status: 'ok' } | { status: 'error'; error: string }

/** The Cancel of a QR Chat bubble's row: ends the run through `cancel_run`
 *  (items.id=816) and nothing else. Resolves to the error text, or null when
 *  the run was cancelled. */
export async function cancelRetryWait(
  cancelRun: (runId: string, userId: string, personaId: string) => Promise<CancelRunResult>,
  runId: string,
  userId: string,
  personaId: string,
): Promise<string | null> {
  const r = await cancelRun(runId, userId, personaId)
  return r.status === 'ok' ? null : r.error
}
