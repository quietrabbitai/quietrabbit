// Plain-assertion test for parseFrictionGateDetail (items.id=533), matching
// consentDecisions.test.ts's own convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/frictionGateDetail.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { parseFrictionGateDetail, type FrictionGateDetail } from './frictionGateDetail.ts'

function validDetail(): FrictionGateDetail {
  return {
    persona_id: 'persona-1',
    focus_id: 'focus-1',
    requested_privacy_tier: 'green',
    requested_focus_profile: null,
    requested_max_permitted_tier: null,
    existing_privacy_tier: 'yellow',
    existing_focus_profile: 'open',
    existing_max_permitted_tier: 'anonymous_required',
    privacy_would_loosen: true,
    moves_to_protected: false,
    max_permitted_tier_would_loosen: false,
  }
}

// A well-formed payload with valid enum spellings parses successfully.
assert.deepEqual(parseFrictionGateDetail(JSON.stringify(validDetail())), validDetail())

// requested_privacy_tier may legitimately be null (only one dimension
// tripped the gate).
assert.deepEqual(
  parseFrictionGateDetail(JSON.stringify({ ...validDetail(), requested_privacy_tier: null })),
  { ...validDetail(), requested_privacy_tier: null },
)

// An invalid existing_privacy_tier spelling (old numeric wire shape, or
// wrong case) must fail visibly -- null, not a silent bad cast.
assert.equal(
  parseFrictionGateDetail(JSON.stringify({ ...validDetail(), existing_privacy_tier: 3 })),
  null,
  'a bare number for existing_privacy_tier must not parse',
)
assert.equal(
  parseFrictionGateDetail(JSON.stringify({ ...validDetail(), existing_privacy_tier: 'Green' })),
  null,
  'PascalCase existing_privacy_tier must not parse',
)

// An invalid requested_privacy_tier (non-null, unrecognized) must also fail.
assert.equal(
  parseFrictionGateDetail(
    JSON.stringify({ ...validDetail(), requested_privacy_tier: 'purple' }),
  ),
  null,
  'an unrecognized requested_privacy_tier spelling must not parse',
)

// The already-enum max_permitted_tier fields are validated the same way
// (items.id=533 widened validation to cover both enum fields, not just the
// newly retyped privacy_tier ones).
assert.equal(
  parseFrictionGateDetail(
    JSON.stringify({ ...validDetail(), existing_max_permitted_tier: 'tier_2' }),
  ),
  null,
  'an unrecognized existing_max_permitted_tier spelling must not parse',
)
assert.equal(
  parseFrictionGateDetail(
    JSON.stringify({ ...validDetail(), requested_max_permitted_tier: 2 }),
  ),
  null,
  'a bare number for requested_max_permitted_tier must not parse',
)

// A non-JSON string is still a plain error message (not_found, tier bounds
// check, ...), not a friction-gate detail.
assert.equal(parseFrictionGateDetail('not_found'), null)

console.log('frictionGateDetail.test.ts: all assertions passed')
