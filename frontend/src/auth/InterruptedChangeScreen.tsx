// What the login screen shows when the OLD password was typed while a password
// change is pending ("rotation_interrupted", items.id=769 stage 4). The
// account's data is split between two keys and only the NEW password or the
// recovery phrase reaches the new one, so the old password cannot sign in.
//
// Three ways forward:
//   - sign in with the NEW password (login() finishes the change and signs in);
//   - use the recovery phrase (the restore form finishes it with the words);
//   - cancel the change (cancelInterruptedRotationWith*): proves it is the
//     account's owner with the new password or the phrase, rolls the account
//     back to its old key, and does NOT sign in -- the person then signs in
//     with the OLD password. The old password typed here is simply wrong.
//
// What the backend does not tell us before sign-in is which kind of change was
// pending (get_rotation_status reports none without a session), so the copy
// stays general.

import './InterruptedChangeScreen.css'
import { useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type KeyRotationError } from '../bindings'
import { PasswordField } from './PasswordField'
import { joinWords, looksLikePhrase, splitWords, PHRASE_WORD_COUNT } from './recoveryWords'
import { RotationErrorText } from './RotationErrorText'
import { beginRotationTask } from './rotationTask'

type CancelMethod = 'password' | 'phrase'

export interface InterruptedChangeScreenProps {
  displayName: string
  /** Back to the sign-in form, to type the new password. */
  onSignInWithNewPassword: () => void
  /** To the recovery-phrase form, which finishes the change with the words. */
  onUseRecoveryPhrase: () => void
  /** Back to the sign-in form (after a cancel: sign in with the old password). */
  onBack: () => void
}

export function InterruptedChangeScreen({
  displayName,
  onSignInWithNewPassword,
  onUseRecoveryPhrase,
  onBack,
}: InterruptedChangeScreenProps) {
  const { t } = useTranslation()
  const [method, setMethod] = useState<CancelMethod | null>(null)
  const [password, setPassword] = useState('')
  const [phrase, setPhrase] = useState('')
  const [error, setError] = useState<KeyRotationError | null>(null)
  const [submitting, setSubmitting] = useState(false)
  const [cancelled, setCancelled] = useState(false)

  const wordCount = splitWords(phrase).length

  const handleCancel = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const submittedPassword = password
    const submittedPhrase = phrase
    // Cleared on every submit.
    setPassword('')
    setPhrase('')
    setError(null)
    if (method === 'phrase' && !looksLikePhrase(submittedPhrase)) {
      setError({ code: 'phrase_invalid' })
      return
    }

    setSubmitting(true)
    const release = beginRotationTask('cancel')
    let result: Awaited<ReturnType<typeof commands.cancelInterruptedRotationWithPassword>>
    try {
      result =
        method === 'phrase'
          ? await commands.cancelInterruptedRotationWithPhrase(displayName, joinWords(submittedPhrase))
          : await commands.cancelInterruptedRotationWithPassword(displayName, submittedPassword)
    } catch {
      result = { status: 'error', error: { code: 'internal' } }
    } finally {
      release()
      setSubmitting(false)
    }
    if (result.status === 'ok') {
      setCancelled(true)
      return
    }
    setError(result.error)
  }

  if (cancelled) {
    return (
      <div className="interrupted-change">
        <p className="interrupted-change__text">{t('auth.interrupted.cancelled')}</p>
        <div className="interrupted-change__buttons">
          <button type="button" onClick={onBack}>
            {t('auth.interrupted.backToSignIn')}
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="interrupted-change">
      <h3 className="interrupted-change__heading">{t('auth.interrupted.heading')}</h3>
      <p className="interrupted-change__text">{t('auth.interrupted.intro', { name: displayName })}</p>

      <div className="interrupted-change__buttons">
        <button type="button" onClick={onSignInWithNewPassword} disabled={submitting}>
          {t('auth.interrupted.signInNew')}
        </button>
        <button type="button" onClick={onUseRecoveryPhrase} disabled={submitting}>
          {t('auth.interrupted.useRecovery')}
        </button>
      </div>

      <h4 className="interrupted-change__subheading">{t('auth.interrupted.cancelHeading')}</h4>
      <p className="interrupted-change__text">{t('auth.interrupted.cancelIntro')}</p>

      {method === null ? (
        <div className="interrupted-change__buttons">
          <button type="button" onClick={() => setMethod('password')}>
            {t('auth.interrupted.cancelWithPassword')}
          </button>
          <button type="button" onClick={() => setMethod('phrase')}>
            {t('auth.interrupted.cancelWithPhrase')}
          </button>
        </div>
      ) : (
        <form className="interrupted-change__form" onSubmit={handleCancel}>
          {method === 'password' ? (
            <div className="interrupted-change__field">
              <label htmlFor="interrupted-new-password">{t('auth.interrupted.newPasswordLabel')}</label>
              <PasswordField
                id="interrupted-new-password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                autoComplete="new-password"
                disabled={submitting}
                required
              />
            </div>
          ) : (
            <div className="interrupted-change__field">
              <label htmlFor="interrupted-phrase">{t('auth.interrupted.phraseLabel')}</label>
              <textarea
                id="interrupted-phrase"
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
              <span className="interrupted-change__count">
                {t('auth.restore.wordCount', { count: wordCount, total: PHRASE_WORD_COUNT })}
              </span>
            </div>
          )}
          <div className="interrupted-change__buttons">
            <button
              type="button"
              onClick={() => {
                setMethod(null)
                setError(null)
              }}
              disabled={submitting}
            >
              {t('auth.interrupted.cancelBack')}
            </button>
            <button type="submit" disabled={submitting}>
              {submitting ? t('auth.interrupted.cancelling') : t('auth.interrupted.cancelSubmit')}
            </button>
          </div>
        </form>
      )}
      <RotationErrorText error={error} context="cancel" />

      <div className="interrupted-change__buttons">
        <button type="button" onClick={onBack} disabled={submitting}>
          {t('auth.interrupted.backToSignIn')}
        </button>
      </div>
    </div>
  )
}
