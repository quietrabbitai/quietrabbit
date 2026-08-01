// Middle zone + chat mechanism -- structural implementation.
//
// Traces to: 03_ProjectDocs/Specifications/INFORMATION_ARCHITECTURE_SPEC.md
// Section 3, adopted 2026-07-27, decisions.id=652-656, items.id=8 (CLOSED).
// Reused by Tier 3 access per Section 9 (Tier 3 internals deferred,
// items.id=177/199-201, decisions.id=699 -- this component is the shared
// mechanism both are built on, not Tier 3-specific).
//
// Scope of this file: Section 3a (structure), 3b (contextual, not global,
// left to the caller via contextKey), 3c (three-state ratio profile),
// 3d (focus-location trigger, not a timer), 3e (hard minimum floor).
// Numeric values are placeholders -- see middleZoneConfig.ts header.

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react'
import './MiddleZone.css'
import {
  FOCUS_DEBOUNCE_MS,
  HARD_MINIMUM_CHAT_SHARE,
  type RatioProfile,
  type RatioState,
} from './middleZoneConfig'

export interface MiddleZoneProps {
  /** Section 3b: chat is contextual, not global -- switching this key
   *  represents switching which content is active, and therefore which
   *  transcript is showing. This component does not own transcript
   *  identity; the caller does. */
  contextKey: string
  /** This content type's declared three-value profile (Section 3c). */
  profile: RatioProfile
  /** Whether a response is currently generating for this context, driving
   *  the mid-response state (Section 3c) independently of focus. */
  isGenerating: boolean
  /** Context pane content (browsing-primary side). */
  contextPane: ReactNode
  /** Chat pane content (conversation side). */
  chatPane: ReactNode
}

/** Clamp a context-pane share so the chat pane never drops below
 *  Section 3e's hard minimum floor. */
function clampToFloor(contextShare: number): number {
  const maxContextShare = 1 - HARD_MINIMUM_CHAT_SHARE
  return Math.min(contextShare, maxContextShare)
}

/** Dev-only sanity check for a caller-supplied profile (external review,
 *  2026-07-31). Section 3g leaves the real numeric values undefined, so
 *  this warns rather than throws or silently repairs -- an out-of-range
 *  placeholder value is a bug to surface, not something to mask by
 *  clamping it away in clampToFloor. No-op in production builds. */
function warnIfProfileInvalid(profile: RatioProfile): void {
  if (import.meta.env.PROD) return
  const maxAllowed = 1 - HARD_MINIMUM_CHAT_SHARE
  for (const [state, value] of Object.entries(profile)) {
    if (!Number.isFinite(value) || value < 0 || value > maxAllowed) {
      console.warn(
        `[MiddleZone] profile.${state} (${value}) is outside the ` +
          `allowed range [0, ${maxAllowed}] (Section 3e hard floor).`,
      )
    }
  }
}

export function MiddleZone({
  contextKey,
  profile,
  isGenerating,
  contextPane,
  chatPane,
}: MiddleZoneProps) {
  // Section 3d: trigger is focus location, not time. 'resting' is the
  // default before either pane has been focused this context.
  const [focusState, setFocusState] = useState<'resting' | 'active'>(
    'resting',
  )
  const debounceRef = useRef<number | null>(null)

  // Section 3b: switching contextKey means switching which content/chat
  // pair is active -- reset to that content type's own resting state
  // rather than carrying over the previous context's focus state. This
  // is Chat-DEV's interpretation of 3b/3d, not an explicit spec
  // requirement: the spec doesn't say whether focus should be considered
  // to have "moved" when the underlying content changes under an
  // unmoved cursor (e.g. switching Library A -> Library B while the
  // cursor sits in the chat input the whole time). Starting fresh at
  // resting was judged the safer reading (external review, 2026-07-31).
  //
  // A pending debounced transition from the OLD context must be
  // cancelled here, not just left to fire -- otherwise a focus event
  // from just before the switch can resolve after this reset and
  // silently override the new context's resting state (external
  // review, 2026-07-31).
  useEffect(() => {
    if (debounceRef.current !== null) {
      window.clearTimeout(debounceRef.current)
      debounceRef.current = null
    }
    setFocusState('resting')
  }, [contextKey])

  const setFocusDebounced = useCallback((next: 'resting' | 'active') => {
    if (debounceRef.current !== null) {
      window.clearTimeout(debounceRef.current)
    }
    debounceRef.current = window.setTimeout(() => {
      setFocusState(next)
      debounceRef.current = null
    }, FOCUS_DEBOUNCE_MS)
  }, [])

  // Ignore a focus/blur pair that stays inside the same pane (e.g.
  // clicking from one button to another within the chat pane) -- without
  // this, moving focus between two elements in one pane still queues a
  // transition on every hop, since onBlur fires before the next
  // element's onFocus (external review, 2026-07-31).
  const paneContainsRelatedTarget = (
    e: React.FocusEvent<HTMLDivElement>,
  ): boolean => e.currentTarget.contains(e.relatedTarget as Node | null)

  // Development-only: surface an out-of-range placeholder profile rather
  // than letting clampToFloor mask it (external review, 2026-07-31).
  useEffect(() => {
    warnIfProfileInvalid(profile)
  }, [profile])

  useEffect(() => {
    return () => {
      if (debounceRef.current !== null) {
        window.clearTimeout(debounceRef.current)
      }
    }
  }, [])

  // Section 3c: mid-response sits between resting and active -- it wins
  // over a plain 'resting' focus state (user clicked away, but a
  // response is still generating), but active focus (user is in the
  // chat pane, typing or reading a live exchange) takes precedence over
  // it, since the user is already looking at the full transcript.
  const ratioState: RatioState =
    focusState === 'active'
      ? 'active'
      : isGenerating
        ? 'midResponse'
        : 'resting'

  const contextShare = clampToFloor(profile[ratioState])

  return (
    <div className="middle-zone" data-ratio-state={ratioState}>
      <div
        className="middle-zone__pane middle-zone__pane--context"
        style={{ '--pane-share': contextShare } as React.CSSProperties}
        onFocus={(e) => {
          if (paneContainsRelatedTarget(e)) return
          setFocusDebounced('resting')
        }}
        tabIndex={-1}
      >
        {contextPane}
      </div>
      <div
        className="middle-zone__pane middle-zone__pane--chat"
        style={{ '--pane-share': 1 - contextShare } as React.CSSProperties}
        onFocus={(e) => {
          if (paneContainsRelatedTarget(e)) return
          setFocusDebounced('active')
        }}
        onBlur={(e) => {
          if (paneContainsRelatedTarget(e)) return
          setFocusDebounced('resting')
        }}
        tabIndex={-1}
      >
        {isGenerating && (
          <output className="middle-zone__generating-cue" aria-live="polite">
            <span className="middle-zone__spinner" aria-hidden="true" />
            <span>still generating</span>
          </output>
        )}
        {chatPane}
      </div>
    </div>
  )
}
