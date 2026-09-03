// Navigation-shell structural types and constants -- the 5-rail dock model.
//
// Traces to: 03_ProjectDocs/Specifications/HIERARCHICAL_NAV_SHELL_DESIGN_20260902.md
// (items.id=404, decisions.id=748-752), which supersedes
// 03_ProjectDocs/Specifications/INFORMATION_ARCHITECTURE_SPEC.md Section 2
// (top strip structure, superseded in full) and partially supersedes
// Section 4 (Persona hub screen). Prior history: adopted 2026-07-27
// (decisions.id=652-656, then 658/659), then items.id=384's 3-button merge
// (decisions.id=734-743, "workspace"/"library"/"myFacts"), which this file
// used to encode as FixedButtonId/TopLevel/ContentDescriptor/chain. All of
// that is gone now -- there is no flat top strip in the target design at
// all; Persona selection happens inside the History rail's row-stack, not
// via a separate button cluster.
//
// items.id=404: the 3-peer accordion (Board/Chat/Tier3, decisions.id=747's
// "three peer bars, exactly one expanded") generalizes to 5 peer rails
// (Board/Chat/Tier3/Library/History) under ONE dominance field --
// `dominantRail` below -- replacing the old two-axis
// boardSize/pair.dominant split. See WorkspaceShell.tsx's own header
// comment for the full mechanics (in particular the two-way sync between
// `dominantRail` and `pair.dominant`, needed because Tier3AccessPane's
// internal chat<->tier3 split still keys off `pair.dominant` and isn't
// being rewritten).
//
// What got deleted, not just left unwired, and why: FixedButtonId,
// FIXED_BUTTON_ORDER, TopLevel, TemporaryCrumb, chain,
// selectFixed/selectWorkspaceHome/selectPersona/isPersonaAnchor/pushCrumb/
// selectCrumb/fixedButtonLitState/fixedButtonContent/currentContent are all
// removed -- once the top strip and its Persona-button cluster are gone,
// nothing in the new UI can ever construct a non-default TopLevel again
// (PersonaHub.tsx's own action-row callbacks were the only thing that ever
// pushed a chain crumb, and nothing can reach PersonaHub any more either).
// Per CLAUDE.md's "if you're certain something is unused, delete it
// completely" -- this is navigation PLUMBING, confirmed dead.
//
// What did NOT get deleted: PersonaHub.tsx and FocusSettingsPane.tsx
// themselves. Both are real, working feature code (a working Focus list
// via listFocuses, a Library action-row button) whose disposition is
// items.id=400's scope, not this item's -- deleting them would destroy
// that item's starting point. They're left on disk, simply unimported by
// NavShell.tsx now. Concrete, user-visible consequence: there is currently
// no UI path to view a Persona's Focus list at all (flagged in this item's
// session handoff, not silently absorbed).

/** One of the five peer rails, exactly one dominant (fills the majority of
 *  the screen) at a time -- see WorkspaceShell.tsx. Direct generalization
 *  of decisions.id=747's three-peer-bar model. */
export type DockRailId = 'board' | 'chat' | 'tier3' | 'library' | 'history'

/** decisions.id=735: the QR Chat <-> Tier 3 dominance pair. Which side
 *  fills the main slot (when the pair itself is dominant -- see
 *  WorkspaceShell.tsx's sync effects), and Tier 3's own rail/pane
 *  bookkeeping -- lifted here (rather than left local to Tier3AccessPane's
 *  component state) so it survives navigating to Board/Library/History and
 *  back. Live as of slice 4 -- read/written via useDominancePair.ts, the
 *  only place that constructs a new value of this shape (its own
 *  withDominance() keeps `dominant` in sync with `activeProviderId` in one
 *  place, rather than each call site setting it separately).
 *
 *  items.id=404: `dominant` here is a narrower, Tier3AccessPane-internal
 *  echo of the real source of truth (`workspace.dominantRail` below), kept
 *  in sync by WorkspaceShell.tsx rather than read directly by anything
 *  outside Tier3AccessPane. Not collapsed into one field because
 *  Tier3AccessPane's internals key off `pair.dominant` in enough places
 *  that rewriting them wasn't worth the risk for this item. */
export interface DominancePairState {
  dominant: 'chat' | 'tier3'
  openProviderIds: string[]
  activeProviderId: string | null
}

export interface NavState {
  /** Which Persona is currently active -- read by Chat, Library, and
   *  History's Persona-row selection alike. Set via setActivePersonaId
   *  (the "quiet" switch, reused by items.id=404's two cross-navigation
   *  actions) -- there is no other way to change it any more now that the
   *  Persona-button cluster is gone. */
  activePersonaId: string | null
  workspace: {
    dominantRail: DockRailId
    /** Board's own density preference (ActiveBoardPane's `dense` prop),
     *  independent of dominance -- was folded into the old
     *  BoardSizeState's 'full'/'compact' values alongside a 'minimized'
     *  state that `dominantRail !== 'board'` now covers instead. */
    boardDensity: 'full' | 'compact'
    pair: DominancePairState
  }
}

