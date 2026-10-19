// Change-password form (items.id=769 stage 2). Re-verifies the current
// password server-side, then the backend rotates the data key (a long task:
// progress is the RotationProgress overlay's job, not this form's). The form
// registers as the rotation's starter (rotationTask) so it, not the overlay,
// reports how the change ended.
//
// Every field is cleared as soon as it is submitted. On needs_resume or
// epoch_changed the session may no longer match the account's key, so the app
// signs the user out and the login screen carries the same message.

import './ChangePasswordForm.css'
import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type KeyRotationError } from '../bindings'
import { NewPasswordHint } from './NewPasswordHint'
import { PasswordField } from './PasswordField'
import { canSubmitPasswordChange } from './passwordForm'
import { validateNewPassword } from './passwordRules'
import { RotationErrorText } from './RotationErrorText'
import { requiresSignOut, type SignOutNotice } from './rotationErrors'
import { beginRotationTask } from './rotationTask'

export interface ChangePasswordFormProps {
  /** The signed-in display name, for the hidden username field that lets a
   *  password manager update the right saved login. */
  displayName: string
  /** Reported so the surrounding panel cannot be closed mid-change. */
  onBusyChange: (busy: boolean) => void
  onSignOut: (notice: SignOutNotice) => void
}

export function ChangePasswordForm({ displayName, onBusyChange, onSignOut }: ChangePasswordFormProps) {
  const { t } = useTranslation()
  const [current, setCurrent] = useState('')
  const [next, setNext] = useState('')
  const [confirm, setConfirm] = useState('')
  const [mismatch, setMismatch] = useState(false)
  const [error, setError] = useState<KeyRotationError | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [done, setDone] = useState(false)

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const submittedCurrent = current
    const submittedNext = next
    const problem = validateNewPassword(next, confirm)
    // Cleared on every submit, valid or not: nothing typed lingers in state.
    setCurrent('')
    setNext('')
    setConfirm('')
    setError(null)
    setMismatch(false)
    if (problem === 'empty') {
      setError({ code: 'new_password_empty' })
      return
    }
    if (problem === 'too_short') {
      setError({ code: 'new_password_too_short' })
      return
    }
    if (problem === 'mismatch') {
      setMismatch(true)
      return
    }

    setSubmitting(true)
    onBusyChange(true)
    const release = beginRotationTask('password_change')
    let result: Awaited<ReturnType<typeof commands.changePassword>>
    try {
      result = await commands.changePassword(submittedCurrent, submittedNext)
    } catch {
      // The IPC call itself threw (the bridge, not a coded error): report the
      // generic error rather than leave the form stuck busy.
      result = { status: 'error', error: { code: 'internal' } }
    } finally {
      release()
      setSubmitting(false)
      onBusyChange(false)
    }

    if (result.status === 'ok') {
      setDone(true)
      return
    }
    if (requiresSignOut(result.error.code)) {
      onSignOut({ error: result.error, context: 'changePassword' })
      return
    }
    setError(result.error)
  }

  if (done) {
    return <p className="change-password-form__success">{t('auth.accountSecurity.changePassword.success')}</p>
  }

  return (
    <form className="change-password-form" onSubmit={handleSubmit}>
      <p className="change-password-form__intro">{t('auth.accountSecurity.changePassword.intro')}</p>

      {/* Not shown: tells a password manager which saved login this new
          password belongs to. */}
      <input
        type="text"
        className="change-password-form__username"
        value={displayName}
        autoComplete="username"
        readOnly
        tabIndex={-1}
        aria-hidden="true"
      />

      <div>
        <label htmlFor="change-password-current">{t('auth.accountSecurity.changePassword.currentLabel')}</label>
        <PasswordField
          id="change-password-current"
          value={current}
          onChange={(event) => setCurrent(event.target.value)}
          autoComplete="current-password"
          disabled={submitting}
          required
        />
      </div>

      <div>
        <label htmlFor="change-password-new">{t('auth.accountSecurity.changePassword.newLabel')}</label>
        <PasswordField
          id="change-password-new"
          value={next}
          onChange={(event) => setNext(event.target.value)}
          autoComplete="new-password"
          aria-describedby="change-password-new-hint"
          disabled={submitting}
          required
        />
        <NewPasswordHint id="change-password-new-hint" password={next} />
      </div>

      <div>
        <label htmlFor="change-password-confirm">{t('auth.accountSecurity.changePassword.confirmLabel')}</label>
        <PasswordField
          id="change-password-confirm"
          value={confirm}
          onChange={(event) => setConfirm(event.target.value)}
          autoComplete="new-password"
          disabled={submitting}
          required
        />
      </div>

      <button type="submit" disabled={submitting || !canSubmitPasswordChange(current, next, confirm)}>
        {submitting
          ? t('auth.accountSecurity.changePassword.submitting')
          : t('auth.accountSecurity.changePassword.submit')}
      </button>

      {mismatch && (
        <p role="alert" className="change-password-form__mismatch">
          {t('auth.accountSecurity.changePassword.mismatch')}
        </p>
      )}
      <RotationErrorText error={error} context="changePassword" />
    </form>
  )
}
