// Who started the rotation that is running (items.id=769 stage 1).
//
// A form that calls changePassword / restoreWithRecoveryPhrase /
// confirmNewRecoveryPhrase registers here first. While registered, that form
// is the one that tells the user how it ended (it gets the coded
// KeyRotationError and knows which kind of change it was); the progress
// overlay then shows progress only. With nobody registered -- a webview that
// reloaded mid-rotation -- the overlay reports the outcome itself, from
// getRotationStatus, in generic wording.

import type { RotationKind } from './rotationProgress.ts'

let current: RotationKind | null = null
let owner: symbol | null = null
const listeners = new Set<() => void>()

/** Register as the starter of a rotation. Returns an idempotent release that
 *  only clears the registration if it is still this caller's. */
export function beginRotationTask(kind: RotationKind): () => void {
  const mine = Symbol('rotation-task')
  owner = mine
  current = kind
  notify()
  return () => {
    if (owner === mine) {
      owner = null
      current = null
      notify()
    }
  }
}

export function getRotationTask(): RotationKind | null {
  return current
}

export function subscribeRotationTask(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

function notify(): void {
  for (const listener of [...listeners]) listener()
}
