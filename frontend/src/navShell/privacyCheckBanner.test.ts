// Plain-assertion test for showPrivacyCheckBanner (items.id=618), matching
// frictionGateDetail.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/privacyCheckBanner.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { showPrivacyCheckBanner, showPrivacyRetry } from './privacyCheckBanner.ts'
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
  { kind: 'unavailable', reason: 'download_failed' },
  { kind: 'unavailable', reason: 'low_disk' },
  { kind: 'unavailable', reason: 'verification_failed' },
]
for (const status of shown) {
  assert.equal(showPrivacyCheckBanner(status), true, JSON.stringify(status))
}

// Retry is offered for every unavailable reason except a build without the
// filter, and for no other state.
assert.equal(showPrivacyRetry(null), false)
assert.equal(showPrivacyRetry({ kind: 'ready' }), false)
assert.equal(showPrivacyRetry({ kind: 'verifying' }), false)
assert.equal(showPrivacyRetry({ kind: 'downloading', percent: 3 }), false)
assert.equal(showPrivacyRetry({ kind: 'unavailable', reason: 'not_compiled_in' }), false)
for (const reason of ['model_unavailable', 'download_failed', 'low_disk', 'verification_failed'] as const) {
  assert.equal(showPrivacyRetry({ kind: 'unavailable', reason }), true, reason)
}

console.log('privacyCheckBanner.test.ts: ok')
