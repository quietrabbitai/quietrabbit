// The merged Active Board / QR Chat / Tier 3 screen -- items.id=384
// slice 3/4, rebuilt for items.id=391.
//
// items.id=391 (Jason, 2026-09-02, tenth pass -- the "three bars, one
// expanded" redesign): supersedes this file's own prior model (a fixed
// vertical Board/Chat/Tier3 order with exactly TWO of three ever visible,
// the third fully hidden). Jason's own words, reacting to the bottom
// second-opinion bar built the pass before this one: "What you did on the
// main screen listing the second opinion bar was not what I was
// expecting, but I like it better than the buttons. What if instead of an
// active board button, we added a row at the top for active board (like
// the second opinion bar at the bottom) and extended the Quiet Rabbit --
// this conversation header to be the same size as the other two bars...
// three rows with the chat expanded and the ability to select either of
// the other two rows to expand that instead." Confirmed for all three
// starting points (chat expanded, second-opinion expanded, board
// expanded) -- in every case, the OTHER TWO of {Board, Chat, Tier3} show
// as a bar, never hidden outright.
//
// The fixed vertical order (Board / Chat / Tier3, top to bottom) is
// UNCHANGED -- Jason's original correction from the first rebuild still
// holds, this pass only changes what a non-expanded region looks like
// (a full-width bar, always present) instead of whether it's there at
// all. Concretely, exactly ONE of the three is "expanded" (gets the big
// remaining space) at a time:
//   - Board expanded  -- boardSize !== 'minimized'. ActiveBoardPane fills
//     the region (density from boardSize, 'full' vs 'compact' -- now
//     purely a Board-owned density preference, no longer a distinct
//     growth stage; see BoardSizeState's own doc comment). Chat is
//     compressed to its own floor row (Tier3AccessPane's `floor` prop),
//     AND the second-opinion bar renders below it, unconditionally --
//     the two "other" rows, both present as bars.
//   - Chat expanded   -- boardSize === 'minimized' AND pair.dominant ===
//     'chat'. QR renders full-size inside Tier3AccessPane; Board's own
//     bar (this file, below) and the second-opinion bar (Tier3AccessPane)
//     both show.
//   - Tier3 expanded  -- boardSize === 'minimized' AND pair.dominant ===
//     'tier3'. The rail+content-pane fills the region; Board's own bar
//     (this file) and QR's own compressed row (Tier3AccessPane) both show.
//
// The three-step Board/Chat GROWTH sequence this file used to implement
// (decisions.id=737's three sizes driving full -> compact -> minimized as
// successive "chat grows, board shrinks" stages) is REMOVED, not just
// renamed -- Jason's own description of this pass is a single-step swap
// ("select ... the other row to expand that instead"), not a multi-stage
// growth. 'compact' now means only "Board expanded, dense density" --
// see BoardSizeState's own updated doc comment.
//
// Board's own bar (rendered directly in this file, not inside
// Tier3AccessPane) reuses Tier3CollapsedStrip.css's `.tier3-collapsed-
// strip` class verbatim -- Jason's own comparison ("like the second
// opinion bar") is the literal spec here, and importing the same
// stylesheet rather than duplicating it means the two can never visually
// drift apart. Clicking it must both reclaim Chat dominance (in case
// Tier3 was the expanded region -- reclaimChat, from a second instance of
// useDominancePair, see this file's own note on that below) AND set
// boardSize to 'full' in the SAME event handler tick, matching what the
// pre-tenth-pass rail-board-btn already had to do for the same reason:
// effectiveBoardSize (below) forces boardSize back to 'minimized' on any
// render where boardSize !== 'minimized' but pair.dominant is still
// 'tier3' -- reclaiming first (in the same React 18 batched update) is
// what keeps that correction from immediately undoing the click.
//
// Tier3AccessPane is mounted continuously across every boardSize -- never
// unmounted while this screen itself is mounted, just resized (floor vs
// full) by whichever branch below renders alongside it. See this file's
// git history (items.id=384/391) for why: earlier designs that unmounted
// it whenever Board was dominant were reasoned from CEF pane lifecycle
// concerns that don't actually apply once Board becoming the expanded
// region already forces dominant back to 'chat' first (no pane is ever
// actively compositing while Board fills the screen).
//
// BUG FOUND + FIXED (2026-09-02, fourth pass, still relevant to this
// pass's rewrite): this file used to have THREE separate
// `if (boardSize === ...) return (...)` blocks, each a differently-SHAPED
// JSX tree -- React's reconciliation compares trees structurally, so this
// unmounted and remounted Tier3AccessPane on every boardSize transition,
// silently resetting its local state. Fixed then, and preserved by this
// rewrite, by keeping Tier3AccessPane at the SAME JSX position across
// every boardSize -- this pass's simplification (Board is either a bar or
// a region, never a third shape sharing the screen with a "medium" pair)
// only reduces the number of distinct shapes further, it doesn't
// reintroduce variation in Tier3AccessPane's own position.

