// The tail every sign-in path shares (items.id=769 stage 2, extracted from
// LoginForm.performLogin): after login() or restoreWithRecoveryPhrase() has
// opened a session, ask the backend who is signed in and hand that id to
// navShellConfig. login() and the restore return no identity themselves
// (CLAUDE.md: key and session state never cross IPC), so this follow-up read
// is how the frontend learns the user id. The caller then calls its own
// onLoggedIn().

import { commands } from '../bindings'
import { setCurrentUserId } from '../navShell/navShellConfig'

export type FinishSignInResult =
  | { status: 'ok' }
  | { status: 'error'; message: string }
  /** Should be impossible right after a successful sign-in; surfaced
   *  distinctly rather than retried, as commands/auth.rs does for its own
   *  "should be impossible" cases. */
  | { status: 'sessionMissing' }

export async function finishSignIn(): Promise<FinishSignInResult> {
  const sessionResult = await commands.getSession()
  if (sessionResult.status !== 'ok') {
    return { status: 'error', message: sessionResult.error }
  }
  if (sessionResult.data === null) {
    return { status: 'sessionMissing' }
  }
  setCurrentUserId(sessionResult.data.user_id)
  return { status: 'ok' }
}
