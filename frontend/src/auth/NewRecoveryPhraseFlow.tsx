// "Create a new recovery phrase" (items.id=769 stage 3). The revocation path
// for a leaked phrase: the old words stop opening current data once this
// completes (AUTH_MULTIUSER_ARCHITECTURE.md 4.0).
//
// Order, as the backend built it (commands/key_rotation.rs): the current
// password is verified and the NEW words are generated and shown first
// (beginNewRecoveryPhrase); nothing is rotated until the person re-enters the
// words the SERVER asked for and types the password again
// (confirmNewRecoveryPhrase), which then runs the rotation. Abandoning the flow
// at any earlier point leaves the old phrase valid, and unmounting calls
// cancelNewRecoveryPhrase so the server drops the shown phrase.
//
// The mnemonic is held in this component's state only between begin and the
// moment RecoveryPhraseDisplay hides the words; it is dropped then, on cancel,
// and on unmount, and never stored or logged.

import './NewRecoveryPhraseFlow.css'
import { useEffect, useRef, useState, type FormEvent } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type KeyRotationError, type WordAnswer } from '../bindings'
import { PasswordField } from './PasswordField'
import { RecoveryPhraseDisplay } from './RecoveryPhraseDisplay'
import { RotationErrorText } from './RotationErrorText'
import { requiresSignOut, type SignOutNotice } from './rotationErrors'
import { beginRotationTask } from './rotationTask'
import { useWritersFlow } from './useWritersFlow'
import { WritersPrompt } from './WritersPrompt'

type Step = 'password' | 'phrase'

interface PendingPhrase {
  /** null once the words have been hidden. */
  mnemonic: string | null
  positions: number[]
}

export interface NewRecoveryPhraseFlowProps {
  /** Leave the flow (before starting, or after finishing). */
  onClose: () => void
  onBusyChange: (busy: boolean) => void
  onSignOut: (notice: SignOutNotice) => void
}

