// Real top-strip / navigation-shell -- IA spec Section 2, replacing
// App.tsx's former TEMPORARY HARNESS (items.id=3/202/223) per items.id=232.
//
// Scope of this file: the top strip itself (2a/2c/2d/2e/2f), the
// selected-state persistent-orientation mechanism (Section 2's own
// doubling as orientation signal -- no separate title bar), Persona
// switching, and routing the middle zone to whatever the strip (or a
// deeper navigation action) has dispatched (Section 1). MiddleZone and
// Tier3Selector are re-hosted, not rebuilt -- see Tier3AccessPane.tsx and
// PersonaHub.tsx for where each actually mounts. The merged Board/Chat/
// Tier3 workspace (items.id=384, decisions.id=734-743) is WorkspaceShell.tsx,
// mounted here exactly like any other content branch.
//
// Explicitly NOT built here (flagged, not silently skipped):
// - The outbound Privacy Guardian gate that must precede Tier 3 access
//   in the real flow (items.id=233) -- separate item, blocked on this one.
// - My Facts' real screen (items.id=176, Chat-BRAND's design).
// - Onboarding's strip-wide gating (Section 11b) -- not built because
//   nothing in this pass triggers Onboarding at all.
// - Any Chat-BRAND visual pass -- structural/behavioral only, same
//   discipline as middleZone/ and tier3Access/.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import './NavShell.css'
import { commands, type PersonaInfo } from '../bindings'
import { ChatPane } from '../chat/ChatPane'
import { LibraryPane } from '../library/LibraryPane'
import { MiddleZone } from '../middleZone/MiddleZone'
import { DEFAULT_BROWSING_PROFILE } from '../middleZone/middleZoneConfig'
import { FocusSettingsPane } from './FocusSettingsPane'
import { PersonaHub } from './PersonaHub'
import { WorkspaceShell } from './WorkspaceShell'
import {
  DEFAULT_NAV_STATE,
  FIXED_BUTTON_ORDER,
  currentContent,
  fixedButtonLitState,
  getCurrentUserId,
  requireCurrentUserId,
  isPersonaAnchor,
  pushCrumb,
  selectFixed,
  selectPersona,
  selectCrumb,
  setActivePersonaId,
  setBoardSize,
  updateWorkspacePair,
  type BoardSizeState,
  type ContentDescriptor,
  type DominancePairState,
  type FixedButtonId,
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

  const handleSelectFixed = (id: FixedButtonId) => {
    setNavState(selectFixed(id))
  }

  const handleSelectPersona = (personaId: string) => {
    setNavState(selectPersona(personaId))
  }

  const handleSetActivePersonaId = (personaId: string) => {
    setNavState(setActivePersonaId(personaId))
  }

  const handleSetBoardSize = (size: BoardSizeState) => {
    setNavState(setBoardSize(size))
  }

  const handleUpdatePair = (
    updater: (prev: DominancePairState) => DominancePairState,
  ) => {
    setNavState(updateWorkspacePair(updater))
  }

  const handleOpenPersonaLibrary = (personaId: string) => {
    setNavState((prev) =>
      pushCrumb(prev, {
        id: `library-${personaId}`,
        labelKey: 'navShell.library',
        aliasesFixedButton: 'library',
        content: { type: 'library', personaFilter: personaId },
      }),
    )
  }

  const handleOpenFocusSettings = (personaId: string, focusId: string) => {
    setNavState((prev) =>
      pushCrumb(prev, {
        id: `focus-settings-${personaId}-${focusId}`,
        labelKey: 'navShell.focusSettings.crumbLabel',
        content: { type: 'focusSettings', personaId, focusId },
      }),
    )
  }

  const handleSelectCrumb = (crumbId: string) => {
    setNavState((prev) => selectCrumb(prev, crumbId))
  }

  const content = currentContent(navState)

  return (
    <main className="nav-shell">
      <nav
        className="nav-shell__top-strip"
        aria-label={t('navShell.topStripLabel')}
      >
        {FIXED_BUTTON_ORDER.map((id) => {
          const lit = fixedButtonLitState(navState, id)
          return (
            <button
              key={id}
              type="button"
              className="nav-shell__button"
              data-selected={lit === 'none' ? undefined : lit}
              onClick={() => handleSelectFixed(id)}
            >
              {t(`navShell.${id}`)}
            </button>
          )
        })}

        <fieldset className="nav-shell__persona-cluster">
          <legend>{t('navShell.personaClusterLabel')}</legend>
          {personaError && (
            <span role="alert">
              {t('navShell.personaLoadError', { message: personaError })}
            </span>
          )}
          {personas.map((persona) => (
            <button
              key={persona.id}
              type="button"
              className="nav-shell__button nav-shell__persona-button"
              data-selected={
                isPersonaAnchor(navState, persona.id) ? 'anchor' : undefined
              }
              onClick={() => handleSelectPersona(persona.id)}
            >
              {/* Section 2d / decisions.id=654: Persona-color-dot-on-button.
                  PersonaInfo has no color field yet -- documented IPC gap
                  (commands/persona.rs: "color... in personas.extra_metadata
                  (not yet parsed)"). Slot reserved, unstyled, no color
                  assigned -- Jason 2026-08-09: inventing one here would be
                  exactly the unilateral visual/brand call this build isn't
                  scoped to make. */}
              <span
                className="nav-shell__persona-color-dot"
                aria-hidden="true"
              />
              {persona.display_name}
            </button>
          ))}
        </fieldset>

        {navState.chain.map((crumb) => (
          <button
            key={crumb.id}
            type="button"
            className="nav-shell__button nav-shell__temporary-button"
            data-selected="anchor"
            onClick={() => handleSelectCrumb(crumb.id)}
          >
            {t(crumb.labelKey)}
          </button>
        ))}
      </nav>

      <div className="nav-shell__content">
        <NavShellContent
          content={content}
          onOpenPersonaLibrary={handleOpenPersonaLibrary}
          onOpenFocusSettings={handleOpenFocusSettings}
          activePersonaId={navState.activePersonaId}
          onActivePersonaIdChange={handleSetActivePersonaId}
          personas={personas}
          boardSize={navState.workspace.boardSize}
          onBoardSizeChange={handleSetBoardSize}
          pair={navState.workspace.pair}
          onUpdatePair={handleUpdatePair}
        />
      </div>
    </main>
  )
}

