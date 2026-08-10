// Persona hub screen -- IA spec Section 4. Opened by tapping a Persona
// button; not a Focus, not a chat directly -- a control panel for
// everything scoped to that Persona.
//
// Focus list: real IPC (commands.listFocuses), but rendered flat, not
// grouped by lifecycle state (Active/Paused/Hibernated per Section 4).
// FocusInfo has no lifecycle field to group by yet -- documented IPC gap
// (commands/persona.rs: "list_focuses IPC gap (post-Release 1): ...
// dormancy state is not in focus_settings"). Grouping is not invented
// here; the flat list is an honest reflection of what the backend can
// currently report, not a design choice.
//
// Action row (Section 4, "extensible, NOT a fixed set. Confirmed members
// from this session: Library... Focus Builder... Fork/duplicate"): all
// three are shown since the spec confirms their slot, but only Library
// is wired -- Focus Builder and Fork/duplicate have no built screen
// behind them (getFocusBuilderSession/submitFocusBuilderStep are still
// `not_implemented` stubs), so they render disabled rather than being
// omitted outright.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type FocusInfo } from '../bindings'

export interface PersonaHubProps {
  userId: string
  personaId: string
  keyHex: string | null
  onOpenLibrary: () => void
}

export function PersonaHub({ userId, personaId, keyHex, onOpenLibrary }: PersonaHubProps) {
  const { t } = useTranslation()
  const [focuses, setFocuses] = useState<FocusInfo[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    setFocuses([])
    setError(null)
    commands.listFocuses(userId, personaId, keyHex ?? '').then((result) => {
      if (result.status === 'ok') {
        setFocuses(result.data)
      } else {
        setError(result.error)
      }
    })
  }, [userId, personaId, keyHex])

  return (
    <div className="persona-hub">
      <h2>{t('navShell.personaHub.focusesHeading')}</h2>
      {error && (
        <p role="alert">
          {t('navShell.personaHub.focusLoadError', { message: error })}
        </p>
      )}
      {focuses.length === 0 && !error && (
        <p>{t('navShell.personaHub.noFocuses')}</p>
      )}
      {focuses.length > 0 && (
        <ul className="persona-hub__focus-list">
          {focuses.map((focus) => (
            <li key={focus.focus_id}>{focus.focus_id}</li>
          ))}
        </ul>
      )}

      <fieldset className="persona-hub__action-row">
        <legend>{t('navShell.personaHub.actionRowLabel')}</legend>
        <button type="button" onClick={onOpenLibrary}>
          {t('navShell.library')}
        </button>
        <button type="button" disabled>
          {t('navShell.personaHub.focusBuilder')}
        </button>
        <button type="button" disabled>
          {t('navShell.personaHub.forkDuplicate')}
        </button>
      </fieldset>
    </div>
  )
}
