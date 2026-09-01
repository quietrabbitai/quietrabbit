// The merged Active Board / QR Chat / Tier 3 screen -- items.id=384
// slice 3/4 (decisions.id=734-737). Replaces the old separate 'activeBoard'
// and 'tier3' content branches in NavShell.tsx with one screen laying
// Active Board and the QR Chat<->Tier 3 dominance pair (Tier3AccessPane;
// its own dominance state now lives in NavState.workspace.pair, passed
// through here as `pair`/`onUpdatePair` -- see useDominancePair.ts) as
// two regions whose relative space is controlled by decisions.id=737's
// three discrete Board size states:
//
//   full      -- Board only. Tier3AccessPane is UNMOUNTED entirely here,
//                not CSS-hidden -- it assumes a real mount/unmount
//                lifecycle for its CEF pane bookkeeping (its own
//                beforeunload/closeAllOpenPanes cleanup), which a
//                display:none toggle would silently bypass.
//   compact   -- both regions visible side by side. Board renders dense
//                (ActiveBoardPane's `dense` prop -- a single-column row
//                list, matching the reference mockup's board-full.compact
//                treatment) rather than its full card grid, which
//                decisions.id=737 says doesn't reflow gracefully at
//                reduced width.
//   minimized -- Tier3AccessPane only, at its full pre-merge size --
//                zero visual change from this screen's pre-merge
//                behavior. Board collapses to a single clickable row.
//
// decisions.id=735: Active Board is a separate, always-reachable anchor,
// not gated behind selecting a Persona first -- entering this screen at
// all has no Persona gate (see navShellConfig.ts's removal of the old
// isTier3Enabled check). activePersonaId flows through to Tier3AccessPane
// unchanged; a null value is already handled gracefully there (its own
// existing tier3ChatUnavailable message), so nothing new was needed to
// support arriving here with no Persona selected yet.

import { useTranslation } from 'react-i18next'
import type { PersonaInfo } from '../bindings'
import { ActiveBoardPane } from './ActiveBoardPane'
import { Tier3AccessPane } from './Tier3AccessPane'
import type { BoardSizeState, DominancePairState } from './navShellConfig'
import './WorkspaceShell.css'

export interface WorkspaceShellProps {
  userId: string
  activePersonaId: string | null
  onActivePersonaIdChange: (personaId: string) => void
  personas: PersonaInfo[]
  boardSize: BoardSizeState
  onBoardSizeChange: (size: BoardSizeState) => void
  pair: DominancePairState
  onUpdatePair: (updater: (prev: DominancePairState) => DominancePairState) => void
}

export function WorkspaceShell({
  userId,
  activePersonaId,
  onActivePersonaIdChange,
  personas,
  boardSize,
  onBoardSizeChange,
  pair,
  onUpdatePair,
}: WorkspaceShellProps) {
  const { t } = useTranslation()

  if (boardSize === 'minimized') {
    return (
      <div className="workspace-shell workspace-shell--minimized">
        <button
          type="button"
          className="workspace-shell__board-row"
          onClick={() => onBoardSizeChange('full')}
        >
          {t('navShell.workspaceShell.boardRowLabel')}
        </button>
        <div className="workspace-shell__pair-region">
          <Tier3AccessPane
            personaId={activePersonaId}
            onPersonaChange={onActivePersonaIdChange}
            personas={personas}
            pair={pair}
            onUpdatePair={onUpdatePair}
          />
        </div>
      </div>
    )
  }

  if (boardSize === 'compact') {
    return (
      <div className="workspace-shell workspace-shell--compact">
        <div className="workspace-shell__board-region">
          <button
            type="button"
            className="workspace-shell__board-size-button"
            onClick={() => onBoardSizeChange('minimized')}
          >
            {t('navShell.workspaceShell.minimizeBoardButton')}
          </button>
          <ActiveBoardPane userId={userId} dense />
        </div>
        <div className="workspace-shell__pair-region">
          <Tier3AccessPane
            personaId={activePersonaId}
            onPersonaChange={onActivePersonaIdChange}
            personas={personas}
            pair={pair}
            onUpdatePair={onUpdatePair}
          />
        </div>
      </div>
    )
  }

  return (
    <div className="workspace-shell workspace-shell--full">
      <button
        type="button"
        className="workspace-shell__pair-row"
        onClick={() => onBoardSizeChange('compact')}
      >
        {pair.openProviderIds.length > 0
          ? t('navShell.workspaceShell.pairRowLabelWithCount', {
              count: pair.openProviderIds.length,
            })
          : t('navShell.workspaceShell.pairRowLabel')}
      </button>
      <div className="workspace-shell__board-region">
        <ActiveBoardPane userId={userId} />
      </div>
    </div>
  )
}