import { useEffect } from 'react'
import { useTranslation } from 'react-i18next'
import type { PersonaInfo } from '../bindings'
import { ActiveBoardPane } from './ActiveBoardPane'
import { Tier3AccessPane } from './Tier3AccessPane'
import { useDominancePair } from './useDominancePair'
import type { BoardSizeState, DominancePairState } from './navShellConfig'
import './Tier3CollapsedStrip.css'
// items.id=391 (eleventh pass): for .tier3-access-pane__section-header --
// Board's own expanded header (below) reuses that class verbatim so it's
// guaranteed to match QR's and Tier3's own expanded headers (Tier3AccessPane.tsx),
// not just visually similar values re-declared here.
import './NavShell.css'
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
  boardSize: persistedBoardSize,
  onBoardSizeChange,
  pair,
  onUpdatePair,
}: WorkspaceShellProps) {
  const { t } = useTranslation()

  // items.id=391 (third pass, preserved by this rewrite): Board expanded
  // (boardSize !== 'minimized') and a dominant Tier3 must never coexist as
  // both "expanded" at once -- computed synchronously during render so
  // there is never a committed frame where that's true, not even briefly.
  // The effect below only persists the correction into NavState afterward.
  const effectiveBoardSize: BoardSizeState =
    persistedBoardSize !== 'minimized' && pair.dominant === 'tier3'
      ? 'minimized'
      : persistedBoardSize

  useEffect(() => {
    if (effectiveBoardSize !== persistedBoardSize) {
      onBoardSizeChange(effectiveBoardSize)
    }
  }, [effectiveBoardSize, persistedBoardSize, onBoardSizeChange])

  const boardSize = effectiveBoardSize
  const boardExpanded = boardSize !== 'minimized'

  // items.id=391 (tenth pass): a second, independent instance of this hook
  // -- see useDominancePair.ts's own shape: it has no internal state of
  // its own besides a local `openError` this file never reads, every
  // other value is a pure function of the `pair`/`onUpdatePair` args both
  // this component and Tier3AccessPane are already handed. Two instances
  // reading/writing the same external pair behave identically to one;
  // this avoids lifting the hook (and Tier3AccessPane's whole rail/
  // content-pane prop surface) up into this file just so Board's own bar
  // (below) can call reclaimChat before setting boardSize to 'full'.
  const { reclaimChat } = useDominancePair(pair, onUpdatePair)

  return (
    <div className="workspace-shell" data-board-size={boardSize}>
      {boardExpanded ? (
        <div key="board-region" className="workspace-shell__board-region">
          {/* items.id=391 (eleventh pass): Board's own expanded header --
              the SAME .section-header class QR's and Tier3's own expanded
              headers use (NavShell.css, imported below for it) -- confirmed
              live (Jason): "the active bar needs a header when it's fully
              expanded," matching the other two rather than a plain
              standalone density-toggle pill floating above a bare <h2>
              (ActiveBoardPane's own internal heading, now removed as
              redundant with this). The density toggle folds into this
              header's own controls slot, same position QR's History/
              persona-picker controls sit in. */}
          <div className="tier3-access-pane__section-header">
            <span className="tier3-access-pane__section-header-name">
              {t('navShell.workspaceShell.boardBarLabel')}
            </span>
            <div className="tier3-access-pane__section-header-controls">
              <button
                type="button"
                className="workspace-shell__board-density-button"
                onClick={() => onBoardSizeChange(boardSize === 'compact' ? 'full' : 'compact')}
              >
                {boardSize === 'compact'
                  ? t('navShell.workspaceShell.fullBoardButton')
                  : t('navShell.workspaceShell.compactBoardButton')}
              </button>
            </div>
          </div>
          <ActiveBoardPane userId={userId} dense={boardSize === 'compact'} />
        </div>
      ) : (
        <button
          key="board-bar"
          type="button"
          className="tier3-collapsed-strip"
          onClick={() => {
            reclaimChat()
            onBoardSizeChange('full')
          }}
        >
          <span className="tier3-collapsed-strip__name">
            {t('navShell.workspaceShell.boardBarLabel')}
          </span>
          <span className="tier3-collapsed-strip__expand">
            {t('navShell.tier3CollapsedStrip.expandLabel')}
          </span>
        </button>
      )}

      <Tier3AccessPane
        key="tier3-access-pane"
        personaId={activePersonaId}
        onPersonaChange={onActivePersonaIdChange}
        personas={personas}
        pair={pair}
        onUpdatePair={onUpdatePair}
        floor={boardExpanded}
        onFloorExpand={() => onBoardSizeChange('minimized')}
      />
    </div>
  )
}
