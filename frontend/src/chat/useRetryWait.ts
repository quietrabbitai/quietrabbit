// The shared hook behind every "waiting to retry" surface (items.id=817,
// decisions.id=923): the QR Chat bubble today, the API Chat pane, the minimized
// pane's dock bar and the Other chats bar count later (items.id=743, 744, 745).
// It reads the module-level store in retryWait.ts, so a surface needs no
// backend work of its own, only a run id.
//
// What a surface still owns: its own Cancel (cancel_run for a Focus run,
// cancel_chat_stream for API Chat) and its layout.
import { useEffect, useState, useSyncExternalStore } from 'react'
import { commands } from '../bindings'
import { retryWaitRow, retryWaitStore, type RetryWaitRow } from './retryWait'
import { ensureRetryWaitListener } from './retryWaitListener'

/** How often a visible row re-reads the clock. Finer than a second so the
 *  displayed number never lags the true one by more than a quarter second. */
const TICK_MS = 250

/** provider id -> display name, from the provider row (data, never code). */
const providerNames = new Map<string, string>()
const providerNamesRequested = new Set<string>()

export interface UseRetryWait {
  /** The row to show now, or null. */
  row: RetryWaitRow | null
  /** The provider's display name, or null until known (or if it cannot be read). */
  providerName: string | null
}

/** `runId` is the run this surface shows (null = none). */
export function useRetryWait(runId: string | null): UseRetryWait {
  useEffect(() => {
    ensureRetryWaitListener()
  }, [])

  const state = useSyncExternalStore(retryWaitStore.subscribe, () =>
    runId ? retryWaitStore.get(runId) : undefined,
  )
  const [now, setNow] = useState(() => Date.now())
  // An event may be newer than the last tick (or than this mount): never show
  // a time before the wait began.
  const effectiveNow = state?.wait ? Math.max(now, state.wait.receivedAtMs) : now
  const row = retryWaitRow(state, effectiveNow)
  const visible = row !== null

  useEffect(() => {
    if (!visible) return
    const id = window.setInterval(() => setNow(Date.now()), TICK_MS)
    return () => window.clearInterval(id)
  }, [visible])

  const providerId = row?.providerId ?? null
  const modelId = row?.modelId ?? null
  const [, setNamesVersion] = useState(0)
  useEffect(() => {
    if (!providerId || providerNamesRequested.has(providerId)) return
    providerNamesRequested.add(providerId)
    commands
      .getProviderDisclosure(providerId, modelId, false)
      .then((r) => {
        if (r.status === 'ok' && r.data.display_name) {
          providerNames.set(providerId, r.data.display_name)
          setNamesVersion((v) => v + 1)
        } else {
          // Try again next time a row shows; the row works without a name.
          providerNamesRequested.delete(providerId)
        }
      })
      .catch(() => providerNamesRequested.delete(providerId))
  }, [providerId, modelId])

  return { row, providerName: providerId ? (providerNames.get(providerId) ?? null) : null }
}