export function NewRecoveryPhraseFlow({ onClose, onBusyChange, onSignOut }: NewRecoveryPhraseFlowProps) {
  const { t } = useTranslation()
  const [step, setStep] = useState<Step>('password')
  const [password, setPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [pending, setPending] = useState<PendingPhrase | null>(null)
  const [phraseGone, setPhraseGone] = useState(false)
  const [error, setError] = useState<KeyRotationError | null>(null)
  const [busy, setBusy] = useState(false)
  // True while the server may be holding a shown, unconfirmed phrase.
  const serverHoldsPhrase = useRef(false)

  const setWorking = (working: boolean) => {
    setBusy(working)
    onBusyChange(working)
  }

  // Before the rotation starts the backend drains any running chat reply or
  // Focus run (items.id=778); the flow shows the finish-or-stop prompt and
  // holds the confirm call's password and answers only while it is up.
  const flow = useWritersFlow(({ result }) => {
    setWorking(false)
    if (result === null) return // cancelled: nothing was changed
    if (result.status === 'ok') {
      // The backend has ended the session (items.id=778): session-locked returns
      // the app to the login screen with the message. Drop the shown words.
      serverHoldsPhrase.current = false
      setPending(null)
      setStep('password')
      return
    }
    if (requiresSignOut(result.error.code)) {
      onSignOut({ error: result.error, context: 'newPhrase' })
      return
    }
    if (result.error.code === 'no_pending_phrase') {
      // Discarded server-side (too many wrong confirmations, or a lock): the
      // shown phrase is gone and a new one must be generated.
      serverHoldsPhrase.current = false
      setPhraseGone(true)
    }
    setError(result.error)
  })

  // Leaving with a phrase shown but not adopted: tell the server to drop it.
  useEffect(
    () => () => {
      if (serverHoldsPhrase.current) {
        serverHoldsPhrase.current = false
        void commands.cancelNewRecoveryPhrase()
      }
    },
    [],
  )

  const cancelPhrase = () => {
    setPending(null)
    setConfirmPassword('')
    setPhraseGone(false)
    setError(null)
    if (serverHoldsPhrase.current) {
      serverHoldsPhrase.current = false
      void commands.cancelNewRecoveryPhrase()
    }
  }

  const handleBegin = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const submitted = password
    setPassword('')
    setError(null)
    setWorking(true)
    let result: Awaited<ReturnType<typeof commands.beginNewRecoveryPhrase>>
    try {
      result = await commands.beginNewRecoveryPhrase(submitted)
    } catch {
      result = { status: 'error', error: { code: 'internal' } }
    } finally {
      setWorking(false)
    }
    if (result.status === 'ok') {
      serverHoldsPhrase.current = true
      setPending({ mnemonic: result.data.mnemonic, positions: result.data.confirm_positions })
      setStep('phrase')
      return
    }
    if (requiresSignOut(result.error.code)) {
      onSignOut({ error: result.error, context: 'newPhrase' })
      return
    }
    setError(result.error)
  }

  const handleConfirm = (answers: WordAnswer[]) => {
    const submitted = confirmPassword
    setConfirmPassword('')
    setError(null)
    setWorking(true)
    flow.begin(
      (writers) => commands.confirmNewRecoveryPhrase(submitted, answers, writers),
      beginRotationTask('new_phrase'),
    )
  }

  if (flow.state.kind !== 'idle') {
    return (
      <WritersPrompt
        state={flow.state}
        onChoose={flow.choose}
        onCancel={flow.cancel}
        onDismiss={flow.dismiss}
      />
    )
  }

  if (step === 'phrase' && pending !== null) {
    if (phraseGone) {
      return (
        <div className="new-phrase-flow">
          <RotationErrorText error={error} context="newPhrase" />
          <div className="new-phrase-flow__buttons">
            <button
              type="button"
              onClick={() => {
                cancelPhrase()
                setStep('password')
              }}
            >
              {t('auth.newPhrase.startOver')}
            </button>
          </div>
        </div>
      )
    }
    return (
      <RecoveryPhraseDisplay
        // Only mounted with a phrase in hand; `mnemonic` is null after the
        // words are hidden, and the display no longer reads it by then.
        mnemonic={pending.mnemonic ?? ''}
        confirmPositions={pending.positions}
        onWordsHidden={() => setPending((prev) => (prev ? { ...prev, mnemonic: null } : prev))}
        onConfirm={(answers) => handleConfirm(answers)}
        onCancel={() => {
          cancelPhrase()
          onClose()
        }}
        submitLabel={t('auth.newPhrase.confirmSubmit')}
        busy={busy}
        confirmDisabled={confirmPassword.length === 0}
        confirmExtra={
          <div className="new-phrase-flow__field">
            <label htmlFor="new-phrase-confirm-password">{t('auth.newPhrase.confirmPasswordLabel')}</label>
            <PasswordField
              id="new-phrase-confirm-password"
              value={confirmPassword}
              onChange={(event) => setConfirmPassword(event.target.value)}
              autoComplete="current-password"
              disabled={busy}
            />
          </div>
        }
        error={<RotationErrorText error={error} context="newPhrase" />}
      />
    )
  }

  return (
    <form className="new-phrase-flow" onSubmit={handleBegin}>
      <p className="new-phrase-flow__text">{t('auth.newPhrase.intro')}</p>
      <div className="new-phrase-flow__field">
        <label htmlFor="new-phrase-password">{t('auth.newPhrase.passwordLabel')}</label>
        <PasswordField
          id="new-phrase-password"
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          autoComplete="current-password"
          disabled={busy}
          required
        />
      </div>
      <div className="new-phrase-flow__buttons">
        <button type="button" onClick={onClose} disabled={busy}>
          {t('auth.newPhrase.cancel')}
        </button>
        <button type="submit" disabled={busy || password.length === 0}>
          {t('auth.newPhrase.start')}
        </button>
      </div>
      <RotationErrorText error={error} context="newPhrase" />
    </form>
  )
}
