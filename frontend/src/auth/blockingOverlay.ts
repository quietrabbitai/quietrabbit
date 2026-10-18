// Blocking-layer store (items.id=769 stage 1).
//
// A native Cloud Chat provider pane is composited by Rust ABOVE the webview,
// so any DOM layer that must sit on top of the app (the rotation progress
// overlay, the Account security panel) has to make CloudChatAccessPane push an
// empty pane layout while it is open -- the same hide that copyModalOpen
// already triggers for the Privacy Guardian copy modal. This is the one place
// those layers register; CloudChatAccessPane subscribes with
// useSyncExternalStore and folds the result into its "hide the panes" flag.
//
// Holders are counted, not boolean, so the overlay and the panel can both be
// open and the panes only come back when the last one lets go.

const holders = new Set<symbol>()
const listeners = new Set<() => void>()

/** Register a blocking layer. Returns an idempotent release function. */
export function acquireBlockingOverlay(): () => void {
  const token = Symbol('blocking-overlay')
  holders.add(token)
  notify()
  return () => {
    if (holders.delete(token)) notify()
  }
}

export function isBlockingOverlayActive(): boolean {
  return holders.size > 0
}

export function subscribeBlockingOverlay(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

function notify(): void {
  for (const listener of [...listeners]) listener()
}
