// DEV-ONLY simulated "this copy was flagged" hook (items.id=501 slice 2).
//
// Why it exists: the dev build has no Privacy Filter (PRIVACY_FILTER_LIB_DIR
// unset, items.id=356), so gate3 can never flag a real span and the flagged-
// copy flow (modal -> consent write -> decision application -> clipboard)
// could not be exercised live. This synthesizes the consent_request payload
// gate3 WOULD have emitted, so everything downstream of "a span was flagged"
// runs for real: the modal, submit_chat_copy_consent_decision, the stub
// focus_runs row, applyCopyDecisions and the clipboard write. Only the
// flagging itself is simulated -- the real scan -> modal path is NOT covered
// by this and stays untested until a Privacy Filter build exists.
//
// Guards (all must hold, otherwise this returns null and does nothing):
//   1. The call site (ChatPane.tsx) wraps the call in the literal
//      `import.meta.env.DEV`, which Vite replaces with `false` at build
//      time, so in a production bundle the call and this module are dead
//      code and tree-shaken away. It cannot ship. (Verified: the switch key
//      string is absent from `npm run build` output.)
//   1b. The same env check again inside, as defense in depth.
//   2. localStorage['qr.devSimulateCopyFlag'] is '1', '2' or '3' -- off by
//      default; set by hand, never by app code. Modes:
//        1  modal with two interactive spans (first + last word)
//        2  modal with the first word + the last word silently
//           auto-resolved keep_private (partly auto, partly interactive)
//        3  no modal: first word auto release_original, last word auto
//           keep_private (everything resolved silently)
// It only ever ADDS a flag to a copy the real review already approved; it
// can never suppress or weaken a real review result. The `?.` on env is
// because Node's test runner (no Vite) has no import.meta.env at all.
//
// Removal: delete this file, its test, and the one call site in ChatPane.tsx
// once a Privacy Filter build exists to exercise the real path.
import type { ReviewTier } from '../cloudChatAccess/reviewSections.ts'

// Structurally identical to ConsentSpanItem / ConsentRequestPayload in
// PrivacyGuardianModal.tsx, redeclared here because the Node-typed test
// project cannot import a .tsx file (same reason consentDecisions.ts exists).
// ChatPane assigns the result to the real type, so drift is a compile error.
interface ConsentSpanItem {
  span_id: string
  category: string
  user_label: string
  original_text: string
  suggestion: string | null
  start_byte: number
  end_byte: number
  score: number
  review_tier: ReviewTier
  fact_key: string | null
}

interface ConsentRequestPayload {
  focus_run_id: string
  focus_name: string
  review_tier: ReviewTier
  spans: ConsentSpanItem[]
}

export const DEV_SIMULATE_COPY_FLAG_KEY = 'qr.devSimulateCopyFlag'

/** Structurally the generated AutoResolvedSpan (bindings.ts). */
export interface SimAutoResolved {
  start_byte: number
  end_byte: number
  decision: string
  suggestion_text: string | null
  user_modified_text: string | null
}

export interface SimulatedCopyFlag {
  /** null = nothing left for the modal (everything resolved silently). */
  payload: ConsentRequestPayload | null
  auto: SimAutoResolved[]
}

/** Pure builder, exported for the test (no guards -- the guarded entry point
 *  is devSimulatedFlaggedPayload). Flags the first and last whitespace-
 *  delimited word of 3+ characters: the first as a Medium-tier span with a
 *  suggestion, the last (if distinct) as a High-tier span with none, so both
 *  the default-generalize and the must-decide High paths of the modal
 *  appear. Offsets are real UTF-8 byte offsets into `text`. */
export function buildSimulatedFlag(text: string, runId: string, mode: 1 | 2 | 3): SimulatedCopyFlag {
  const encoder = new TextEncoder()
  const words = [...text.matchAll(/\S{3,}/g)]
  const toSpan = (
    m: RegExpMatchArray,
    id: string,
    tier: ConsentSpanItem['review_tier'],
    category: string,
    suggestion: string | null,
  ): ConsentSpanItem => {
    const start = encoder.encode(text.slice(0, m.index)).length
    return {
      span_id: id,
      category,
      user_label: category === 'private_person' ? 'Name (simulated)' : 'Email (simulated)',
      original_text: m[0],
      suggestion,
      start_byte: start,
      end_byte: start + encoder.encode(m[0]).length,
      score: 0.99,
      review_tier: tier,
      fact_key: null,
    }
  }

  const byteRange = (m: RegExpMatchArray) => {
    const start = encoder.encode(text.slice(0, m.index)).length
    return { start_byte: start, end_byte: start + encoder.encode(m[0]).length }
  }
  const auto: SimAutoResolved[] = []
  const spans: ConsentSpanItem[] = []
  const first = words[0]
  const last = words.length > 1 ? words[words.length - 1] : undefined
  if (mode === 3) {
    if (first) {
      auto.push({ ...byteRange(first), decision: 'release_original', suggestion_text: null, user_modified_text: null })
    }
    if (last) {
      auto.push({ ...byteRange(last), decision: 'keep_private', suggestion_text: null, user_modified_text: null })
    }
    return { payload: null, auto }
  }
  if (first) spans.push(toSpan(first, 'sim-1', 'medium', 'private_person', '[a person]'))
  if (last) {
    if (mode === 1) {
      spans.push(toSpan(last, 'sim-2', 'high', 'private_email', null))
    } else {
      auto.push({ ...byteRange(last), decision: 'keep_private', suggestion_text: null, user_modified_text: null })
    }
  }
  return {
    payload: {
      focus_run_id: runId,
      focus_name: 'Quick Ask',
      review_tier: spans.some((s) => s.review_tier === 'high') ? 'high' : 'medium',
      spans,
    },
    auto,
  }
}

/** Guarded entry point: null unless this is a Vite dev build AND the
 *  localStorage switch is on. */
export function devSimulatedCopyFlag(text: string, runId: string): SimulatedCopyFlag | null {
  // Typed loosely on purpose: this file is also compiled by the Node-typed
  // test project, which has neither Vite's ImportMeta.env nor DOM types.
  const env = (import.meta as { env?: { DEV?: boolean } }).env
  if (!env?.DEV) return null
  try {
    const storage = (globalThis as { localStorage?: { getItem(key: string): string | null } })
      .localStorage
    const mode = storage?.getItem(DEV_SIMULATE_COPY_FLAG_KEY)
    if (mode !== '1' && mode !== '2' && mode !== '3') return null
    return buildSimulatedFlag(text, runId, Number(mode) as 1 | 2 | 3)
  } catch {
    return null
  }
}
