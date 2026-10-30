// Pure logic behind the final-failure notice (items.id=831, decisions.id=925).
// No React, no Tauri: finalFailureListener.ts feeds events into the store
// below, useFinalFailure.ts reads it, and every rule here runs on plain Node in
// finalFailure.test.ts.
//
// The "final-failure" payload is NOT in bindings.ts (tauri-specta exports only
// command and type signatures, and the event is emitted straight from
// src-tauri/src/final_failure.rs), so it is typed by hand here to match
// FinalFailurePayload in that file, like RetryWaitPayload in retryWait.ts.
//
// WHAT THE EVENT IS FOR. The failure sentence itself is the stored reply (the
// backend writes it into the reply row, naming the provider from its row), so
// nothing here builds or translates it. The event only says WHICH reply is a
// final failure, so the pane can offer Try again on it. It is kept in memory:
// after an app restart the sentence is still there but the button is not (the
// reply row has no place to keep the marker without a schema change).

export const FINAL_FAILURE_EVENT = 'final-failure'

export type FinalFailureReason = 'busy' | 'slow' | 'unavailable'

/** Hand-typed to match src-tauri/src/final_failure.rs::FinalFailurePayload. */
export interface FinalFailurePayload {
  focus_run_id: string
  message_id: string
  reason: FinalFailureReason
  provider_id: string
  model_id: string
}

const REASONS: readonly string[] = ['busy', 'slow', 'unavailable']

/** Narrowing guard for the untyped event payload. */
export function isFinalFailurePayload(value: unknown): value is FinalFailurePayload {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Record<string, unknown>
  return (
    typeof v.focus_run_id === 'string' &&
    typeof v.message_id === 'string' &&
    typeof v.reason === 'string' &&
    REASONS.includes(v.reason) &&
    typeof v.provider_id === 'string' &&
    typeof v.model_id === 'string'
  )
}

/** Most runs remembered; the oldest is dropped past this (keeps the store bounded). */
export const MAX_REMEMBERED_FAILURES = 100

/** Module-level store of final failures by run id; `useSyncExternalStore` shaped. */
export class FinalFailureStore {
  private byRun = new Map<string, FinalFailurePayload>()
  private listeners = new Set<() => void>()
  private revision = 0

  apply(p: FinalFailurePayload): void {
    // A newer failure of the same run replaces the older one and counts as the
    // newest (re-inserted last).
    this.byRun.delete(p.focus_run_id)
    this.byRun.set(p.focus_run_id, p)
    while (this.byRun.size > MAX_REMEMBERED_FAILURES) {
      const oldest = this.byRun.keys().next().value
      if (oldest === undefined) break
      this.byRun.delete(oldest)
    }
    this.revision += 1
    this.emit()
  }

  /** Forget a run's failure (Try again has taken it up; a new final failure
   *  of the run announces itself again). */
  clear(runId: string): void {
    if (this.byRun.delete(runId)) {
      this.revision += 1
      this.emit()
    }
  }

  /** The failure of a run, or undefined. */
  get = (runId: string | null | undefined): FinalFailurePayload | undefined =>
    runId ? this.byRun.get(runId) : undefined

  /** Changes whenever the store does; the snapshot `useSyncExternalStore` reads. */
  version = (): number => this.revision

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  private emit(): void {
    for (const l of this.listeners) l()
  }
}

export const finalFailureStore = new FinalFailureStore()

/** The reply row, if any, that carries the notice: a failed reply (is_error)
 *  that the backend announced as this run's final failure. */
export function noticeFor(
  row: { id: string; is_error: boolean; focus_run_id: string | null },
  store: Pick<FinalFailureStore, 'get'>,
): FinalFailurePayload | undefined {
  if (!row.is_error) return undefined
  const found = store.get(row.focus_run_id)
  return found && found.message_id === row.id ? found : undefined
}

/** What Try again leaves behind in the reply row while the resumed run works:
 *  the failure sentence, which must not be mistaken for the new reply. */
export interface ResumeBaseline {
  runId: string
  staleContent: string
}

/** Whether a reply row's content is a reply to show for `runId`: not empty and
 *  not the failure text a Try again is still replacing. A resumed run that
 *  fails the same way again writes the same text; that case is finalized by
 *  the run's message-content-ready event, which does not look at the text. */
export function isFreshReply(
  content: string,
  runId: string,
  baseline: ResumeBaseline | null,
): boolean {
  if (!content) return false
  return !(baseline !== null && baseline.runId === runId && baseline.staleContent === content)
}
