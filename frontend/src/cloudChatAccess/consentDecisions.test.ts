// Plain-assertion test for isAllKeptPrivate (items.id=377). No test
// framework exists in this frontend yet, so this runs directly on Node's
// built-in TypeScript support -- no new dependency for one function:
//
//   node --experimental-strip-types src/cloudChatAccess/consentDecisions.test.ts
//
// (also wired as `npm test`.) If this grows into a real suite, that's the
// point to bring in vitest instead of adding more of these by hand.
import assert from 'node:assert/strict'
import { isAllKeptPrivate, type ElementDecision } from './consentDecisions.ts'

function decision(kind: ElementDecision['decision']): ElementDecision {
  return {
    span_id: 'x',
    decision: kind,
    suggestion_text: null,
    user_modified_text: null,
    category: 'private_person',
    fact_key: null,
    save_for_persona: false,
  }
}

// The regression: zero spans to review must not read as "all kept private".
assert.equal(isAllKeptPrivate([]), false, 'empty decisions must not be vacuously all-kept-private')

assert.equal(isAllKeptPrivate([decision('keep_private')]), true)
assert.equal(isAllKeptPrivate([decision('keep_private'), decision('keep_private')]), true)
assert.equal(isAllKeptPrivate([decision('keep_private'), decision('generalize')]), false)
assert.equal(isAllKeptPrivate([decision('release_original')]), false)

console.log('consentDecisions.test.ts: all assertions passed')
