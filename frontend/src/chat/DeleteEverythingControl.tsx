// The separate "delete everything" control of the Tech Support memory screen
// (items.id=891 stage 7, decisions.id=944 (8)): offered next to stopping, never
// part of it. Stopping keeps what QuietRabbit already knows; this removes it. It
// is a two-step control (button, then an inline confirmation) and calls
// purge_all_devices, which also cancels any Tech Support chat still waiting.
//
// THE CONFIRMATION SAYS PLAINLY what it deletes (every device, with what
// QuietRabbit recorded about them: facts, tickets and configuration changes) and
// that it does NOT delete chat history, earlier replies or the record of the
// user's choices. It promises nothing more. A refusal because a chat is still going
// (runs_still_running) leaves the confirmation open with a try again button; nothing
// was deleted. Placeholder copy, Chat-BRAND (items.id=839).
import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { DevicePurgeSummary } from '../bindings'
import { purgeEveryDevice } from './deviceLibraryApi'
import { deviceErrorKind, isRetryable } from './deviceLibrary'
import './DeleteEverythingControl.css'

export interface DeleteEverythingControlProps {
  personaId: string
  focusId: string
}

type Stage =
  | { phase: 'idle' }
  | { phase: 'confirming'; retry: boolean }
  | { phase: 'deleting' }
  | { phase: 'done'; counts: DevicePurgeSummary }

export function DeleteEverythingControl({ personaId, focusId }: DeleteEverythingControlProps) {
  const { t } = useTranslation()
  const [stage, setStage] = useState<Stage>({ phase: 'idle' })
  const [error, setError] = useState<{ retryable: boolean; message: string } | null>(null)

  async function run() {
    setError(null)
    setStage({ phase: 'deleting' })
    const r = await purgeEveryDevice(personaId, focusId)
    if (r.status === 'ok') {
      setStage({ phase: 'done', counts: r.data })
      return
    }
    const retryable = isRetryable(deviceErrorKind(r.error))
    setError({ retryable, message: r.error })
    setStage({ phase: 'confirming', retry: retryable })
  }

  if (stage.phase === 'done') {
    return (
      <output className="delete-everything__done">
        {t('navShell.chat.collectionConsent.manage.deleteAll.done', {
          facts: stage.counts.facts,
          cases: stage.counts.cases,
          changes: stage.counts.changes,
        })}
      </output>
    )
  }

  if (stage.phase === 'idle') {
    return (
      <div className="delete-everything">
        <button type="button" onClick={() => setStage({ phase: 'confirming', retry: false })}>
          {t('navShell.chat.collectionConsent.manage.deleteAll.button')}
        </button>
      </div>
    )
  }

  const deleting = stage.phase === 'deleting'
  return (
    <div
      className="delete-everything delete-everything__confirm"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="delete-everything-title"
    >
      <p id="delete-everything-title" className="delete-everything__title">
        {t('navShell.chat.collectionConsent.manage.deleteAll.confirmTitle')}
      </p>
      <p className="delete-everything__body">
        {t('navShell.chat.collectionConsent.manage.deleteAll.confirmBody')}
      </p>
      <p className="delete-everything__body">
        {t('navShell.chat.collectionConsent.manage.deleteAll.confirmNotDeleted')}
      </p>
      <p className="delete-everything__body">
        {t('navShell.chat.collectionConsent.manage.deleteAll.confirmPermanent')}
      </p>
      {error && (
        <p role="alert" className="delete-everything__error">
          {error.retryable
            ? t('navShell.chat.collectionConsent.manage.deleteAll.error.runsRunning')
            : t('navShell.chat.collectionConsent.manage.deleteAll.error.other', {
                message: error.message,
              })}
        </p>
      )}
      <div className="delete-everything__footer">
        <button type="button" disabled={deleting} onClick={() => setStage({ phase: 'idle' })}>
          {t('navShell.chat.collectionConsent.manage.deleteAll.cancel')}
        </button>
        <button
          type="button"
          className="delete-everything__danger"
          disabled={deleting}
          onClick={() => void run()}
        >
          {deleting
            ? t('navShell.chat.collectionConsent.manage.deleteAll.deleting')
            : stage.retry
              ? t('navShell.chat.collectionConsent.manage.deleteAll.tryAgain')
              : t('navShell.chat.collectionConsent.manage.deleteAll.confirm')}
        </button>
      </div>
    </div>
  )
}
