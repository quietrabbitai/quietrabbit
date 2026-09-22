// The 5-rail navigation dock -- Board / Chat / Cloud Chat / Library / History,
// exactly one dominant (fills the majority of the screen) at a time.
// items.id=404, generalizing items.id=391's "three peer bars, exactly one
// expanded" model (decisions.id=747) from 3 rails to 5. Full design:
// 03_ProjectDocs/Specifications/HIERARCHICAL_NAV_SHELL_DESIGN_20260902.md.
//
// The fixed vertical order (Board / Chat+Cloud Chat / Library / History, top to
// bottom) mirrors the old file's Board/Chat/Cloud Chat order, with Library and
// History appended below. Exactly one region is "dominant" (gets the big
// remaining space) at a time -- the rest render as compact bars, reusing
// `.cloud-chat-collapsed-strip` verbatim (CloudChatCollapsedStrip.css) the same way
// Board's own bar already did pre-404, so all four non-dominant bars read
// as the same kind of row.
//
// Chat and Cloud Chat are NOT two separate top-level regions in this file's own
// JSX -- they stay nested inside one CloudChatAccessPane mount, exactly as
// pre-404 (never unmounted, see that component's own header comment on
// why). What changes is what drives their split:
//   - `floor` (Chat's own passive-but-functional compact form) generalizes
//     from "true only while Board is expanded" to "true whenever neither
//     Chat nor Cloud Chat is the outer-dominant rail" -- Library or History
//     being dominant now ALSO floors Chat, which pre-404 was impossible
//     (there was no Library/History rail to be dominant instead).
//   - Cloud Chat's own compact form needed ZERO changes: CloudChatCollapsedStrip
//     already rendered unconditionally whenever Cloud Chat wasn't the expanded
//     region, regardless of floor -- that already IS the "plain inert
//     dock bar" the design doc asks for, just previously only reachable
//     via the Board/pair split.
//
// Two dominance REPRESENTATIONS now coexist and must be kept from
// drifting: `dominantRail` (this file's own prop, the one true source of
// truth for the outer 5-way choice) and `pair.dominant` (CloudChatAccessPane's
// own internal 'chat'|'cloudChat' echo, which its ~10 existing internal call
// sites still key off -- not rewritten for this item, too much surface for
// the value). The two are kept in sync by TWO complementary mechanisms,
// not one bidirectional effect (an earlier draft of this file tried a
// single effect mirroring pair.dominant back into dominantRail and it
// actively fought Board's-bar-reclaimChat, which sets pair.dominant='chat'
// as pure cleanup while deliberately promoting Board, not Chat, to
// dominant -- confirmed by tracing the click through by hand):
//   1. CloudChatAccessPane calls the new `onDominantRailChange` prop directly,
//      at the exact handful of ITS OWN internal call sites where a user
//      action genuinely means "make Chat/Cloud Chat the outer-dominant rail"
//      (CloudChatCollapsedStrip's expand, the content-head's reclaim click,
//      ChatPane's own non-floor collapsed-strip click) -- see that file's
//      own header comment for the full list. Board's bar (below, this
//      file) does the same directly for its own reclaimChat+dominantRail
//      pairing.
//   2. A one-directional correction effect (below): whenever
//      `dominantRail` IS 'chat' or 'cloudChat' but `pair.dominant` disagrees,
//      force `pair.dominant` to match. This is real, load-bearing
//      correctness, not just tidiness -- it's what stops a stale
//      pair.dominant='cloudChat' from silently showing Cloud Chat's rail+content
//      instead of Chat right after an EXTERNAL jump into Chat (History's
//      "Chat" row-action, or Chat's own floor-click), neither of which
//      goes through CloudChatAccessPane's own wrapped call sites.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import type { ChatInfo, PersonaInfo } from '../bindings'
import { ActiveBoardPane } from './ActiveBoardPane'
import { HistoryScreen, type HistoryOpenTarget } from './HistoryScreen'
import { LibraryPane } from '../library/LibraryPane'
import { CloudChatAccessPane } from './CloudChatAccessPane'
import { useDominancePair } from './useDominancePair'
import type { DockRailId, DominancePairState } from './navShellConfig'
import './CloudChatCollapsedStrip.css'
import './NavShell.css'
import './WorkspaceShell.css'

export interface WorkspaceShellProps {
  userId: string
  activePersonaId: string | null
  onActivePersonaIdChange: (personaId: string) => void
  personas: PersonaInfo[]
  dominantRail: DockRailId
  onDominantRailChange: (rail: DockRailId) => void
  boardDensity: 'full' | 'compact'
  onBoardDensityChange: (density: 'full' | 'compact') => void
  pair: DominancePairState
  onUpdatePair: (updater: (prev: DominancePairState) => DominancePairState) => void
}

