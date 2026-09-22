// items.id=543 (PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md Section 2.2/2.3):
// the local-filter-with-fallback mechanism Board already had inline
// (ActiveBoardPane.tsx's pre-existing personaFilter useState), factored out
// so Library can mirror it exactly and a future persona-scoped screen (Focus
// Builder, named explicitly in the design doc) can reuse it too. Takes no
// NavState dependency -- just props -- same "controlled hook" shape as this
// file's neighbor useDominancePair.ts.
//
// allowAll distinguishes the two real callers: Board defaults to null
// ("All", unchanged from its pre-existing default) and offers an All pill;
// Library has no All pill (listOutputs requires a non-null personaId, no
// merged view exists to build one against) and instead defaults its local
// filter from activePersonaId on first mount, per the design's "first-open
// default" (Section 2.2) -- this is a lazy useState initializer, not an
// effect, so it only ever applies once per mount and never resyncs
// afterward, which is the entire point of decoupling this from
// activePersonaId.

import { useState } from 'react'

export interface UsePersonaLocalFilterResult {
  /** null means "All" (only meaningful when allowAll) or "nothing picked
   *  yet" (Library, before any selection or default). */
  filterId: string | null
  /** Mirrors the mockup's Board/Library tabs' identical click logic
   *  (PERSONA_SELECTOR_MOCKUP_ITEM543_20260921.html): selecting the
   *  already-selected id (including null/"All") falls back to whatever's
   *  active elsewhere (activePersonaId), or stays put if nothing is active
   *  anywhere to fall back to. Selecting anything else just selects it. */
  select: (id: string | null) => void
}

export function usePersonaLocalFilter(
  activePersonaId: string | null,
  allowAll: boolean,
): UsePersonaLocalFilterResult {
  const [filterId, setFilterId] = useState<string | null>(() =>
    allowAll ? null : activePersonaId,
  )

  const select = (id: string | null) => {
    setFilterId(id === filterId ? activePersonaId : id)
  }

  return { filterId, select }
}
