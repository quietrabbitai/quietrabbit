// Applies a resolved Privacy Guardian decision set to the text of a flagged
// copy (items.id=501 slice 2, Jason's answer A: the copied text reflects the
// user's choices). Nothing applied decisions to text before this -- the
// per-reply flow only recorded them -- but for a copy the clipboard IS the
// output, so "generalize" must not copy the original.
//
// A plain .ts module (no JSX), same convention as consentDecisions.ts: its
// test runs directly on Node's type-stripping.
import type { ElementDecision } from '../cloudChatAccess/consentDecisions.ts'

/** The minimal span shape this needs -- structurally a subset of
 *  ConsentSpanItem (PrivacyGuardianModal.tsx), declared here so this module
 *  and its test don't import a .tsx file. */
export interface CopySpan {
  span_id: string
  start_byte: number
  end_byte: number
}

/** Prefix of a copy review's synthetic key (focus_run_id on its
 *  consent_request payload and on its consent rows). Mirrors the backend's
 *  output_store::COPY_REVIEW_RUN_ID_PREFIX -- keep in sync by hand. */
export const COPY_REVIEW_RUN_ID_PREFIX = 'clipboard-copy-chat-'

/** Stands in for a kept-private span, or a generalize with no replacement
 *  text. Written into the clipboard content, not shown in UI chrome. */
export const COPY_REMOVED_PLACEHOLDER = '[removed]'

function replacementFor(decision: ElementDecision | undefined): string | null {
  // null = leave the original text in place.
  if (!decision) return COPY_REMOVED_PLACEHOLDER // unresolved -> safe default
  switch (decision.decision) {
    case 'release_original':
      return null
    case 'keep_private':
      return COPY_REMOVED_PLACEHOLDER
    case 'generalize': {
      const text = (decision.user_modified_text ?? decision.suggestion_text ?? '').trim()
      return text === '' ? COPY_REMOVED_PLACEHOLDER : text
    }
  }
}

/** Returns the text to put on the clipboard, or null if the spans cannot be
 *  applied safely (offset out of range, inverted, overlapping, or not on a
 *  UTF-8 character boundary) -- the caller must treat null as a failed copy,
 *  never fall back to the unmodified text. An empty span list returns the
 *  text unchanged (gate3's forced-High-with-no-spans case has nothing to
 *  replace). Span offsets are BYTE offsets into the UTF-8 text, as gate3
 *  emits them. */
export function applyCopyDecisions(
  text: string,
  spans: CopySpan[],
  decisions: ElementDecision[],
): string | null {
  if (spans.length === 0) return text

  const bytes = new TextEncoder().encode(text)
  const ordered = [...spans].sort((a, b) => a.start_byte - b.start_byte)
  const decoder = new TextDecoder('utf-8', { fatal: true })
  const byId = new Map(decisions.map((d) => [d.span_id, d]))

  const out: string[] = []
  let cursor = 0
  try {
    for (const span of ordered) {
      if (
        !Number.isInteger(span.start_byte) ||
        !Number.isInteger(span.end_byte) ||
        span.start_byte < cursor ||
        span.end_byte < span.start_byte ||
        span.end_byte > bytes.length
      ) {
        return null
      }
      out.push(decoder.decode(bytes.subarray(cursor, span.start_byte)))
      const replacement = replacementFor(byId.get(span.span_id))
      out.push(
        replacement ?? decoder.decode(bytes.subarray(span.start_byte, span.end_byte)),
      )
      cursor = span.end_byte
    }
    out.push(decoder.decode(bytes.subarray(cursor)))
  } catch {
    return null // a slice split a multi-byte character
  }
  return out.join('')
}

/** A span gate3 silently resolved from a prior or standing decision (so it
 *  was never in the modal payload). Structurally the generated
 *  AutoResolvedSpan (bindings.ts); redeclared so this module and its test
 *  stay importable from the Node-typed test project. */
export interface AutoResolved {
  start_byte: number
  end_byte: number
  decision: string
  suggestion_text: string | null
  user_modified_text: string | null
}

function toKind(decision: string): ElementDecision['decision'] {
  // Anything unrecognized is treated as the most private choice: a decision
  // string this code does not understand must never release text.
  return decision === 'generalize' || decision === 'release_original' ? decision : 'keep_private'
}

/** Applies BOTH kinds of resolved decision to the text: the interactive
 *  modal's (`spans` + `decisions`) and the ones gate3 reapplied silently
 *  (`auto`). Disjoint by construction (one Privacy Filter pass partitions
 *  its entities between them); if they ever overlap, or any offset is
 *  invalid, this returns null and the caller must fail the copy -- a
 *  keep_private span never appears unredacted, and the original text is
 *  never the fallback. */
export function applyAllCopyDecisions(
  text: string,
  spans: CopySpan[],
  decisions: ElementDecision[],
  auto: AutoResolved[],
): string | null {
  const autoSpans: CopySpan[] = auto.map((a, i) => ({
    span_id: `auto-${i}`,
    start_byte: a.start_byte,
    end_byte: a.end_byte,
  }))
  const autoDecisions: ElementDecision[] = auto.map((a, i) => ({
    span_id: `auto-${i}`,
    decision: toKind(a.decision),
    suggestion_text: a.suggestion_text,
    user_modified_text: a.user_modified_text,
    category: '',
    fact_key: null,
    save_for_persona: false,
  }))
  return applyCopyDecisions(
    text,
    [...spans, ...autoSpans],
    [...decisions, ...autoDecisions],
  )
}
