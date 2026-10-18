// "Forgot password": restore access with the 24-word recovery phrase and
// choose a new password (items.id=769 stage 4). The backend unseals the data
// key with the words, proves it against the account's data, re-locks
// everything under a new key and the new password, and signs the person in
// (restoreWithRecoveryPhrase) -- so success follows LoginForm's path through
// finishSignIn. If a change was interrupted earlier, the same call finishes it
// with the words and the account ends on the password typed here.
//
// The typed phrase and both passwords are cleared as soon as the form is
// submitted, and are never stored or logged.

import './RestoreAccessForm.css'
import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type KeyRotationError } from '../bindings'
import { finishSignIn } from './finishSignIn'
import { newPasswordProblem } from './passwordForm'
import { joinWords, looksLikePhrase, PHRASE_WORD_COUNT, splitWords } from './recoveryWords'
import { RotationErrorText } from './RotationErrorText'
import { beginRotationTask } from './rotationTask'

export interface RestoreAccessFormProps {
  initialDisplayName: string
  onLoggedIn: () => void
  onBack: () => void
}

export function RestoreAccessForm({ initialDisplayName, onLoggedIn, onBack }: RestoreAccessFormProps) {
  const { t } = useTranslation()
  const [displayName, setDisplayName] = useState(initialDisplayName)
  const [phrase, setPhrase] = useState('')
  const [next, setNext] = useState('')
  const [confirm, setConfirm] = useState('')
  const [mismatch, setMismatch] = useState(false)
  const [error, setError] = useState<KeyRotationError | null>(null)
  const [sessionProblem, setSessionProblem] = useState<string | null>(null)
  const [submitting, setSubmitting] = useState(false)

  const wordCount = splitWords(phrase).length

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const submittedPhrase = phrase
    const submittedNext = next
    const problem = newPasswordProblem(next, confirm)
    // Cleared on every submit, valid or not.
    setPhrase('')
    setNext('')
    setConfirm('')
    setError(null)
    setMismatch(false)
    setSessionProblem(null)
    if (!looksLikePhrase(submittedPhrase)) {
      setError({ code: 'phrase_invalid' })
      return
    }
    if (problem === 'empty') {
      setError({ code: 'new_password_empty' })
      return
    }
    if (problem === 'mismatch') {
      setMismatch(true)
      return
    }

    setSubmitting(true)
    const release = beginRotationTask('restore')
    let result: Awaited<ReturnType<typeof commands.restoreWithRecoveryPhrase>>
    try {
      result = await commands.restoreWithRecoveryPhrase(
        displayName,
        joinWords(submittedPhrase),
        submittedNext,
      )
    } catch {
      result = { status: 'error', error: { code: 'internal' } }
    } finally {
      release()
      setSubmitting(false)
    }
    if (result.status !== 'ok') {
      setError(result.error)
      return
    }
    const finished = await finishSignIn()
    if (finished.status === 'ok') {
      onLoggedIn()
    } else if (finished.status === 'sessionMissing') {
      setSessionProblem(t('auth.sessionMissingAfterLogin'))
    } else {
      setSessionProblem(finished.message)
    }
  }

  return (
    <form className="restore-access-form" onSubmit={handleSubmit}>
      <h3 className="restore-access-form__heading">{t('auth.restore.heading')}</h3>
      <p className="restore-access-form__text">{t('auth.restore.intro')}</p>

      <div className="restore-access-form__field">
        <label htmlFor="restore-display-name">{t('auth.displayNameLabel')}</label>
        <input
          id="restore-display-name"
          type="text"
          value={displayName}
          onChange={(event) => setDisplayName(event.target.value)}
          autoComplete="username"
          disabled={submitting}
          required
        />
      </div>

      <div className="restore-access-form__field">
        <label htmlFor="restore-phrase">{t('auth.restore.phraseLabel')}</label>
        <textarea
          id="restore-phrase"
          value={phrase}
          onChange={(event) => setPhrase(event.target.value)}
          rows={4}
          autoComplete="off"
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          disabled={submitting}
          required
        />
        <span className="restore-access-form__count">
          {t('auth.restore.wordCount', { count: wordCount, total: PHRASE_WORD_COUNT })}
        </span>
      </div>

      <div className="restore-access-form__field">
        <label htmlFor="restore-new-password">{t('auth.restore.newPasswordLabel')}</label>
        <input
          id="restore-new-password"
          type="password"
          value={next}
          onChange={(event) => setNext(event.target.value)}
          autoComplete="new-password"
          disabled={submitting}
          required
        />
      </div>

      <div className="restore-access-form__field">
        <label htmlFor="restore-confirm-password">{t('auth.restore.confirmLabel')}</label>
        <input
          id="restore-confirm-password"
          type="password"
          value={confirm}
          onChange={(event) => setConfirm(event.target.value)}
          autoComplete="new-password"
          disabled={submitting}
          required
        />
      </div>

      <div className="restore-access-form__buttons">
        <button type="button" onClick={onBack} disabled={submitting}>
          {t('auth.restore.back')}
        </button>
        <button type="submit" disabled={submitting}>
          {submitting ? t('auth.restore.submitting') : t('auth.restore.submit')}
        </button>
      </div>

      {mismatch && (
        <p role="alert" className="restore-access-form__error">
          {t('auth.restore.mismatch')}
        </p>
      )}
      {sessionProblem && (
        <p role="alert" className="restore-access-form__error">
          {t('auth.loginError', { message: sessionProblem })}
        </p>
      )}
      <RotationErrorText error={error} context="restore" />
    </form>
  )
}
