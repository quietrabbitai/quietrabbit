// App root -- login gate (items.id=267). Every cold launch starts with an
// empty KeyRegistry (master key is never persisted, CLAUDE.md), so
// commands.getSession() at mount will always resolve to null on a fresh
// process; this still runs the real check rather than assuming that, since
// it's the same call NavShell's own tree relies on being accurate.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from './bindings'
import { LoginForm } from './auth/LoginForm'
import { NavShell } from './navShell/NavShell'
import { setCurrentUserId } from './navShell/navShellConfig'

type BootState = 'checking' | 'loginRequired' | 'loggedIn'

function App() {
  const { t } = useTranslation()
  const [bootState, setBootState] = useState<BootState>('checking')
  const [sessionCheckError, setSessionCheckError] = useState<string | null>(
    null,
  )

  useEffect(() => {
    commands.getSession().then((result) => {
      if (result.status === 'ok' && result.data !== null) {
        setCurrentUserId(result.data.user_id)
        setBootState('loggedIn')
        return
      }
      if (result.status !== 'ok') {
        // A query failure at boot shouldn't hard-block the app -- login is
        // a reasonable fallback, and a real backend problem will resurface
        // when login() itself is attempted. Kept visible rather than
        // swallowed, for debuggability.
        setSessionCheckError(result.error)
      }
      setBootState('loginRequired')
    })
  }, [])

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

  if (bootState === 'checking') {
    return <p>{t('auth.checkingSession')}</p>
  }

  if (bootState === 'loginRequired') {
    return (
      <>
        {sessionCheckError && (
          <p role="alert">
            {t('auth.loginError', { message: sessionCheckError })}
          </p>
        )}
        <LoginForm onLoggedIn={() => setBootState('loggedIn')} />
      </>
    )
  }

  return <NavShell />
}

export default App
