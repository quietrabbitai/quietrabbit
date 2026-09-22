// items.id=543: shared icon+name identity primitive for the three new
// persona-selection surfaces (PersonaBox, PersonaPillRow, and the box's own
// selector popover) -- PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md Section
// 2.4: no color is used for persona identity on any of these new controls
// (existing color-dot usages elsewhere -- Board's card border,
// NewChatPersonaPicker's dot -- are untouched, this just doesn't extend
// color to what's added here).
//
// PersonaInfo (bindings.ts) has no dedicated icon field yet -- a real
// icon-picker is split out as its own item (items.id=544), not built here.
// A neutral first-letter avatar derived from display_name stands in, same
// placeholder the design session's own mockup uses.

import type { PersonaInfo } from '../../bindings'

export interface PersonaIdentityProps {
  persona: PersonaInfo | null
  size?: 'sm' | 'md'
}

export function PersonaIdentity({ persona, size = 'md' }: PersonaIdentityProps) {
  const initial = persona?.display_name.trim().charAt(0).toUpperCase() ?? ''
  return (
    <span className="persona-identity" data-size={size}>
      <span
        className="persona-identity__icon"
        data-empty={persona ? undefined : ''}
        aria-hidden="true"
      >
        {initial}
      </span>
      {persona && <span className="persona-identity__name">{persona.display_name}</span>}
    </span>
  )
}
