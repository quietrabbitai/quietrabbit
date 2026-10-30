// Feeds "final-failure" events into the module-level store (items.id=831).
//
// App-lifetime, NOT detached on a view's unmount, for the reason
// retryWaitListener.ts gives: an event arriving while a pane is remounting
// must not be lost. Installed once and idempotent, so StrictMode's
// mount-unmount-mount and a second pane both call it safely.
import { listen } from '@tauri-apps/api/event'
import { FINAL_FAILURE_EVENT, finalFailureStore, isFinalFailurePayload } from './finalFailure'

let started = false

export function ensureFinalFailureListener(): void {
  if (started) return
  started = true
  listen<unknown>(FINAL_FAILURE_EVENT, (event) => {
    if (isFinalFailurePayload(event.payload)) {
      finalFailureStore.apply(event.payload)
    }
  }).catch(() => {
    // Not listening: a later call may try again. The reply simply has no button.
    started = false
  })
}
