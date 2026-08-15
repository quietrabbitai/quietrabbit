// Top-strip / navigation-shell -- structural types and constants.
//
// Traces to: 03_ProjectDocs/Specifications/INFORMATION_ARCHITECTURE_SPEC.md
// Section 2 (Top strip structure), Section 2e (temporary buttons + chain
// truncation), Section 4 (Persona hub), adopted 2026-07-27,
// decisions.id=652-656 -- and 658/659, which is what actually fixes the
// CURRENT slot set: My Facts supersedes Privacy Audit as the 4th global
// button (decisions.id=658), Tier 3 access is the 5th (decisions.id=659).
// Builds items.id=232.

export type FixedButtonId = 'activeBoard' | 'library' | 'myFacts' | 'tier3'

/** Section 2f's confirmed slot order (Active Board, Library, My Facts,
 *  Tier 3 access), left-to-right ahead of the Persona cluster. */
export const FIXED_BUTTON_ORDER: FixedButtonId[] = [
  'activeBoard',
  'library',
  'myFacts',
  'tier3',
]

/** The top of the navigation chain at any moment -- exactly one at a
 *  time, either a fixed global button or a single Persona (Section 2d).
 *  Selecting either is what Section 2e's chain-truncation rule clears
 *  the temporary chain against. */
export type AnchorId =
  | { kind: 'fixed'; id: FixedButtonId }
  | { kind: 'persona'; personaId: string }

/** Content this shell can actually resolve to real or placeholder panes
 *  this pass. Active Board / Library / My Facts have no built screen yet
 *  (Active Board and Library are real, separately-scoped IPC-backed
 *  features; this item's job is the shell, not those screens) -- see
 *  NavShell.tsx's content resolution for how each renders. */
export type ContentDescriptor =
  | { type: 'activeBoard' }
  | { type: 'library'; personaFilter?: string }
  | { type: 'myFacts' }
  | { type: 'tier3' }
  | { type: 'personaHub'; personaId: string }

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

export interface NavState {
  anchor: AnchorId
  chain: TemporaryCrumb[]
}

export const DEFAULT_NAV_STATE: NavState = {
  // Section 2a: "Likely the default landing view when QR opens (not
  // formally locked here -- flagged as the working assumption; Chat-PM
  // should confirm before treating as final)." Taken as the working
  // default here on that same explicit hedge, not as a locked decision.
  anchor: { kind: 'fixed', id: 'activeBoard' },
  chain: [],
}

/** Tapping any fixed or Persona button sets it as the new anchor and
 *  clears the temporary chain. This is Section 2e's chain-truncation
 *  rule applied uniformly ("this applies uniformly, not just to Persona
 *  navigation") -- Section 4's "tapping the Persona button again
 *  truncates back to the hub" falls out of this same rule when the
 *  tapped anchor is already the current one, rather than needing its
 *  own special case. */
export function selectAnchor(anchor: AnchorId): NavState {
  return { anchor, chain: [] }
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
  return state.anchor.kind === 'fixed'
    ? fixedButtonContent(state.anchor.id)
    : { type: 'personaHub', personaId: state.anchor.personaId }
}

function fixedButtonContent(id: FixedButtonId): ContentDescriptor {
  switch (id) {
    case 'activeBoard':
      return { type: 'activeBoard' }
    case 'library':
      return { type: 'library' }
    case 'myFacts':
      return { type: 'myFacts' }
    case 'tier3':
      return { type: 'tier3' }
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
  if (state.anchor.kind === 'fixed' && state.anchor.id === id) return 'anchor'
  if (state.chain.some((c) => c.aliasesFixedButton === id)) return 'alias'
  return 'none'
}

export function isPersonaAnchor(state: NavState, personaId: string): boolean {
  return (
    state.anchor.kind === 'persona' && state.anchor.personaId === personaId
  )
}

/** decisions.id=659: Tier 3 access is "disabled until a Persona is
 *  selected" -- read here as requiring the CURRENT anchor to be a
 *  Persona (Section 9: "reached... potentially as its own global
 *  top-strip button requiring a Persona selection first"), not merely
 *  that some Persona has ever been selected earlier in the session. */
export function isTier3Enabled(state: NavState): boolean {
  return state.anchor.kind === 'persona'
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
// user_id. Content that requires key_hex (e.g. commands.getActiveBoard)
// stays real-session-only and unbuilt until items.id=268 lands -- see the
// Active Board placeholder content in NavShell.tsx.
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
