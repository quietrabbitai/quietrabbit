// items.id=957 unit 3: the confirm of "Clear consent history" (design record
// section 8). A small controlled component, modelled on CrossPersonaConfirmModal: no
// backend call of its own, SharedLogView owns the command. The destructive button
// stays disabled until the user ticks "I understand this can't be undone"; Cancel
// and Escape close it. The command itself also requires an explicit confirm flag.
//
// CHAT-BRAND REVIEW: placeholder copy under navShell.shared.clear*.

import { useState } from 'react'
import { useTranslation } from 'react-i18next'

export interface ClearConsentHistoryDialogProps {
  personaName: string
  /** Consent records the purge will remove. */
  count: number
  busy: boolean
  /** A failure from the last attempt, already worded. */
  error: string | null
  onConfirm: () => void
  onCancel: () => void
}

export function ClearConsentHistoryDialog({
  personaName,
  count,
  busy,
  error,
  onConfirm,
  onCancel,
}: ClearConsentHistoryDialogProps) {
  const { t } = useTranslation()
  const [understood, setUnderstood] = useState(false)
  return (
    <div
      className="shared-log__overlay"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="shared-log-clear-title"
      onKeyDown={(e) => {
        if (e.key === 'Escape' && !busy) onCancel()
      }}
    >
      <div className="shared-log__dialog">
        <h3 id="shared-log-clear-title" className="shared-log__dialog-title">
          {t('navShell.shared.clearTitle', { persona: personaName })}
        </h3>
        <p className="shared-log__dialog-body">{t('navShell.shared.clearBody', { count })}</p>
        <label className="shared-log__dialog-check">
          <input
            type="checkbox"
            checked={understood}
            disabled={busy}
            onChange={(e) => setUnderstood(e.target.checked)}
          />
          <span>{t('navShell.shared.clearUnderstand')}</span>
        </label>
        {error !== null && <p role="alert">{error}</p>}
        <div className="shared-log__dialog-actions">
          <button type="button" className="shared-log__reset" disabled={busy} onClick={onCancel}>
            {t('navShell.shared.clearCancel')}
          </button>
          <button
            type="button"
            className="shared-log__danger"
            disabled={!understood || busy}
            onClick={onConfirm}
          >
            {t('navShell.shared.clearConfirm')}
          </button>
        </div>
      </div>
    </div>
  )
}
