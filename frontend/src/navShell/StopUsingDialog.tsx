// items.id=959 unit 3: the stop-using confirm of the Facts view. A small controlled
// component, modelled on ClearConsentHistoryDialog: no backend call of its own,
// MyFactsView owns the commands. It asks no reason (decisions.id=968). Cancel has the
// focus on open; Cancel and Escape close it.

import { useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'

export interface StopUsingDialogProps {
  title: string
  /** The sentences of the confirm, already worded, one paragraph each. */
  lines: string[]
  busy: boolean
  onConfirm: () => void
  onCancel: () => void
}

export function StopUsingDialog({ title, lines, busy, onConfirm, onCancel }: StopUsingDialogProps) {
  const { t } = useTranslation()
  const cancel = useRef<HTMLButtonElement>(null)
  useEffect(() => {
    cancel.current?.focus()
  }, [])
  return (
    <div
      className="my-facts__overlay"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="my-facts-stop-title"
      onKeyDown={(e) => {
        if (e.key === 'Escape' && !busy) {
          e.stopPropagation()
          onCancel()
        }
      }}
    >
      <div className="my-facts__dialog">
        <h3 id="my-facts-stop-title" className="my-facts__dialog-title">
          {title}
        </h3>
        {lines.map((line) => (
          <p key={line} className="my-facts__dialog-body">
            {line}
          </p>
        ))}
        <div className="my-facts__dialog-actions">
          <button
            type="button"
            className="my-facts__button"
            ref={cancel}
            disabled={busy}
            onClick={onCancel}
          >
            {t('navShell.facts.stop.cancel')}
          </button>
          <button type="button" className="my-facts__confirm" disabled={busy} onClick={onConfirm}>
            {t('navShell.facts.stop.confirm')}
          </button>
        </div>
      </div>
    </div>
  )
}
