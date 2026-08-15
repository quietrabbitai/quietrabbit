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
