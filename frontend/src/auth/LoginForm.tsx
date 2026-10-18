// Login/bootstrap form -- items.id=267. The only entry point into a real
// session: commands.login() branches transparently server-side on
// has_any_users() (commands/auth.rs), creating the primary admin account on
// a fresh install or authenticating an existing one -- this form has no way
// to know in advance which case it is, and doesn't need to; one plain form
// serves both.
//
// login() itself returns null on success (CLAUDE.md: master key/session
// state never cross IPC), so a successful submit immediately follows up
// with finishSignIn() (commands.getSession() plus navShellConfig.ts's
// setCurrentUserId()) before calling onLoggedIn().
//
// items.id=769 stage 4: this screen also hosts the two credential-recovery
// views. "Forgot your password?" switches to RestoreAccessForm (recovery
// phrase + a new password), and the login() answer "rotation_interrupted" (the
// OLD password typed while a change is pending) switches to
// InterruptedChangeScreen. login() answers are plain strings; classifyLoginError
// (rotationErrors.ts) recognises the few that have their own wording and
// everything else keeps the original "Could not log in: ..." message. The
// password field is cleared on every submit. login() registers as the
// rotation's starter (rotationTask) because typing the new password resumes an
// interrupted rotation inside it: the form reports the outcome, and the
// progress overlay (mounted above this screen) shows the progress.

import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands } from '../bindings'
import { finishSignIn } from './finishSignIn'
import { InterruptedChangeScreen } from './InterruptedChangeScreen'
import { RestoreAccessForm } from './RestoreAccessForm'
import { classifyLoginError, loginErrorSuffix, type LoginProblem } from './rotationErrors'
import { beginRotationTask } from './rotationTask'

export interface LoginFormProps {
  onLoggedIn: () => void
}

type Mode = 'login' | 'restore' | 'interrupted'

interface LoginFailure {
  problem: LoginProblem
  message: string
}

export function LoginForm({ onLoggedIn }: LoginFormProps) {
  const { t } = useTranslation()
  const [mode, setMode] = useState<Mode>('login')
  const [displayName, setDisplayName] = useState('')
  const [password, setPassword] = useState('')
  const [failure, setFailure] = useState<LoginFailure | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)

  const performLogin = async (name: string, pass: string) => {
    setError(null)
    setFailure(null)
    setSubmitting(true)

    const release = beginRotationTask('resume')
    let loginResult: Awaited<ReturnType<typeof commands.login>>
    try {
      loginResult = await commands.login(name, pass)
    } catch {
      loginResult = { status: 'error', error: 'internal error' }
    } finally {
      release()
    }
    if (loginResult.status !== 'ok') {
      const problem = classifyLoginError(loginResult.error)
      setSubmitting(false)
      if (problem === 'interrupted') {
        setMode('interrupted')
        return
      }
      setFailure({ problem, message: loginResult.error })
      return
    }

    const finished = await finishSignIn()
    if (finished.status === 'error') {
      setError(finished.message)
      setSubmitting(false)
      return
    }
    if (finished.status === 'sessionMissing') {
      // Should be impossible immediately after a successful login() --
      // surfaced distinctly rather than silently retried, same discipline
      // commands/auth.rs uses for its own "should be impossible" cases.
      setError(t('auth.sessionMissingAfterLogin'))
      setSubmitting(false)
      return
    }

    onLoggedIn()
  }

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const submitted = password
    setPassword('')
    await performLogin(displayName, submitted)
  }

  // DIAG_329 (items.id=329): dev-only test scaffolding -- logs straight into
  // the local dev/test account ("jason"/"jason", auto-created by login()'s
  // own bootstrap-on-first-call branch same as any other display name) with
  // no typing, so Cloud Chat pane-testing iterations don't each pay for a manual
  // login. import.meta.env.DEV is Vite's build-time flag (false, and this
  // whole branch dead-code-eliminated, in a production build -- see
  // MiddleZone.tsx:59 for the only other use of this convention in the
  // codebase). Remove once items.id=329's Cloud Chat pane work no longer needs
  // fast iteration.
  const handleDevLogin = () => {
    void performLogin('jason', 'jason')
  }

  if (mode === 'restore') {
    return (
      <RestoreAccessForm
        initialDisplayName={displayName}
        onLoggedIn={onLoggedIn}
        onBack={() => setMode('login')}
      />
    )
  }

  if (mode === 'interrupted') {
    return (
      <InterruptedChangeScreen
        displayName={displayName}
        onSignInWithNewPassword={() => setMode('login')}
        onUseRecoveryPhrase={() => setMode('restore')}
        onBack={() => setMode('login')}
      />
    )
  }

  // Member access keeps the t() key below statically checkable by
  // styleAndI18nConsistency.test.ts.
  const failureView = failure && {
    suffix: loginErrorSuffix(failure.problem),
    message: failure.message,
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

      <button type="button" onClick={() => setMode('restore')} disabled={submitting}>
        {t('auth.forgotPassword')}
      </button>

      {/* DIAG_329 (items.id=329): dev-only, see handleDevLogin's own comment. */}
      {import.meta.env.DEV && (
        <button type="button" onClick={handleDevLogin} disabled={submitting}>
          Dev login
        </button>
      )}

      {failureView &&
        (failureView.suffix !== null ? (
          <p role="alert">{t(`auth.loginErrors.${failureView.suffix}`)}</p>
        ) : (
          <p role="alert">{t('auth.loginError', { message: failureView.message })}</p>
        ))}
      {error && <p role="alert">{t('auth.loginError', { message: error })}</p>}
    </form>
  )
}
