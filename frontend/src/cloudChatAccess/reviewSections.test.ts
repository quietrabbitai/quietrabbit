// Plain-assertion test for groupSpansByTier (items.id=406), matching
// consentDecisions.test.ts's own convention: run directly on Node's
// built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/cloudChatAccess/reviewSections.test.ts
import assert from 'node:assert/strict'
import { groupSpansByTier, type ReviewTierSpan } from './reviewSections.ts'

function span(id: string, tier: ReviewTierSpan['review_tier']): ReviewTierSpan {
  return { span_id: id, review_tier: tier }
}

// Empty input: all three sections empty, not an error.
const empty = groupSpansByTier([])
assert.deepEqual(empty, { low: [], medium: [], high: [] })

// Mixed tiers land in their own bucket, order preserved within each bucket.
const mixed = groupSpansByTier([
  span('a', 'high'),
  span('b', 'low'),
  span('c', 'medium'),
  span('d', 'low'),
])
assert.equal(mixed.low.length, 2)
assert.deepEqual(
  mixed.low.map((s) => s.span_id),
  ['b', 'd'],
)
assert.deepEqual(
  mixed.medium.map((s) => s.span_id),
  ['c'],
)
assert.deepEqual(
  mixed.high.map((s) => s.span_id),
  ['a'],
)

// All spans in one tier -- the other two sections must come back empty
// (this is what "blank sections hidden" keys off of at render time).
const allHigh = groupSpansByTier([span('x', 'high'), span('y', 'high')])
assert.equal(allHigh.low.length, 0)
assert.equal(allHigh.medium.length, 0)
assert.equal(allHigh.high.length, 2)

console.log('reviewSections.test.ts: all assertions passed')
