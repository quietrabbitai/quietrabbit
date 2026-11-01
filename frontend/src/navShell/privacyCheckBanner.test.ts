// Plain-assertion test for showPrivacyCheckBanner (items.id=618), matching
// frictionGateDetail.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/privacyCheckBanner.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { showPrivacyCheckBanner } from './privacyCheckBanner.ts'
import type { PrivacyFilterStatus } from '../bindings.ts'

// Nothing to show before get_health answers, or when the check is ready.
assert.equal(showPrivacyCheckBanner(null), false)
assert.equal(showPrivacyCheckBanner({ kind: 'ready' }), false)

// Every other state shows the banner.
const shown: PrivacyFilterStatus[] = [
  { kind: 'downloading', percent: 42 },
  { kind: 'downloading', percent: null },
  { kind: 'verifying' },
  { kind: 'unavailable', reason: 'not_compiled_in' },
  { kind: 'unavailable', reason: 'model_unavailable' },
]
for (const status of shown) {
  assert.equal(showPrivacyCheckBanner(status), true, JSON.stringify(status))
}

console.log('privacyCheckBanner.test.ts: ok')
