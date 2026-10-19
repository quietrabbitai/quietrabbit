// App root -- login gate (items.id=267). Every cold launch starts with an
// empty KeyRegistry (master key is never persisted, CLAUDE.md), so
// commands.getSession() at mount will always resolve to null on a fresh
// process; this still runs the real check rather than assuming that, since
// it's the same call NavShell's own tree relies on being accurate.

import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from './bindings'
import { LoginForm } from './auth/LoginForm'
import { RotationErrorText } from './auth/RotationErrorText'
import { RotationProgress } from './auth/RotationProgress'
import { SessionLockText } from './auth/SessionLockText'
import { WriteFailedNotice } from './auth/WriteFailedNotice'
import { addFailure, parseWriteFailed, WRITE_FAILED_EVENT } from './auth/writeFailed'
import type { SignOutNotice } from './auth/rotationErrors'
import {
  isSessionLockNotice,
  nextBootState,
  noticeAfterLock,
  parseSessionLocked,
  recheckAfterListen,
  SESSION_LOCKED_EVENT,
  type BootEvent,
  type BootState,
} from './auth/sessionLock'
import { NavShell } from './navShell/NavShell'
import { setCurrentUserId } from './navShell/navShellConfig'

function App() {
  const { t } = useTranslation()
  const [bootState, setBootState] = useState<BootState>('checking')
  // Read by the listener's post-attach re-check, which runs outside render.
  const bootStateRef = useRef<BootState>(bootState)
  useEffect(() => {
    bootStateRef.current = bootState
  }, [bootState])
  const dispatchBoot = useCallback((event: BootEvent) => {
    setBootState((state) => nextBootState(state, event))
  }, [])
  const [sessionCheckError, setSessionCheckError] = useState<string | null>(
    null,
  )

  // items.id=769: why the user was just signed out, shown on the login screen.
  const [signOutNotice, setSignOutNotice] = useState<SignOutNotice | null>(null)

  // items.id=778: a write of the user's own content failed after its retries
  // (a reply, a run's final status, a History entry). Shown until dismissed.
  const [writeFailures, setWriteFailures] = useState<string[]>([])
  useEffect(() => {
    let cancelled = false
    let unlisten: UnlistenFn | undefined
    listen<unknown>(WRITE_FAILED_EVENT, (event) => {
      const kind = parseWriteFailed(event.payload)
      if (kind !== null) setWriteFailures((current) => addFailure(current, kind))
    })
      .then((fn) => {
        if (cancelled) fn()
        else unlisten = fn
      })
      .catch(() => {
        // No Tauri bridge (plain browser): nothing emits.
      })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [])

  // Sign out (explicit, or forced by a credential-change error). logout() clears
  // the resident key; the cached user id goes with it. A failed logout() still
  // returns to the login screen -- showing the error there -- because the
  // alternative is a signed-in-looking app the user asked to leave.
  const signOut = useCallback(async (notice?: SignOutNotice) => {
    const result = await commands.logout()
    setCurrentUserId(null)
    setSignOutNotice(notice ?? null)
    setSessionCheckError(result.status === 'ok' ? null : result.error)
    dispatchBoot({ type: 'signed-out' })
  }, [dispatchBoot])

  // items.id=775: the backend ended the session (idle, the 30-day ceiling, a
  // key-epoch change, logout). Jason's ruling: always return to the login
  // screen, with the reason, rather than stay signed-in-looking over a locked
  // backend. A logout this webview asked for keeps the notice signOut() sets.
  const applyLock = useCallback(
    (reason: string) => {
      setCurrentUserId(null)
      setSignOutNotice((current) => noticeAfterLock(current, reason))
      if (reason !== 'logged_out') {
        setSessionCheckError(null)
      }
      dispatchBoot({ type: 'session-locked', reason })
    },
    [dispatchBoot],
  )

  // One listener for the life of the app. The cancelled flag covers React
  // StrictMode's mount-unmount-mount: listen() resolves asynchronously, so a
  // cleanup that runs first must detach the listener the moment it arrives.
  // Once attached, getSession() is asked again: a lock that fired between the
  // boot answer and this point emitted to nobody.
  useEffect(() => {
    let cancelled = false
    let unlisten: UnlistenFn | undefined
    listen<unknown>(SESSION_LOCKED_EVENT, (event) => {
      const reason = parseSessionLocked(event.payload)
      if (reason !== null) applyLock(reason)
    })
      .then((fn) => {
        if (cancelled) {
          fn()
          return
        }
        unlisten = fn
        return commands.getSession().then((result) => {
          if (cancelled || result.status !== 'ok') return
          const recheck = recheckAfterListen(
            bootStateRef.current,
            result.data === null ? 'absent' : 'present',
          )
          if (recheck?.type === 'session-locked') applyLock(recheck.reason)
        })
      })
      .catch(() => {
        // No Tauri bridge (plain browser): nothing emits, nothing to do.
      })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [applyLock])

  useEffect(() => {
    commands.getSession().then((result) => {
      if (result.status === 'ok' && result.data !== null) {
        setCurrentUserId(result.data.user_id)
        dispatchBoot({ type: 'boot-session', present: true })
        return
      }
      if (result.status !== 'ok') {
        // A query failure at boot shouldn't hard-block the app -- login is
        // a reasonable fallback, and a real backend problem will resurface
        // when login() itself is attempted. Kept visible rather than
        // swallowed, for debuggability.
        setSessionCheckError(result.error)
      }
      dispatchBoot({ type: 'boot-session', present: false })
    })
  }, [dispatchBoot])

  // items.id=311: idle-timeout activity signal. Frontend-driven rather than
  // bumped on every IPC command -- see auth::idle_timeout's own module
  // header (backend) for why: there's no central command guard to hook,
  // and mouse/keyboard activity is the more correct proxy for "someone is
  // actually at the keyboard" than "a command fired" (a long-running
  // backend call shouldn't reset the idle clock while the user has
  // genuinely stepped away). Debounced to one ping per interval rather
  // than one per event -- a dirty flag set by the listeners, drained by
  // the interval, so a burst of mousemove events costs one IPC call, not
  // hundreds. 60s matches the backend timer's own check granularity
  // (main.rs) -- pinging more often than the backend checks would just be
  // wasted round-trips.
  useEffect(() => {
    if (bootState !== 'loggedIn') {
      return
    }

    let dirty = false
    const markDirty = () => {
      dirty = true
    }
    const events = ['mousemove', 'keydown', 'click', 'scroll'] as const
    events.forEach((event) => window.addEventListener(event, markDirty))

    const interval = window.setInterval(() => {
      if (!dirty) {
        return
      }
      dirty = false
      void commands.recordActivity()
    }, 60_000)

    return () => {
      events.forEach((event) => window.removeEventListener(event, markDirty))
      window.clearInterval(interval)
    }
  }, [bootState])

  // items.id=542: WebKitGTK (the outer Tauri webview) shows its own default
  // context menu (Inspect Element, etc.) on any right-click that isn't
  // already handled -- PaneHitLayer/PopupHitLayer suppress it for clicks
  // inside a CEF pane/popup, but nothing did for the rest of the app.
  // preventDefault() here is a harmless no-op on events already handled by
  // those two (they don't stopPropagation), and doesn't touch the separate
  // pointer-forwarding path that sends right-clicks into CEF. Not gated on
  // bootState -- the login screen is in scope too.
  useEffect(() => {
    if (import.meta.env.DEV) {
      return
    }
    const handleContextMenu = (event: MouseEvent) => {
      event.preventDefault()
    }
    window.addEventListener('contextmenu', handleContextMenu)
    return () => window.removeEventListener('contextmenu', handleContextMenu)
  }, [])

  // items.id=769: the key-rotation progress overlay is mounted here, beside
  // every boot state, not inside NavShell -- login() and the recovery-phrase
  // restore both run rotations with no session, and a webview that reloads
  // mid-rotation must find the overlay before it finds anything else.
  let screen
  if (bootState === 'checking') {
    screen = <p>{t('auth.checkingSession')}</p>
  } else if (bootState === 'loginRequired') {
    screen = (
      <>
        {sessionCheckError && (
          <p role="alert">
            {t('auth.loginError', { message: sessionCheckError })}
          </p>
        )}
        {signOutNotice &&
          (isSessionLockNotice(signOutNotice) ? (
            <SessionLockText reason={signOutNotice.lockReason} />
          ) : (
            <RotationErrorText error={signOutNotice.error} context={signOutNotice.context} />
          ))}
        <LoginForm
          onLoggedIn={() => {
            setSignOutNotice(null)
            dispatchBoot({ type: 'login-succeeded' })
          }}
        />
      </>
    )
  } else {
    screen = <NavShell onSignOut={(notice) => void signOut(notice)} />
  }

  return (
    <>
      {screen}
      <RotationProgress onFailedAcknowledged={() => void signOut()} />
      <WriteFailedNotice kinds={writeFailures} onDismiss={() => setWriteFailures([])} />
    </>
  )
}

export default App
