// New-chat persona dot-picker -- items.id=384 slice 7, decisions.id=740:
// "single-action dot-picker with name and color," reusing the Active
// Board card convention (persona-color-coded, not color-alone -- color
// alone was judged prone to confusion).
//
// SINGLE ACTION, per the decision's own title: there is no separate "New
// chat" button anywhere in this build. Clicking ANY pill here -- including
// the currently-active Persona's own pill -- starts a fresh chat under
// that Persona in one click. This is a deliberate reading of "single-
// action": the alternative (pick a Persona first, then press a separate
// confirm/New-chat button) would be two actions, not one. Browsing PAST
// chats is a completely separate affordance (ChatHistoryList, this same
// toolbar) -- this component only ever creates new ones.

import { useTranslation } from 'react-i18next'
import type { PersonaInfo } from '../bindings'
import './NewChatPersonaPicker.css'

export interface NewChatPersonaPickerProps {
  personas: PersonaInfo[]
  activePersonaId: string | null
  onStartNewChat: (persona: PersonaInfo) => void
  disabled?: boolean
}

export function NewChatPersonaPicker({
  personas,
  activePersonaId,
  onStartNewChat,
  disabled = false,
}: NewChatPersonaPickerProps) {
  const { t } = useTranslation()

  if (personas.length === 0) return null

  return (
    <div
      className="new-chat-persona-picker"
      role="group"
      aria-label={t('navShell.newChatPersonaPicker.groupLabel')}
    >
      {personas.map((persona) => (
        <button
          key={persona.id}
          type="button"
          className="new-chat-persona-picker__pill"
          data-selected={persona.id === activePersonaId ? '' : undefined}
          disabled={disabled}
          onClick={() => onStartNewChat(persona)}
          title={t('navShell.newChatPersonaPicker.buttonTitle', {
            persona: persona.display_name,
          })}
        >
          <span
            className="new-chat-persona-picker__dot"
            style={persona.color ? { background: persona.color } : undefined}
            aria-hidden="true"
          />
          {persona.display_name}
        </button>
      ))}
    </div>
  )
}
