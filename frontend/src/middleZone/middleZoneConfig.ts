// Middle zone + chat mechanism -- ratio-profile types and constants.
//
// Traces to: 03_ProjectDocs/Specifications/INFORMATION_ARCHITECTURE_SPEC.md
// Section 3 (Middle zone + chat mechanism), adopted 2026-07-27,
// decisions.id=652-656, items.id=8 (CLOSED).
//
// IMPORTANT -- read before editing any numeric value in this file:
// Section 3g of the IA spec explicitly leaves the real ratio percentages,
// the hard-floor size, and debounce timing UNSET, flagged as downstream
// numeric-tuning work. The values below are Chat-DEV implementation
// placeholders chosen 2026-07-31 to unblock structural work on items.id=3
// (Jason-approved: build the mechanism now, tune the numbers later --
// see this session's handoff). They are NOT a design decision and were
// never reviewed by Chat-BRAND. Treat every constant in this file as
// provisional; do not cite it as settled elsewhere.

/** The three states a content type's chat/context ratio can be in.
 *  Section 3c. */
export type RatioState = 'resting' | 'active' | 'midResponse'

/** One content type's declared ratio profile (Section 3c: "each content
 *  type declares its own three-value profile, not a single fixed ratio").
 *  Values are the context pane's share of the shared width, 0-1; the
 *  chat pane gets the remainder. */
export interface RatioProfile {
  resting: number
  active: number
  midResponse: number
}

/** PLACEHOLDER -- Section 3g leaves this unset. A generic content type
 *  (browsing-primary, per 3c's "context-heavy for browsing-primary
 *  content") until per-content-type profiles are defined for real. */
export const DEFAULT_BROWSING_PROFILE: RatioProfile = {
  resting: 0.7,
  active: 0.4,
  midResponse: 0.55,
}

/** PLACEHOLDER -- Section 3g leaves this unset. A conversation-primary
 *  content type (per 3c: "chat-heavy for conversation-primary content"),
 *  e.g. Section 8's Persona Chat / hub. */
export const DEFAULT_CONVERSATION_PROFILE: RatioProfile = {
  resting: 0.3,
  active: 0.15,
  midResponse: 0.2,
}

/** PLACEHOLDER -- Section 3e: "a single project-wide hard minimum floor"
 *  beneath every content type's resting state. Expressed as the chat
 *  pane's minimum share of the shared width (0-1); this is the floor
 *  the 3f near-zero requirement was resolved into, per Jason's
 *  2026-07-27 ruling (decisions.id=653) -- never truly zero, but the
 *  smallest chat can be pushed to while remaining tappable/visible.
 *  NOT reviewed by Chat-BRAND. */
export const HARD_MINIMUM_CHAT_SHARE = 0.12

/** PLACEHOLDER -- Section 3g: "debounce behavior on rapid focus/blur
 *  transitions" flagged as an open implementation risk, not resolved.
 *  Milliseconds to wait before acting on a focus/blur change, to avoid
 *  visible flicker on rapid tabbing between panes. NOT reviewed by
 *  Chat-BRAND. */
export const FOCUS_DEBOUNCE_MS = 120

/** Section 3c: state transitions are animated, not an instant snap.
 *  PLACEHOLDER duration. */
export const RATIO_TRANSITION_MS = 220
