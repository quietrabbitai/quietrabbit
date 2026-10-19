// Returning to the login screen when the backend session ends (items.id=775).
//
// The backend emits `session-locked` with `{ reason }` from every lock path:
// idle expiry, the 30-day ceiling, a key-epoch change, an unverifiable
// session, and logout (src-tauri/src/auth/idle_timeout.rs, LockReason). Jason's
// ruling: when the backend session ends the app always shows the login
// screen; it never stays signed-in-looking over a locked backend.
//
// Everything decidable lives here as plain functions so it runs under
// `node --experimental-strip-types` (sessionLock.test.ts); App.tsx only wires
// them to React state and the Tauri listener.

import type { SignOutNotice } from './rotationErrors.ts'

export const SESSION_LOCKED_EVENT = 'session-locked'

export type BootState = 'checking' | 'loginRequired' | 'loggedIn'

/** Shown on the login screen after the backend ended the session. `lockReason`
 *  is the backend's wire string; a string this file does not know still gets
 *  generic copy, so a reason added later never leaves the user signed in. */
export interface SessionLockNotice {
  lockReason: string
}

/** Narrowing guard for the untyped event payload: `{ reason: string }`, as
 *  hand-typed from src-tauri/src/auth/idle_timeout.rs. Returns the reason, or
 *  null for anything else (which is ignored: a malformed event is not a lock). */
export function parseSessionLocked(payload: unknown): string | null {
  if (typeof payload !== 'object' || payload === null) return null
  const reason = (payload as Record<string, unknown>).reason
  return typeof reason === 'string' && reason.length > 0 ? reason : null
}

export type BootEvent =
  | { type: 'session-locked'; reason: string }
  /** The boot-time getSession() answer. */
  | { type: 'boot-session'; present: boolean }
  | { type: 'login-succeeded' }
  | { type: 'signed-out' }

export function nextBootState(state: BootState, event: BootEvent): BootState {
  switch (event.type) {
    case 'session-locked':
    case 'signed-out':
      return 'loginRequired'
    case 'login-succeeded':
      return 'loggedIn'
    case 'boot-session':
      // Only the first answer decides; a late one must not undo a lock that
      // arrived while the check was in flight.
      return state === 'checking' ? (event.present ? 'loggedIn' : 'loginRequired') : state
  }
}

/** What the listener should do once it has attached, closing the gap between
 *  the boot getSession() answer and the listener existing: a lock that fired
 *  in that gap emitted to nobody. `session` is a fresh getSession() answer
 *  (null when the check itself failed, which proves nothing either way). */
export function recheckAfterListen(
  state: BootState,
  session: 'present' | 'absent' | null,
): BootEvent | null {
  if (state === 'loggedIn' && session === 'absent') {
    return { type: 'session-locked', reason: 'no_session' }
  }
  return null
}

/** The notice to show after a lock. Logout leaves whatever notice is already
 *  there (an explicit sign-out flow supplies its own); every other reason sets
 *  a lock notice. */
export function noticeAfterLock(
  current: SignOutNotice | null,
  reason: string,
): SignOutNotice | null {
  return reason === 'logged_out' ? current : { lockReason: reason }
}

const COPY_SUFFIX: Record<string, string> = {
  idle: 'idle',
  session_expired: 'expired',
  key_epoch_changed: 'keyEpochChanged',
}
const GENERIC_SUFFIX = 'generic'

/** Leaf under `auth.sessionLocked.` for a reason. no_session, check_failed and
 *  any reason this file does not know share the generic copy. */
export function lockNoticeSuffix(reason: string): string {
  return Object.hasOwn(COPY_SUFFIX, reason) ? COPY_SUFFIX[reason] : GENERIC_SUFFIX
}

/** Every suffix lockNoticeSuffix can return, for the test that checks en.json. */
export function allLockNoticeSuffixes(): string[] {
  return [...new Set([...Object.values(COPY_SUFFIX), GENERIC_SUFFIX])].sort()
}

export function isSessionLockNotice(notice: SignOutNotice): notice is SessionLockNotice {
  return 'lockReason' in notice
}
