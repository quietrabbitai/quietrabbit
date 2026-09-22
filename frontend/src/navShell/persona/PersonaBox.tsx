// items.id=543 (PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md Section 2.1):
// Chat's in-bar box -- empty (dashed, no icon, chevron) / filled (icon+
// name+chevron) states, opens a forced compact selector popover. Controlled
// open state (open/onOpenChange), not internal -- CloudChatAccessPane.tsx
// needs to open the SAME popover from two triggers (the box itself, and
// clicking Chat's empty rail body), and a future persona-scoped screen
// (Focus Builder, named explicitly in the design doc) may want the same.

import { useTranslation } from 'react-i18next'
import type { PersonaInfo } from '../../bindings'
import { PersonaIdentity } from './PersonaIdentity'
import './persona.css'

export interface PersonaBoxProps {
  activePersonaId: string | null
  personas: PersonaInfo[]
  onChange: (persona: PersonaInfo) => void
  open: boolean
  onOpenChange: (open: boolean) => void
  disabled?: boolean
}

export function PersonaBox({
  activePersonaId,
  personas,
  onChange,
  open,
  onOpenChange,
  disabled = false,
}: PersonaBoxProps) {
  const { t } = useTranslation()
  const activePersona = personas.find((p) => p.id === activePersonaId) ?? null

  if (personas.length === 0) return null

  return (
    <div className="persona-box-wrap">
      <button
        type="button"
        className="persona-box"
        data-empty={activePersona ? undefined : ''}
        disabled={disabled}
        aria-expanded={open}
        aria-label={t('navShell.personaBox.buttonLabel')}
        onClick={() => onOpenChange(!open)}
      >
        <PersonaIdentity persona={activePersona} size="sm" />
        <span className="persona-box__chevron" aria-hidden="true">
          ▾
        </span>
      </button>
      {open && (
        <div
          className="persona-selector-popover"
          role="group"
          aria-label={t('navShell.personaBox.popoverGroupLabel')}
        >
          <p className="persona-selector-popover__title">
            {activePersona
              ? t('navShell.personaBox.popoverTitleSwitch')
              : t('navShell.personaBox.popoverTitleChoose')}
          </p>
          {personas.map((persona) => (
            <button
              key={persona.id}
              type="button"
              className="persona-selector-popover__option"
              onClick={() => {
                onChange(persona)
                onOpenChange(false)
              }}
            >
              <PersonaIdentity persona={persona} size="sm" />
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