export const DEFAULT_NAV_STATE: NavState = {
  activePersonaId: null,
  workspace: {
    // Section 2a (IA_SPEC, historical): "Likely the default landing view
    // when QR opens (not formally locked here)." Chat dominant preserves
    // that same working default (today's WorkspaceShell default) now that
    // there's no separate 'workspace' vs. 'library'/'myFacts' choice to
    // make first.
    dominantRail: 'chat',
    boardDensity: 'full',
    pair: { dominant: 'chat', openProviderIds: [], activeProviderId: null },
  },
}

/** decisions.id=740 (items.id=384 slice 7): a "quiet" persona switch --
 *  updates activePersonaId only, without navigating anywhere. Originally
 *  built for the new-chat persona dot-picker inside Tier3AccessPane;
 *  items.id=404 reuses it unchanged for both of its cross-navigation
 *  actions (History's Persona-row "Chat" action, and Chat's own "Chat
 *  history" toggle jumping into History) -- exactly the "already wired"
 *  mechanism the design doc calls for. */
export function setActivePersonaId(personaId: string): (state: NavState) => NavState {
  return (state) => ({ ...state, activePersonaId: personaId })
}

export function setDominantRail(rail: DockRailId): (state: NavState) => NavState {
  return (state) => ({
    ...state,
    workspace: { ...state.workspace, dominantRail: rail },
  })
}

export function setBoardDensity(density: 'full' | 'compact'): (state: NavState) => NavState {
  return (state) => ({
    ...state,
    workspace: { ...state.workspace, boardDensity: density },
  })
}

/** items.id=384 slice 4: updates NavState.workspace.pair via a functional
 *  updater over the CURRENT pair (not a plain replacement value) --
 *  useDominancePair.ts's activate/close/reclaimChat all need to compute
 *  the next pair against whatever it is at the moment their async Rust
 *  command resolves, not whatever it was when the click happened. See
 *  that hook's own header comment for why a plain-value setter would
 *  reintroduce a stale-closure race. WorkspaceShell.tsx is the only
 *  caller now (previously NavShell.tsx) -- it wraps setNavState's own
 *  functional-updater form around this, and additionally syncs
 *  `dominantRail` when `pair.dominant` changes underneath it (see that
 *  file's own header comment). */
export function updateWorkspacePair(
  updater: (prev: DominancePairState) => DominancePairState,
): (state: NavState) => NavState {
  return (state) => ({
    ...state,
    workspace: { ...state.workspace, pair: updater(state.workspace.pair) },
  })
}

// ---------------------------------------------------------------------------
// Real session identity -- items.id=267, 2026-08-15
// ---------------------------------------------------------------------------
//
// commands.login() returns null on success (per CLAUDE.md, the master key
// and session state never leave Rust/AppState, never in an IPC response),
// so App.tsx's login flow calls commands.getSession() immediately after a
// successful login to learn the resulting user_id, then hands it to
// setCurrentUserId() below. Every call site that needs the current user's
// id goes through getCurrentUserId() -- never inline the literal -- kept
// as a single module-level choke point (mirrors the PLACEHOLDER-constant
// discipline already established in middleZone/middleZoneConfig.ts) rather
// than threading userId as a prop through every consumer.
//
// Returns null before a session exists (during App.tsx's initial
// getSession() check, and on the login screen itself) -- callers that run
// before NavShell mounts must handle that; callers inside NavShell's own
// tree can rely on it being non-null, since NavShell only mounts once
// App.tsx's login gate has confirmed a session.
//
// Do NOT extend this pattern to key_hex anywhere. CLAUDE.md's "Master key
// never persisted" rule has no equivalent carve-out, and a fake key_hex
// would be a materially more sensitive thing to stand in for than a
// user_id. This was never actually a live concern for
// commands.getActiveBoard specifically, despite an earlier version of
// this comment claiming otherwise -- that command pulls key_hex itself
// from server-side KeyRegistry state (items.id=268, landed 2026-08-15),
// the same way every other command does; it never needed one bridged in
// from the frontend. See ActiveBoardPane.tsx (items.id=384 slice 2).
let currentUserId: string | null = null

export function setCurrentUserId(userId: string | null): void {
  currentUserId = userId
}

export function getCurrentUserId(): string | null {
  return currentUserId
}

/** Non-nullable variant for call sites inside NavShell's own render tree,
 *  where a session is guaranteed by construction (NavShell only mounts
 *  post-login) -- fails loudly on the invariant being violated rather than
 *  silently passing an empty string downstream, matching this codebase's
 *  existing discipline of erroring on corrupt/impossible state rather than
 *  papering over it (e.g. auth/user_store.rs's hex_decode). */
export function requireCurrentUserId(): string {
  if (!currentUserId) {
    throw new Error('requireCurrentUserId() called with no active session')
  }
  return currentUserId
}
