// Navigation shell -- items.id=404 rework. Previously IA_SPEC Section 2's
// flat top strip (fixed buttons + Persona cluster + temporary-button
// chain) dispatching to whatever ContentDescriptor currentContent()
// resolved. That entire model is superseded by
// Specifications/HIERARCHICAL_NAV_SHELL_DESIGN_20260902.md: there is no
// flat top strip in the target design at all. This file now just fetches
// the two small pieces of shared data every rail needs (personas, the
// current session id) and mounts WorkspaceShell -- the 5-rail dock -- as
// the only thing this screen ever shows.
//
// What used to live here and is now gone, not just unwired: the top-strip
// JSX itself, the Persona-button cluster, the temporary-button/breadcrumb
// chain, and NavShellContent's whole dispatch-by-ContentDescriptor
// function. Nothing in the new UI can construct a personaHub/focusSettings
// destination any more (PersonaHub.tsx's own action-row callbacks were the
// only thing that ever did) -- see navShellConfig.ts's own header comment
// for the full reasoning and what got deleted vs. what didn't.
//
// PersonaHub.tsx and FocusSettingsPane.tsx themselves are UNTOUCHED, just
// unimported here now -- their disposition is items.id=400's scope, not
// this item's. Concrete consequence: there is currently no UI path to
// view a Persona's Focus list at all. Flagged in this item's session
// handoff, confirmed acceptable by Jason for this build (temporary, until
// items.id=400's chain lands) -- not silently absorbed or worked around.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import './NavShell.css'
import { commands, type PersonaInfo } from '../bindings'
import { WorkspaceShell } from './WorkspaceShell'
import {
  DEFAULT_NAV_STATE,
  getCurrentUserId,
  requireCurrentUserId,
  setActivePersonaId,
  setBoardDensity,
  setDominantRail,
  updateWorkspacePair,
  type DockRailId,
  type DominancePairState,
  type NavState,
} from './navShellConfig'

export function NavShell() {
  const { t } = useTranslation()
  const [navState, setNavState] = useState<NavState>(DEFAULT_NAV_STATE)
  const [personas, setPersonas] = useState<PersonaInfo[]>([])
  const [personaError, setPersonaError] = useState<string | null>(null)

  useEffect(() => {
    // NavShell only mounts once App.tsx's login gate has confirmed a
    // session, so this should always be non-null in practice -- guarded
    // anyway rather than assumed, since this effect doesn't itself know
    // that invariant holds.
    const userId = getCurrentUserId()
    if (!userId) return
    commands.listPersonas(userId).then((result) => {
      if (result.status === 'ok') {
        setPersonas(result.data)
      } else {
        setPersonaError(result.error)
      }
    })
  }, [])

  const handleSetActivePersonaId = (personaId: string) => {
    setNavState(setActivePersonaId(personaId))
  }

  const handleSetDominantRail = (rail: DockRailId) => {
    setNavState(setDominantRail(rail))
  }

  const handleSetBoardDensity = (density: 'full' | 'compact') => {
    setNavState(setBoardDensity(density))
  }

  const handleUpdatePair = (
    updater: (prev: DominancePairState) => DominancePairState,
  ) => {
    setNavState(updateWorkspacePair(updater))
  }

  return (
    <main className="nav-shell">
      {personaError && (
        <p role="alert" className="nav-shell__persona-load-error">
          {t('navShell.personaLoadError', { message: personaError })}
        </p>
      )}
      <div className="nav-shell__content">
        <WorkspaceShell
          userId={requireCurrentUserId()}
          activePersonaId={navState.activePersonaId}
          onActivePersonaIdChange={handleSetActivePersonaId}
          personas={personas}
          dominantRail={navState.workspace.dominantRail}
          onDominantRailChange={handleSetDominantRail}
          boardDensity={navState.workspace.boardDensity}
          onBoardDensityChange={handleSetBoardDensity}
          pair={navState.workspace.pair}
          onUpdatePair={handleUpdatePair}
        />
      </div>
    </main>
  )
}
