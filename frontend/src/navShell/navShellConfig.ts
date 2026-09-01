// Top-strip / navigation-shell -- structural types and constants.
//
// Traces to: 03_ProjectDocs/Specifications/INFORMATION_ARCHITECTURE_SPEC.md
// Section 2 (Top strip structure), Section 2e (temporary buttons + chain
// truncation), Section 4 (Persona hub), adopted 2026-07-27,
// decisions.id=652-656 -- and 658/659, which set the PRIOR slot set (My
// Facts as the 4th global button, Tier 3 access as a persona-gated 5th).
// items.id=384 (decisions.id=734-735) supersedes that 5-button layout:
// Active Board and Tier 3 access are no longer separate top-strip
// buttons -- both live inside one merged 'workspace' button/screen now
// (see WorkspaceShell.tsx), matching the reference mockup's single
// "Board / Chat" top-strip entry.

export type FixedButtonId = 'library' | 'myFacts' | 'workspace'

/** Left-to-right slot order ahead of the Persona cluster. 'workspace'
 *  takes the leading slot the old 'activeBoard' button held -- see this
 *  file's header comment. */
export const FIXED_BUTTON_ORDER: FixedButtonId[] = [
  'workspace',
  'library',
  'myFacts',
]

/** The top of the navigation chain at any moment -- exactly one at a
 *  time, either a fixed global button or the Persona hub (Section 2d).
 *  Selecting either is what Section 2e's chain-truncation rule clears
 *  the temporary chain against.
 *
 *  items.id=384 (decisions.id=734-743): this used to be `AnchorId`, a
 *  union that folded the active Persona's id INTO the fixed/persona
 *  choice itself -- `{kind:'persona', personaId}` -- which meant
 *  switching to any fixed button (e.g. the old separate 'tier3' button)
 *  discarded whatever Persona had been selected, with no room in the
 *  type to carry it through. NavShell.tsx's own tier3PersonaId
 *  workaround (now removed) existed only to paper over that gap.
 *  TopLevel now carries no persona id at all -- NavState.activePersonaId
 *  below is a standing field, independent of which screen is showing,
 *  so a Persona chosen via the hub survives a switch to the merged
 *  Chat/Tier3 workspace without any capture-on-transition hack. */
export type TopLevel =
  | { kind: 'fixed'; id: FixedButtonId }
  | { kind: 'personaHub' }

/** Content this shell can actually resolve to real or placeholder panes.
 *  'workspace' (items.id=384 slice 3) covers what used to be the separate
 *  'activeBoard' and 'tier3' content types -- see WorkspaceShell.tsx.
 *  Library has a real screen; My Facts (items.id=176, Chat-BRAND's
 *  design) is still a placeholder -- see NavShell.tsx's content
 *  resolution for how each renders. */
export type ContentDescriptor =
  | { type: 'workspace' }
  | { type: 'library'; personaFilter?: string }
  | { type: 'myFacts' }
  | { type: 'personaHub'; personaId: string }
  | { type: 'focusSettings'; personaId: string; focusId: string }

/** One entry in the temporary-button chain nested below/alongside the
 *  anchor (Section 2e; Section 4's "tapping a Focus becomes a temporary
 *  button nested one level below the Persona button").
 *
 *  aliasesFixedButton: set when this temporary navigation re-enters an
 *  existing fixed button's own screen in a narrowed context -- e.g.
 *  Library opened from a Persona hub's action row (Section 2c: "the same
 *  underlying view entered a second way, not a second implementation").
 *  When set, the fixed button itself is ALSO shown lit alongside the
 *  anchor -- Section 2e's deliberate two-buttons-lit state ("Work lit +
 *  a document opened from Work's Library also lit... not an error
 *  case") -- rather than this crumb rendering a redundant second
 *  Library-labeled button. */
export interface TemporaryCrumb {
  id: string
  labelKey: string
  aliasesFixedButton?: FixedButtonId
  content: ContentDescriptor
}

/** decisions.id=735: the QR Chat <-> Tier 3 dominance pair. Which side
 *  fills the main slot, and Tier 3's own rail/pane bookkeeping -- lifted
 *  here (rather than left local to Tier3AccessPane's component state) so
 *  it survives navigating away to Board/Library/My Facts and back
 *  (Jason, 2026-09-01 scoping session for this item: the pair persists
 *  across such navigation rather than resetting to Chat-dominant, which
 *  also removes any need for a separate "jump straight to Tier 3 from
 *  Board" affordance -- returning from Board lands wherever the pair
 *  last was). Live as of slice 4 -- read/written via useDominancePair.ts,
 *  the only place that constructs a new value of this shape (its own
 *  withDominance() keeps `dominant` in sync with `activeProviderId` in
 *  one place, rather than each call site setting it separately). */
