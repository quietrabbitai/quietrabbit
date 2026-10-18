// Recovery-phrase display + confirmation (items.id=769 stage 3). Shared: the
// "create a new recovery phrase" flow uses it now and the account-creation
// screen (items.id=180) reuses it later (AUTH_MULTIUSER_ARCHITECTURE.md 5.4);
// wiring it into onboarding is NOT part of this item.
//
// Two steps in one component.
//   show:    the 24 numbered words, once. No clipboard button and the text is
//            not selectable -- the point is writing it down, and a phrase on
//            the clipboard is readable by every other program.
//   confirm: the words are gone; the person re-enters the words at the
//            positions the caller supplies. A tap on "I've written it down"
//            is not enough by design (AUTH 5.4).
//
// THE WORDS LIVE ONLY HERE. The component copies them into its own state at
// mount, drops that copy the moment it moves to the confirm step, and calls
// `onWordsHidden` so the parent drops its copy too. They are never put in
// storage, a URL, a log or an i18n param. (A JavaScript string cannot be
// overwritten in memory; dropping every reference is the most that can be
// done.) Typed answers are cleared on submit.

import './RecoveryPhraseDisplay.css'
import { useState, type FormEvent, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import type { WordAnswer } from '../bindings'
import { answersComplete, buildAnswers, splitWords } from './recoveryWords'

export interface RecoveryPhraseDisplayProps {
  /** The phrase, once. The parent should drop its copy on `onWordsHidden`. */
  mnemonic: string
  /** 1-based positions the person must re-enter. */
  confirmPositions: number[]
  /** The words are about to disappear from the screen. */
  onWordsHidden: () => void
  /** The typed answers, normalised. Fields are cleared after this call. */
  onConfirm: (answers: WordAnswer[]) => void
  /** Leave without confirming. */
  onCancel: () => void
  /** Text of the confirm button (differs between callers). */
  submitLabel: string
  /** A confirm is in flight: everything is disabled. */
  busy: boolean
  /** Extra fields for the confirm step (the new-phrase flow asks for the
   *  password again here). */
  confirmExtra?: ReactNode
  /** The caller's additional reason to keep the confirm button disabled. */
  confirmDisabled?: boolean
  /** Shown under the confirm form (the caller's error text). */
  error?: ReactNode
}

export function RecoveryPhraseDisplay({
  mnemonic,
  confirmPositions,
  onWordsHidden,
  onConfirm,
  onCancel,
  submitLabel,
  busy,
  confirmExtra,
  confirmDisabled = false,
  error,
}: RecoveryPhraseDisplayProps) {
  const { t } = useTranslation()
  // Local copy; null once the words are hidden.
  const [words, setWords] = useState<string[] | null>(() => splitWords(mnemonic))
  const [entered, setEntered] = useState<Record<number, string>>({})

  const hideWords = () => {
    setWords(null)
    onWordsHidden()
  }

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const answers = buildAnswers(confirmPositions, entered)
    setEntered({})
    onConfirm(answers)
  }

  if (words !== null) {
    return (
      <div className="recovery-phrase">
        <h4 className="recovery-phrase__heading">{t('auth.recoveryPhrase.showHeading')}</h4>
        <p className="recovery-phrase__text">{t('auth.recoveryPhrase.showIntro')}</p>
        <ol className="recovery-phrase__words" aria-label={t('auth.recoveryPhrase.wordsLabel')}>
          {words.map((word, index) => (
            // The phrase can repeat a word, so the position is the key.
            <li key={index} className="recovery-phrase__word">
              <span className="recovery-phrase__index">{index + 1}</span>
              <span>{word}</span>
            </li>
          ))}
        </ol>
        <p className="recovery-phrase__text">{t('auth.recoveryPhrase.showWarning')}</p>
        <div className="recovery-phrase__buttons">
          <button type="button" onClick={onCancel} disabled={busy}>
            {t('auth.recoveryPhrase.cancel')}
          </button>
          <button type="button" onClick={hideWords} disabled={busy}>
            {t('auth.recoveryPhrase.writtenDown')}
          </button>
        </div>
      </div>
    )
  }

  return (
    <form className="recovery-phrase" onSubmit={handleSubmit}>
      <h4 className="recovery-phrase__heading">{t('auth.recoveryPhrase.confirmHeading')}</h4>
      <p className="recovery-phrase__text">{t('auth.recoveryPhrase.confirmIntro')}</p>
      {confirmPositions.map((position) => (
        <div key={position} className="recovery-phrase__answer">
          <label htmlFor={`recovery-phrase-word-${position}`}>
            {t('auth.recoveryPhrase.wordLabel', { position })}
          </label>
          <input
            id={`recovery-phrase-word-${position}`}
            type="text"
            value={entered[position] ?? ''}
            onChange={(event) => setEntered((prev) => ({ ...prev, [position]: event.target.value }))}
            autoComplete="off"
            autoCapitalize="none"
            autoCorrect="off"
            spellCheck={false}
            disabled={busy}
          />
        </div>
      ))}
      {confirmExtra}
      <div className="recovery-phrase__buttons">
        <button type="button" onClick={onCancel} disabled={busy}>
          {t('auth.recoveryPhrase.cancel')}
        </button>
        <button
          type="submit"
          disabled={busy || confirmDisabled || !answersComplete(confirmPositions, entered)}
        >
          {submitLabel}
        </button>
      </div>
      {error}
    </form>
  )
}
