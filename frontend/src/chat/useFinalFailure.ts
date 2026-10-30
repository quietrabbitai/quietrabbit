// The hook behind the final-failure notice (items.id=831): subscribes a pane to
// the module-level store in finalFailure.ts and installs the event listener.
import { useEffect, useSyncExternalStore } from 'react'
import { finalFailureStore, type FinalFailurePayload } from './finalFailure'
import { ensureFinalFailureListener } from './finalFailureListener'

/** Returns the lookup of a run's final failure. The pane re-renders when any
 *  failure arrives, so calling it during render is always current. */
export function useFinalFailures(): (
  runId: string | null | undefined,
) => FinalFailurePayload | undefined {
  useEffect(() => {
    ensureFinalFailureListener()
  }, [])
  useSyncExternalStore(finalFailureStore.subscribe, finalFailureStore.version)
  return finalFailureStore.get
}