export function WorkspaceShell({
  userId,
  activePersonaId,
  onActivePersonaIdChange,
  personas,
  dominantRail,
  onDominantRailChange,
  boardDensity,
  onBoardDensityChange,
  pair,
  onUpdatePair,
}: WorkspaceShellProps) {
  const { t } = useTranslation()

  // See this file's own header comment (mechanism 2): one-directional only
  // -- corrects pair.dominant to match dominantRail, never the reverse.
  useEffect(() => {
    if (dominantRail === 'chat' && pair.dominant !== 'chat') {
      onUpdatePair((prev) => ({ ...prev, dominant: 'chat' }))
    } else if (dominantRail === 'cloudChat' && pair.dominant !== 'cloudChat') {
      onUpdatePair((prev) => ({ ...prev, dominant: 'cloudChat' }))
    }
  }, [dominantRail, pair.dominant, onUpdatePair])

  // items.id=391 (tenth pass, preserved): a second, independent instance of
  // this hook -- see useDominancePair.ts's own shape, no internal state of
  // its own besides a local `openError` this file never reads. Lets
  // Board's own bar (below) call reclaimChat without lifting
  // CloudChatAccessPane's whole prop surface up into this file.
  const { reclaimChat } = useDominancePair(pair, onUpdatePair)

  // items.id=404's two cross-navigation bridges (design doc's Q1: "a
  // deliberate, context-derived jump action in both directions, not a
  // generic back-stack"). Both are one-shot signals, not persistent
  // NavState -- consumed once by the receiving side, then cleared, same
  // shape as the pre-existing onFloorExpand callback convention.
  const [pendingChatSelection, setPendingChatSelection] = useState<ChatInfo | null>(null)
  const [pendingHistoryTarget, setPendingHistoryTarget] = useState<HistoryOpenTarget | null>(null)

  const boardExpanded = dominantRail === 'board'

  return (
    <div className="workspace-shell" data-dominant-rail={dominantRail}>
      {boardExpanded ? (
        <div key="board-region" className="workspace-shell__board-region">
          <div className="cloud-chat-access-pane__section-header">
            <span className="cloud-chat-access-pane__section-header-name">
              {t('navShell.workspaceShell.boardBarLabel')}
            </span>
            <div className="cloud-chat-access-pane__section-header-controls">
              <button
                type="button"
                className="workspace-shell__board-density-button"
                onClick={() =>
                  onBoardDensityChange(boardDensity === 'compact' ? 'full' : 'compact')
                }
              >
                {boardDensity === 'compact'
                  ? t('navShell.workspaceShell.fullBoardButton')
                  : t('navShell.workspaceShell.compactBoardButton')}
              </button>
            </div>
          </div>
          <ActiveBoardPane
            userId={userId}
            dense={boardDensity === 'compact'}
            activePersonaId={activePersonaId}
          />
        </div>
      ) : (
        <button
          key="board-bar"
          type="button"
          className="cloud-chat-collapsed-strip"
          onClick={() => {
            reclaimChat()
            onDominantRailChange('board')
          }}
        >
          <span className="cloud-chat-collapsed-strip__name">
            {t('navShell.workspaceShell.boardBarLabel')}
          </span>
          <span className="cloud-chat-collapsed-strip__expand">
            {t('navShell.cloudChatCollapsedStrip.expandLabel')}
          </span>
        </button>
      )}

      <CloudChatAccessPane
        key="cloud-chat-access-pane"
        personaId={activePersonaId}
        onPersonaChange={onActivePersonaIdChange}
        personas={personas}
        pair={pair}
        onUpdatePair={onUpdatePair}
        floor={dominantRail !== 'chat' && dominantRail !== 'cloudChat'}
        onFloorExpand={() => onDominantRailChange('chat')}
        onDominantRailChange={onDominantRailChange}
        onOpenHistory={(target) => {
          setPendingHistoryTarget({ ...target, action: 'chatHistory' })
          onDominantRailChange('history')
        }}
        pendingChatSelection={pendingChatSelection}
        onPendingChatSelectionConsumed={() => setPendingChatSelection(null)}
      />

      {dominantRail === 'library' ? (
        <div key="library-region" className="workspace-shell__library-region">
          <div className="cloud-chat-access-pane__section-header">
            <span className="cloud-chat-access-pane__section-header-name">
              {t('navShell.library')}
            </span>
          </div>
          <LibraryPane userId={userId} personas={personas} activePersonaId={activePersonaId} />
        </div>
      ) : (
        <button
          key="library-bar"
          type="button"
          className="cloud-chat-collapsed-strip"
          onClick={() => onDominantRailChange('library')}
        >
          <span className="cloud-chat-collapsed-strip__name">{t('navShell.library')}</span>
          <span className="cloud-chat-collapsed-strip__expand">
            {t('navShell.cloudChatCollapsedStrip.expandLabel')}
          </span>
        </button>
      )}

      {dominantRail === 'history' ? (
        <div key="history-region" className="workspace-shell__history-region">
          <div className="cloud-chat-access-pane__section-header">
            <span className="cloud-chat-access-pane__section-header-name">
              {t('navShell.historyLabel')}
            </span>
          </div>
          <HistoryScreen
            userId={userId}
            personas={personas}
            activePersonaId={activePersonaId}
            onOpenPersonaChat={(personaId, chat) => {
              onActivePersonaIdChange(personaId)
              if (chat) setPendingChatSelection(chat)
              onDominantRailChange('chat')
            }}
            onOpenFullLibrary={(personaId) => {
              onActivePersonaIdChange(personaId)
              onDominantRailChange('library')
            }}
            openTarget={pendingHistoryTarget}
            onOpenTargetConsumed={() => setPendingHistoryTarget(null)}
          />
        </div>
      ) : (
        <button
          key="history-bar"
          type="button"
          className="cloud-chat-collapsed-strip"
          onClick={() => onDominantRailChange('history')}
        >
          <span className="cloud-chat-collapsed-strip__name">{t('navShell.historyLabel')}</span>
          <span className="cloud-chat-collapsed-strip__expand">
            {t('navShell.cloudChatCollapsedStrip.expandLabel')}
          </span>
        </button>
      )}
    </div>
  )
}
