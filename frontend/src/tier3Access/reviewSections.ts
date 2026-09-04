// Shared pure logic for grouping a consent_request payload's spans into the
// three-section review screen (items.id=406, decisions.id=754). Each span
// now carries its own review_tier (backend: gate3.rs's per-span refactor,
// conductor/privacy/gate3.rs::assign_review_tier_for_span) -- this module is
// the frontend half of "each fact independently assigned by (fact profile x
// destination profile)": grouping by that per-span field into up to three
// buckets, hidden when empty.
//
// A plain .ts module (no JSX), same reasoning as consentDecisions.ts: its
// own test runs directly on Node's type-stripping without a JSX transform.

export type ReviewTier = 'low' | 'medium' | 'high'

export interface ReviewTierSpan {
  span_id: string
  review_tier: ReviewTier
}

export interface GroupedSpans<T extends ReviewTierSpan> {
  low: T[]
  medium: T[]
  high: T[]
}

/** Order sections should render in -- low, medium, high, matching the
 *  spec's own Low/Medium/High reading order. Blank sections are hidden by
 *  the caller (check each array's length), not represented here. */
export const REVIEW_TIER_ORDER: readonly ReviewTier[] = ['low', 'medium', 'high']

export function groupSpansByTier<T extends ReviewTierSpan>(spans: T[]): GroupedSpans<T> {
  const grouped: GroupedSpans<T> = { low: [], medium: [], high: [] }
  for (const span of spans) {
    grouped[span.review_tier].push(span)
  }
  return grouped
}