export interface DominancePairState {
  dominant: 'chat' | 'tier3'
  openProviderIds: string[]
  activeProviderId: string | null
}

/** decisions.id=737: Active Board's three discrete size states (its card
 *  grid doesn't reflow continuously, unlike Chat) -- live as of slice 3,
 *  read/written by WorkspaceShell.tsx. */
export type BoardSizeState = 'full' | 'compact' | 'minimized'

export interface NavState {
  topLevel: TopLevel
  /** Which Persona is currently active, independent of `topLevel` --
   *  set by the Persona hub buttons, read by both the Persona hub itself
   *  and (once slice 3+ lands) the merged workspace's Chat/Tier3 side.
   *  Replaces AnchorId's old `persona.personaId`, which only existed
   *  while a Persona anchor was itself selected. */
  activePersonaId: string | null
  chain: TemporaryCrumb[]
  /** decisions.id=735-737: state for the merged Chat/Tier3/Board
   *  workspace. See DominancePairState/BoardSizeState above. */
  workspace: {
    boardSize: BoardSizeState
    pair: DominancePairState
  }
}

export const DEFAULT_NAV_STATE: NavState = {
  // Section 2a: "Likely the default landing view when QR opens (not
  // formally locked here -- flagged as the working assumption; Chat-PM
  // should confirm before treating as final)." Taken as the working
  // default here on that same explicit hedge, not as a locked decision --
  // boardSize:'full' preserves that same default (Board shown first)
  // now that 'activeBoard' isn't its own topLevel value any more.
  topLevel: { kind: 'fixed', id: 'workspace' },
  activePersonaId: null,
  chain: [],
  workspace: {
    boardSize: 'full',
    pair: { dominant: 'chat', openProviderIds: [], activeProviderId: null },
  },
}

/** Tapping a fixed button sets it as the new top-level screen and clears
 *  the temporary chain -- Section 2e's chain-truncation rule. Does NOT
 *  touch activePersonaId or workspace (see their own doc comments on why
 *  those persist across this transition, unlike the old selectAnchor's
 *  fixed variant, which used to discard a Persona anchor outright). */
export function selectFixed(id: FixedButtonId): (state: NavState) => NavState {
  return (state) => ({ ...state, topLevel: { kind: 'fixed', id }, chain: [] })
}

/** Tapping a Persona button enters the Persona hub for it and clears the
 *  temporary chain -- same chain-truncation rule as selectFixed. Section
 *  4's "tapping the Persona button again truncates back to the hub"
 *  falls out of this uniformly (re-selecting the same Persona still
 *  clears the chain), rather than needing its own special case. */
export function selectPersona(personaId: string): (state: NavState) => NavState {
  return (state) => ({
    ...state,
    topLevel: { kind: 'personaHub' },
    activePersonaId: personaId,
    chain: [],
  })
}

/** decisions.id=740 (items.id=384 slice 7): a "quiet" persona switch --
 *  updates activePersonaId only, unlike selectPersona above, which ALSO
 *  navigates to the Persona hub. The new-chat persona dot-picker inside
 *  the merged workspace (Tier3AccessPane) needs to change which Persona
 *  a fresh chat belongs to without leaving the workspace -- selectPersona
 *  would incorrectly bounce the user out to the Persona hub screen just
 *  for picking who a new chat is for. Does not touch topLevel/chain at
 *  all, unlike every other setter in this file that changes
 *  activePersonaId. */
export function setActivePersonaId(personaId: string): (state: NavState) => NavState {
  return (state) => ({ ...state, activePersonaId: personaId })
}

/** decisions.id=737: switches the merged workspace's Board region among
 *  its three discrete size states -- see WorkspaceShell.tsx, the only
 *  caller. Does not touch `pair` (independent axis, per decisions.id=735's
 *  own note that Board is not part of the Chat<->Tier3 dominance pair). */
export function setBoardSize(size: BoardSizeState): (state: NavState) => NavState {
  return (state) => ({ ...state, workspace: { ...state.workspace, boardSize: size } })
}

