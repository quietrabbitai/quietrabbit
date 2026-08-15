// Login/bootstrap form -- items.id=267. The only entry point into a real
// session: commands.login() branches transparently server-side on
// has_any_users() (commands/auth.rs), creating the primary admin account on
// a fresh install or authenticating an existing one -- this form has no way
// to know in advance which case it is, and doesn't need to; one plain form
// serves both.
//
// login() itself returns null on success (CLAUDE.md: master key/session
// state never cross IPC), so a successful submit immediately follows up
// with commands.getSession() to learn the resulting user_id, then hands it
// to navShellConfig.ts's setCurrentUserId() before calling onLoggedIn().

import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from '../bindings'
import { setCurrentUserId } from '../navShell/navShellConfig'

export interface LoginFormProps {
  onLoggedIn: () => void
}

export function LoginForm({ onLoggedIn }: LoginFormProps) {
  const { t } = useTranslation()
  const [displayName, setDisplayName] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    setError(null)
    setSubmitting(true)

    const loginResult = await commands.login(displayName, password)
    if (loginResult.status !== 'ok') {
      setError(loginResult.error)
      setSubmitting(false)
      return
    }

    const sessionResult = await commands.getSession()
    if (sessionResult.status !== 'ok') {
      setError(sessionResult.error)
      setSubmitting(false)
      return
    }
    if (sessionResult.data === null) {
      // Should be impossible immediately after a successful login() --
      // surfaced distinctly rather than silently retried, same discipline
      // commands/auth.rs uses for its own "should be impossible" cases.
      setError(t('auth.sessionMissingAfterLogin'))
      setSubmitting(false)
      return
    }

    setCurrentUserId(sessionResult.data.user_id)
    onLoggedIn()
  }

  return (
    <form className="login-form" onSubmit={handleSubmit}>
      <div>
        <label htmlFor="login-display-name">
          {t('auth.displayNameLabel')}
        </label>
        <input
          id="login-display-name"
          type="text"
          value={displayName}
          onChange={(event) => setDisplayName(event.target.value)}
          autoComplete="username"
          required
        />
      </div>

      <div>
        <label htmlFor="login-password">{t('auth.passwordLabel')}</label>
        <input
          id="login-password"
          type="password"
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          autoComplete="current-password"
          required
        />
      </div>

      <button type="submit" disabled={submitting}>
        {t('auth.submitButton')}
      </button>

      {error && <p role="alert">{t('auth.loginError', { message: error })}</p>}
    </form>
  )
}
