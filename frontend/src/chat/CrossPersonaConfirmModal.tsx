// Cross-Persona export confirmation (decisions.id=546/639/815, items.id=27).
//
// No generic confirm-dialog component exists anywhere in this frontend --
// PrivacyGuardianModal.tsx (tier3Access/) is a heavyweight, Gate3-specific
// tiered-review modal built around ConsentRequestPayload/ElementDecision[]
// and a live consent_request event; the FocusSettingsControls.tsx
// friction-gate pattern is inline component state with no separate
// component at all. This is closer in spirit to ChatPane.tsx's own existing
// "confirm-withheld" inline alertdialog (a lighter yes/no prompt before an
// action proceeds), just itemized: one row per pending
// get_pending_cross_persona_confirmations fact, each gets an Include/Don't
// include decision, and Continue is enabled only once every row has one --
// mirroring PrivacyGuardianModal's itemized-decision shape (ElementDecision[])
// without any of its tier/span machinery, since a cross-Persona fact has
// neither.
//
// Controlled component, no backend calls of its own: ChatPane.tsx owns the
// commands.getPendingCrossPersonaConfirmations() query and the session-scoped
// confirmed/declined bookkeeping (per decisions.id=546/639, "per-session,
// non-persisted" -- nothing here is written to disk). This component only
// turns `facts` into per-fact decisions and hands them back via onResolve.
// field_value is never present on PendingCrossPersonaFact, matching
// EntityFact's own no-raw-values convention -- this modal only ever shows
// field_name/sensitivity/origin_persona_id.

import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { PendingCrossPersonaFact } from '../bindings'
import './CrossPersonaConfirmModal.css'

export interface CrossPersonaFactDecision {
  fact_id: string
  include: boolean
}

export interface CrossPersonaConfirmModalProps {
  open: boolean
  facts: PendingCrossPersonaFact[]
  onResolve: (decisions: CrossPersonaFactDecision[]) => void
  onCancel: () => void
}

export function CrossPersonaConfirmModal({
  open,
  facts,
  onResolve,
  onCancel,
}: CrossPersonaConfirmModalProps) {
  const { t } = useTranslation()
  // fact_id -> include. Local to this open/close cycle -- ChatPane resets
  // `facts` (by unmounting this branch, since `open` flips to false) between
  // prompts, so there's no cross-prompt staleness to guard against here.
  const [decisions, setDecisions] = useState<Record<string, boolean>>({})

  if (!open) return null

  const allDecided = facts.every((f) => decisions[f.fact_id] !== undefined)

  const setDecision = (factId: string, include: boolean) => {
    setDecisions((prev) => ({ ...prev, [factId]: include }))
  }

  const handleContinue = () => {
    const resolved = facts.map((f) => ({
      fact_id: f.fact_id,
      include: decisions[f.fact_id] ?? false,
    }))
    setDecisions({})
    onResolve(resolved)
  }

  const handleCancel = () => {
    setDecisions({})
    onCancel()
  }

  return (
    <div className="cross-persona-confirm-modal" role="alertdialog" aria-modal="true">
      <p className="cross-persona-confirm-modal__title">
        {t('navShell.chat.crossPersonaConfirmTitle')}
      </p>
      <p className="cross-persona-confirm-modal__body">
        {t('navShell.chat.crossPersonaConfirmBody')}
      </p>
      <ul className="cross-persona-confirm-modal__list">
        {facts.map((f) => (
          <li key={f.fact_id} className="cross-persona-confirm-modal__row">
            <span className="cross-persona-confirm-modal__field">
              {f.field_name}{' '}
              <span className="cross-persona-confirm-modal__sensitivity">
                ({f.sensitivity})
              </span>
            </span>
            <span className="cross-persona-confirm-modal__choices">
              <button
                type="button"
                aria-pressed={decisions[f.fact_id] === true}
                className={
                  decisions[f.fact_id] === true
                    ? 'cross-persona-confirm-modal__choice cross-persona-confirm-modal__choice--selected'
                    : 'cross-persona-confirm-modal__choice'
                }
                onClick={() => setDecision(f.fact_id, true)}
              >
                {t('navShell.chat.crossPersonaConfirmInclude')}
              </button>
              <button
                type="button"
                aria-pressed={decisions[f.fact_id] === false}
                className={
                  decisions[f.fact_id] === false
                    ? 'cross-persona-confirm-modal__choice cross-persona-confirm-modal__choice--selected'
                    : 'cross-persona-confirm-modal__choice'
                }
                onClick={() => setDecision(f.fact_id, false)}
              >
                {t('navShell.chat.crossPersonaConfirmOmit')}
              </button>
            </span>
          </li>
        ))}
      </ul>
      <div className="cross-persona-confirm-modal__footer">
        <button type="button" onClick={handleCancel}>
          {t('navShell.chat.crossPersonaConfirmCancel')}
        </button>
        <button type="button" disabled={!allDecided} onClick={handleContinue}>
          {t('navShell.chat.crossPersonaConfirmContinue')}
        </button>
      </div>
    </div>
  )
}