/** items.id=384 slice 4: updates NavState.workspace.pair via a functional
 *  updater over the CURRENT pair (not a plain replacement value) --
 *  useDominancePair.ts's activate/close/reclaimChat all need to compute
 *  the next pair against whatever it is at the moment their async Rust
 *  command resolves, not whatever it was when the click happened. See
 *  that hook's own header comment for why a plain-value setter would
 *  reintroduce a stale-closure race. NavShell.tsx is the only caller --
 *  it wraps setNavState's own functional-updater form around this. */
export function updateWorkspacePair(
  updater: (prev: DominancePairState) => DominancePairState,
): (state: NavState) => NavState {
  return (state) => ({
    ...state,
    workspace: { ...state.workspace, pair: updater(state.workspace.pair) },
  })
}

/** Opening a Focus, or (this pass) a Persona-filtered Library view from
 *  the hub's action row, becomes a temporary button appended below the
 *  current anchor -- not a replacement of it. */
export function pushCrumb(state: NavState, crumb: TemporaryCrumb): NavState {
  return { ...state, chain: [...state.chain, crumb] }
}

/** Tapping a temporary button truncates the chain to end at it (standard
 *  breadcrumb semantics) -- "closes/clears every temporary button below
 *  it," Section 2e. */
export function selectCrumb(state: NavState, crumbId: string): NavState {
  const index = state.chain.findIndex((c) => c.id === crumbId)
  if (index === -1) return state
  return { ...state, chain: state.chain.slice(0, index + 1) }
}

/** What the middle zone should be showing right now: the deepest
 *  temporary crumb if any are open, otherwise the anchor's own content
 *  (Section 4: the Persona hub itself, not a Focus or chat, is what a
 *  bare Persona-button tap opens). */
export function currentContent(state: NavState): ContentDescriptor {
  if (state.chain.length > 0) {
    return state.chain[state.chain.length - 1].content
  }
  if (state.topLevel.kind === 'fixed') return fixedButtonContent(state.topLevel.id)
  // topLevel.kind === 'personaHub': activePersonaId is guaranteed non-null
  // here by construction -- selectPersona() is the only way to reach
  // personaHub, and it always sets activePersonaId in the same update.
  return { type: 'personaHub', personaId: state.activePersonaId as string }
}

function fixedButtonContent(id: FixedButtonId): ContentDescriptor {
  switch (id) {
    case 'workspace':
      return { type: 'workspace' }
    case 'library':
      return { type: 'library' }
    case 'myFacts':
      return { type: 'myFacts' }
  }
}

/** Section 2e: a fixed button reads as lit both when it IS the current
 *  anchor, and when a temporary chain entry aliases it -- the deliberate
 *  two-buttons-lit state. Kept as two distinct reasons rather than one
 *  boolean so a future styling pass can differentiate "ambient scope"
 *  from "current focus" per Section 2e's own open visual-design question
 *  ("not finalized in this draft -- flagged as an open visual-design
 *  question, not a structural one"). This structural distinction is
 *  decided; only its visual treatment isn't, and that's out of scope
 *  here regardless (no Chat-BRAND visual pass this item). */
export function fixedButtonLitState(
  state: NavState,
  id: FixedButtonId,
): 'anchor' | 'alias' | 'none' {
  if (state.topLevel.kind === 'fixed' && state.topLevel.id === id) return 'anchor'
  if (state.chain.some((c) => c.aliasesFixedButton === id)) return 'alias'
  return 'none'
}

export function isPersonaAnchor(state: NavState, personaId: string): boolean {
  return state.topLevel.kind === 'personaHub' && state.activePersonaId === personaId
}

// items.id=384 slice 3: the old decisions.id=659 gate ("Tier 3 access
// disabled until a Persona is selected") -- isTier3Enabled(), formerly
// here -- is REMOVED, not just loosened. Board and the Chat/Tier3 pair
// merged into one 'workspace' button (see FIXED_BUTTON_ORDER's own
// comment above), and decisions.id=735 requires Board to be "a separate,
// always-reachable anchor" -- gating the shared button on activePersonaId
// would have blocked Board along with the pair, which D735 explicitly
// rules out. The pair side still needs a Persona to actually chat (D741)
// -- that's handled downstream, unchanged: Tier3AccessPane already
// renders navShell.content.tier3ChatUnavailable when personaId is null,
// exactly the same component-local null-handling LibraryPane already
// uses for its own personaId===null case. Nothing new was needed there.

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