interface NavShellContentProps {
  content: ContentDescriptor
  onOpenPersonaLibrary: (personaId: string) => void
  onOpenFocusSettings: (personaId: string, focusId: string) => void
  /** items.id=384 slice 1: replaces the old tier3PersonaId capture --
   *  activePersonaId is now a standing NavState field that survives the
   *  switch into the merged workspace on its own, so there's nothing
   *  left to capture-on-transition here. */
  activePersonaId: string | null
  /** items.id=384 slice 7: the new-chat persona dot-picker's "quiet"
   *  switch (setActivePersonaId, not selectPersona) -- see that
   *  function's own doc comment in navShellConfig.ts. */
  onActivePersonaIdChange: (personaId: string) => void
  /** Already fetched once at NavShell's own top level for the persona
   *  cluster buttons -- threaded down here rather than having
   *  Tier3AccessPane re-fetch the same list a second time. */
  personas: PersonaInfo[]
  boardSize: BoardSizeState
  onBoardSizeChange: (size: BoardSizeState) => void
  pair: DominancePairState
  onUpdatePair: (updater: (prev: DominancePairState) => DominancePairState) => void
}

/** Resolves the current ContentDescriptor to what actually mounts in the
 *  middle zone. Section 3: content and its chat share one MiddleZone
 *  instance, keyed by contextKey so switching content switches which
 *  transcript is showing (3b) -- except 'workspace', which has its own
 *  layout requirement (Section 9, decisions.id=734-737); see
 *  WorkspaceShell. */
function NavShellContent({
  content,
  onOpenPersonaLibrary,
  onOpenFocusSettings,
  activePersonaId,
  onActivePersonaIdChange,
  personas,
  boardSize,
  onBoardSizeChange,
  pair,
  onUpdatePair,
}: NavShellContentProps) {
  const { t } = useTranslation()
  const [personaHubGenerating, setPersonaHubGenerating] = useState(false)

  if (content.type === 'workspace') {
    return (
      <WorkspaceShell
        userId={requireCurrentUserId()}
        activePersonaId={activePersonaId}
        onActivePersonaIdChange={onActivePersonaIdChange}
        personas={personas}
        boardSize={boardSize}
        onBoardSizeChange={onBoardSizeChange}
        pair={pair}
        onUpdatePair={onUpdatePair}
      />
    )
  }

  if (content.type === 'personaHub') {
    return (
      <MiddleZone
        contextKey={`persona-hub-${content.personaId}`}
        profile={DEFAULT_BROWSING_PROFILE}
        isGenerating={personaHubGenerating}
        contextPane={
          <PersonaHub
            userId={requireCurrentUserId()}
            personaId={content.personaId}
            onOpenLibrary={() => onOpenPersonaLibrary(content.personaId)}
            onOpenFocusSettings={(focusId) =>
              onOpenFocusSettings(content.personaId, focusId)
            }
          />
        }
        chatPane={
          <ChatPane
            contextKey={`persona-hub-${content.personaId}`}
            userId={requireCurrentUserId()}
            personaId={content.personaId}
            focusId="quick-ask"
            gate3Track={false}
            onGenerating={setPersonaHubGenerating}
          />
        }
      />
    )
  }

  if (content.type === 'focusSettings') {
    return (
      <MiddleZone
        contextKey={`focus-settings-${content.personaId}-${content.focusId}`}
        profile={DEFAULT_BROWSING_PROFILE}
        isGenerating={false}
        contextPane={
          <FocusSettingsPane
            userId={requireCurrentUserId()}
            personaId={content.personaId}
            focusId={content.focusId}
          />
        }
        chatPane={<p>{t('navShell.content.chatPlaceholder')}</p>}
      />
    )
  }

  if (content.type === 'library') {
    const personaId = content.personaFilter ?? null
    const contextKey = personaId ? `library-${personaId}` : 'library'
    return (
      <MiddleZone
        contextKey={contextKey}
        profile={DEFAULT_BROWSING_PROFILE}
        isGenerating={false}
        contextPane={
          <LibraryPane
            userId={requireCurrentUserId()}
            personaId={personaId}
          />
        }
        chatPane={<p>{t('navShell.content.chatPlaceholder')}</p>}
      />
    )
  }

  return (
    <MiddleZone
      contextKey={content.type}
      profile={DEFAULT_BROWSING_PROFILE}
      isGenerating={false}
      contextPane={<p>{describePlaceholder(content, t)}</p>}
      chatPane={<p>{t('navShell.content.chatPlaceholder')}</p>}
    />
  )
}

function describePlaceholder(
  content: ContentDescriptor,
  t: (key: string) => string,
): string {
  switch (content.type) {
    case 'myFacts':
      return t('navShell.content.myFactsPlaceholder')
    default:
      return ''
  }
}
