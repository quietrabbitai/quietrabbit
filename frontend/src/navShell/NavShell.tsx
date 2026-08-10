// Real top-strip / navigation-shell -- IA spec Section 2, replacing
// App.tsx's former TEMPORARY HARNESS (items.id=3/202/223) per items.id=232.
//
// Scope of this file: the top strip itself (2a/2c/2d/2e/2f), the
// selected-state persistent-orientation mechanism (Section 2's own
// doubling as orientation signal -- no separate title bar), Persona
// switching, and routing the middle zone to whatever the strip (or a
// deeper navigation action) has dispatched (Section 1). MiddleZone and
// Tier3Selector are re-hosted, not rebuilt -- see Tier3AccessPane.tsx and
// PersonaHub.tsx for where each actually mounts.
//
// Explicitly NOT built here (flagged, not silently skipped):
// - The outbound Privacy Guardian gate that must precede Tier 3 access
//   in the real flow (items.id=233) -- separate item, blocked on this one.
// - Active Board's real screen (high-priority section + full list,
//   Section 2a/2b) -- a genuine, separately-scoped feature. Additionally
//   cannot be real yet regardless of scope: commands.getActiveBoard
//   requires key_hex, which has no placeholder equivalent to
//   getPlaceholderUserId() (see navShellConfig.ts's note on why that gap
//   is deliberately NOT bridged the same way). Renders as flagged
//   placeholder content below. Library's real screen (Section 2c) is
//   built (see LibraryPane.tsx) -- same key_hex gap, but the screen
//   itself short-circuits per-call rather than staying an unbuilt
//   placeholder; see LibraryPane.tsx's own header comment.
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
import { PersonaHub } from './PersonaHub'
import { Tier3AccessPane } from './Tier3AccessPane'
import {
  DEFAULT_NAV_STATE,
  FIXED_BUTTON_ORDER,
  currentContent,
  fixedButtonLitState,
  getPlaceholderUserId,
  isPersonaAnchor,
  isTier3Enabled,
  pushCrumb,
  selectAnchor,
  selectCrumb,
  type ContentDescriptor,
  type FixedButtonId,
  type NavState,
} from './navShellConfig'

export function NavShell() {
  const { t } = useTranslation()
  const [navState, setNavState] = useState<NavState>(DEFAULT_NAV_STATE)
  const [personas, setPersonas] = useState<PersonaInfo[]>([])
  const [personaError, setPersonaError] = useState<string | null>(null)
  // The persona a Tier 3 session was opened from. isTier3Enabled requires
  // the CURRENT anchor to be a Persona, but selectAnchor({kind:'fixed',
  // id:'tier3'}) discards that persona from navState.anchor on the very
  // same transition -- NavState has no room to carry it through (AnchorId's
  // 'fixed' variant isn't per-button-parameterized). Tier3AccessPane needs
  // a real personaId for persistence (every store in this codebase is
  // opened per user/persona -- no exceptions), so this is captured here,
  // once, right before the anchor switches.
  const [tier3PersonaId, setTier3PersonaId] = useState<string | null>(null)

  useEffect(() => {
    commands.listPersonas(getPlaceholderUserId()).then((result) => {
      if (result.status === 'ok') {
        setPersonas(result.data)
      } else {
        setPersonaError(result.error)
      }
    })
  }, [])

  const handleSelectFixed = (id: FixedButtonId) => {
    if (id === 'tier3') {
      if (!isTier3Enabled(navState)) return
      // isTier3Enabled guarantees anchor.kind === 'persona' here.
      if (navState.anchor.kind === 'persona') {
        setTier3PersonaId(navState.anchor.personaId)
      }
    }
    setNavState(selectAnchor({ kind: 'fixed', id }))
  }

  const handleSelectPersona = (personaId: string) => {
    setNavState(selectAnchor({ kind: 'persona', personaId }))
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
          const disabled = id === 'tier3' && !isTier3Enabled(navState)
          return (
            <button
              key={id}
              type="button"
              className="nav-shell__button"
              data-selected={lit === 'none' ? undefined : lit}
              disabled={disabled}
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
          tier3PersonaId={tier3PersonaId}
        />
      </div>
    </main>
  )
}

interface NavShellContentProps {
  content: ContentDescriptor
  onOpenPersonaLibrary: (personaId: string) => void
  tier3PersonaId: string | null
}

/** Resolves the current ContentDescriptor to what actually mounts in the
 *  middle zone. Section 3: content and its chat share one MiddleZone
 *  instance, keyed by contextKey so switching content switches which
 *  transcript is showing (3b) -- except 'tier3', which has its own
 *  side-by-side layout requirement (Section 9); see Tier3AccessPane. */
function NavShellContent({
  content,
  onOpenPersonaLibrary,
  tier3PersonaId,
}: NavShellContentProps) {
  const { t } = useTranslation()
  const [personaHubGenerating, setPersonaHubGenerating] = useState(false)

  if (content.type === 'tier3') {
    return <Tier3AccessPane personaId={tier3PersonaId} />
  }

  if (content.type === 'personaHub') {
    return (
      <MiddleZone
        contextKey={`persona-hub-${content.personaId}`}
        profile={DEFAULT_BROWSING_PROFILE}
        isGenerating={personaHubGenerating}
        contextPane={
          <PersonaHub
            personaId={content.personaId}
            onOpenLibrary={() => onOpenPersonaLibrary(content.personaId)}
          />
        }
        chatPane={
          <ChatPane
            contextKey={`persona-hub-${content.personaId}`}
            userId={getPlaceholderUserId()}
            personaId={content.personaId}
            keyHex={null}
            focusId="quick-ask"
            gate3Track={false}
            onGenerating={setPersonaHubGenerating}
          />
        }
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
            userId={getPlaceholderUserId()}
            personaId={personaId}
            keyHex={null}
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
    case 'activeBoard':
      return t('navShell.content.activeBoardPlaceholder')
    case 'myFacts':
      return t('navShell.content.myFactsPlaceholder')
    default:
      return ''
  }
}
