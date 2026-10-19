// The finish-or-stop prompt, the waiting screen and the two "did not finish"
// messages of a credential change (items.id=778). Renders by the state from
// rotationWriters.ts; the wording is in en.json under auth.rotationWriters.
// 'Finish first' is the default and gets focus. 'Stop and save' says honestly
// what is kept.

import './WritersPrompt.css'
import { useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'
import { blockerOf, canStop, type WaitingChoice, type WriterState } from './rotationWriters'

export interface WritersPromptProps {
  state: WriterState
  onChoose: (choice: WaitingChoice) => void
  onCancel: () => void
  onDismiss: () => void
}

export function WritersPrompt({ state, onChoose, onCancel, onDismiss }: WritersPromptProps) {
  const { t } = useTranslation()
  const primary = useRef<HTMLButtonElement | null>(null)
  const kind = state.kind

  // The safe choice has focus whenever a new prompt appears.
  useEffect(() => {
    primary.current?.focus()
  }, [kind])

  if (state.kind === 'idle') return null

  if (state.kind === 'ask') {
    const blocker = blockerOf(state.replies, state.focusRuns)
    const title =
      blocker === 'replies'
        ? t('auth.rotationWriters.ask.title.replies')
        : blocker === 'tasks'
          ? t('auth.rotationWriters.ask.title.tasks')
          : t('auth.rotationWriters.ask.title.both')
    return (
      <section className="writers-prompt" aria-labelledby="writers-prompt-title">
        <h3 id="writers-prompt-title" className="writers-prompt__title">
          {title}
        </h3>
        <p className="writers-prompt__text">{t('auth.rotationWriters.ask.body')}</p>
        <div className="writers-prompt__choice">
          <button type="button" ref={primary} onClick={() => onChoose('finish_first')}>
            {t('auth.rotationWriters.ask.finishFirst')}
          </button>
          <p className="writers-prompt__hint">{t('auth.rotationWriters.ask.finishFirstHint')}</p>
        </div>
        <div className="writers-prompt__choice">
          <button type="button" onClick={() => onChoose('stop_and_save')}>
            {t('auth.rotationWriters.ask.stopAndSave')}
          </button>
          <p className="writers-prompt__hint">{t('auth.rotationWriters.ask.stopAndSaveHint')}</p>
        </div>
        <div className="writers-prompt__buttons">
          <button type="button" onClick={onCancel}>
            {t('auth.rotationWriters.ask.cancel')}
          </button>
        </div>
      </section>
    )
  }

  if (state.kind === 'waiting') {
    const blocker = blockerOf(state.replies, state.focusRuns)
    return (
      <output className="writers-prompt" aria-live="polite">
        <p className="writers-prompt__title">
          {state.choice === 'stop_and_save'
            ? t('auth.rotationWriters.waiting.stop')
            : t('auth.rotationWriters.waiting.finish')}
        </p>
        {blocker !== 'none' && (
          <p className="writers-prompt__text">
            {blocker === 'replies'
              ? t('auth.rotationWriters.waiting.blocker.replies')
              : blocker === 'tasks'
                ? t('auth.rotationWriters.waiting.blocker.tasks')
                : t('auth.rotationWriters.waiting.blocker.both')}
          </p>
        )}
        <p className="writers-prompt__text">{t('auth.rotationWriters.waiting.paused')}</p>
        <div className="writers-prompt__buttons">
          <button type="button" ref={primary} onClick={onCancel}>
            {t('auth.rotationWriters.waiting.cancel')}
          </button>
        </div>
      </output>
    )
  }

  if (state.kind === 'ceiling') {
    const work = canStop(state.replies, state.focusRuns)
    return (
      <section className="writers-prompt" aria-labelledby="writers-prompt-title">
        <h3 id="writers-prompt-title" className="writers-prompt__title">
          {t('auth.rotationWriters.ceiling.title')}
        </h3>
        <p className="writers-prompt__text">
          {work
            ? t('auth.rotationWriters.ceiling.bodyWork')
            : t('auth.rotationWriters.ceiling.bodyOther')}
        </p>
        <div className="writers-prompt__buttons">
          <button type="button" ref={primary} onClick={() => onChoose('finish_first')}>
            {t('auth.rotationWriters.ceiling.keepWaiting')}
          </button>
          {work && (
            <button type="button" onClick={() => onChoose('stop_and_save')}>
              {t('auth.rotationWriters.ceiling.stopAndSave')}
            </button>
          )}
          <button type="button" onClick={onCancel}>
            {t('auth.rotationWriters.ceiling.cancel')}
          </button>
        </div>
      </section>
    )
  }

  return (
    <div className="writers-prompt" role="alert">
      <h3 className="writers-prompt__title">{t('auth.rotationWriters.notStopping.title')}</h3>
      <p className="writers-prompt__text">{t('auth.rotationWriters.notStopping.body')}</p>
      <div className="writers-prompt__buttons">
        <button type="button" ref={primary} onClick={onDismiss}>
          {t('auth.rotationWriters.notStopping.ok')}
        </button>
      </div>
    </div>
  )
}
