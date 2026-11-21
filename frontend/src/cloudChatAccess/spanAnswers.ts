// items.id=924: what the Privacy Guardian modal OFFERS for a span, from the answers
// the server says it allows (allowed_answers.rs, AllowedAnswers on each span). DISPLAY
// ONLY: the server applies the same rule to whatever is sent (a release it does not
// allow is removed, a generalization carrying the original counts as keep private, a
// remembered release of a span without standing_release is asked again), so a stale or
// edited client cannot widen it. A plain .ts module so its test runs on Node's type
// stripping, as consentDecisions.ts does.
//
// A span without `allowed` (the copy review's consent_request payload, every span the
// Privacy Filter found) allows every answer.

export interface SpanAllowedAnswers {
  release: boolean
  standing_release: boolean
}

export const ALL_ANSWERS: SpanAllowedAnswers = { release: true, standing_release: true }

interface SpanLike {
  original_text: string
  fact_key: string | null
  allowed?: SpanAllowedAnswers
}

export function allowedOf(span: SpanLike): SpanAllowedAnswers {
  return span.allowed ?? ALL_ANSWERS
}

/** The "Release original" cell is offered. */
export function releaseAvailable(span: SpanLike): boolean {
  return allowedOf(span).release
}

/** "Remember this decision" may be ticked for a release (a remembered release of a
 *  span without standing_release is never applied: the serial number, D3). */
export function rememberReleaseAvailable(span: SpanLike): boolean {
  const a = allowedOf(span)
  return a.release && a.standing_release
}

/** A generalization whose text carries the original, case-insensitively, where no
 *  release is allowed: the server would remove the span instead (the same rule as
 *  allowed_answers::permitted). */
export function editCarriesOriginal(span: SpanLike, text: string): boolean {
  const original = span.original_text.trim().toLowerCase()
  return !releaseAvailable(span) && original.length > 0 && text.toLowerCase().includes(original)
}

/** The save_for_persona flag sent for a span: only with a fact key, only when ticked,
 *  and never for a release the span may not remember. */
export function saveForPersonaFor(
  span: SpanLike,
  decision: 'generalize' | 'keep_private' | 'release_original',
  ticked: boolean,
): boolean {
  if (span.fact_key === null || !ticked) return false
  return decision !== 'release_original' || rememberReleaseAvailable(span)
}
