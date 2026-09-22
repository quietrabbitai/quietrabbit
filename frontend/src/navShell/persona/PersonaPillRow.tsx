// items.id=543 (PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md Sections 2.2/
// 2.3): the pill-row variant, shared by Board and Library. Board's own
// mechanism (local personaFilter, fetch/merge) is unchanged by this item --
// this component only replaces its inline <fieldset> markup, visually
// separating the optional "All" pill and adding the read-only
// active-persona ring mark. Library mounts this with allowAll={false} (no
// merged view exists to build one against -- listOutputs requires a real
// personaId).

import { useTranslation } from 'react-i18next'
import type { PersonaInfo } from '../../bindings'
import { PersonaIdentity } from './PersonaIdentity'
import './persona.css'

export interface PersonaPillRowProps {
  personas: PersonaInfo[]
  /** The shared, app-wide active persona (NavState.activePersonaId) -- read
   *  only here, drives the ring mark. This component never writes it. */
  activePersonaId: string | null
  /** This row's own local selection (usePersonaLocalFilter's filterId) --
   *  independent of activePersonaId. */
  filterId: string | null
  onSelect: (id: string | null) => void
  /** Board: true. Library: false -- see this file's header comment. */
  allowAll: boolean
}

export function PersonaPillRow({
  personas,
  activePersonaId,
  filterId,
  onSelect,
  allowAll,
}: PersonaPillRowProps) {
  const { t } = useTranslation()

  if (personas.length === 0) return null

  return (
    <div
      className="persona-pill-row"
      role="group"
      aria-label={t('navShell.personaPillRow.groupLabel')}
    >
      {allowAll && (
        <>
          <button
            type="button"
            className="persona-pill-row__pill persona-pill-row__pill--all"
            data-selected={filterId === null ? '' : undefined}
            onClick={() => onSelect(null)}
          >
            {t('navShell.personaPillRow.allLabel')}
          </button>
          <span className="persona-pill-row__divider" aria-hidden="true" />
        </>
      )}
      {personas.map((persona) => (
        <button
          key={persona.id}
          type="button"
          className="persona-pill-row__pill"
          data-selected={filterId === persona.id ? '' : undefined}
          onClick={() => onSelect(persona.id)}
        >
          <PersonaIdentity persona={persona} size="sm" />
          {activePersonaId === persona.id && (
            <span
              className="persona-pill-row__active-mark"
              aria-hidden="true"
              title={t('navShell.personaPillRow.activeMarkTitle')}
            />
          )}
        </button>
      ))}
    </div>
  )
}
