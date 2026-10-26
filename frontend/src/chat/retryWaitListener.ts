// Feeds "retry-wait" events into the module-level store (items.id=817).
//
// Deliberately app-lifetime, NOT detached on a view's unmount (the usual
// CLAUDE.md rule is for view-scoped listeners): the store exists so that an
// event arriving before a pane knows its run id, or while a pane is remounting,
// is not lost. The listener is installed once and is idempotent, so StrictMode's
// mount-unmount-mount and a second pane both call it safely.
import { listen } from '@tauri-apps/api/event'
import { isRetryWaitPayload, retryWaitStore, RETRY_WAIT_EVENT } from './retryWait'

let started = false

export function ensureRetryWaitListener(): void {
  if (started) return
  started = true
  listen<unknown>(RETRY_WAIT_EVENT, (event) => {
    if (isRetryWaitPayload(event.payload)) {
      retryWaitStore.apply(event.payload, Date.now())
    }
  }).catch(() => {
    // Not listening: a later call may try again. The row simply does not show.
    started = false
  })
}
