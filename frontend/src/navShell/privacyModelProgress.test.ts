// Plain-assertion test for privacyModelProgress.ts (items.id=678), matching
// privacyCheckBanner.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/privacyModelProgress.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  effectOfProgress,
  isPrivacyModelProgress,
  mergeHealthRead,
  PRIVACY_MODEL_KIND,
  TASK_PROGRESS_EVENT,
} from './privacyModelProgress.ts'
import type { PrivacyFilterStatus } from '../bindings.ts'

const event = (phase: string, current: number | null = null, total: number | null = null) => ({
  task_id: 't1',
  kind: PRIVACY_MODEL_KIND,
  phase,
  current,
  total,
  message: null,
})

assert.equal(TASK_PROGRESS_EVENT, 'task-progress')

// Only this task's events are taken; other kinds and malformed payloads are not.
assert.equal(isPrivacyModelProgress(event('downloading', 1, 2)), true)
assert.equal(isPrivacyModelProgress({ ...event('downloading'), kind: 'key_rotation' }), false)
assert.equal(isPrivacyModelProgress({ ...event('downloading'), kind: 'ollama_pull' }), false)
assert.equal(isPrivacyModelProgress({ ...event('downloading'), task_id: 7 }), false)
assert.equal(isPrivacyModelProgress({ ...event('downloading'), phase: undefined }), false)
assert.equal(isPrivacyModelProgress({ ...event('downloading'), current: 'x' }), false)
assert.equal(isPrivacyModelProgress(null), false)
assert.equal(isPrivacyModelProgress('downloading'), false)
assert.equal(isPrivacyModelProgress(undefined), false)

// A verifying event (a hash of a file already on disk) is the plain verifying state.
assert.deepEqual(effectOfProgress(event('verifying')), {
  kind: 'status',
  status: { kind: 'verifying' },
})

// Downloading carries a whole percent from the byte counts; the total is known
// from the start, and a missing figure is "unknown", never 0%.
assert.deepEqual(effectOfProgress(event('downloading', 0, 1000)), {
  kind: 'status',
  status: { kind: 'downloading', percent: 0 },
})
assert.deepEqual(effectOfProgress(event('downloading', 425, 1000)), {
  kind: 'status',
  status: { kind: 'downloading', percent: 42 },
})
assert.deepEqual(effectOfProgress(event('downloading', 1000, 1000)), {
  kind: 'status',
  status: { kind: 'downloading', percent: 100 },
})
assert.deepEqual(effectOfProgress(event('downloading', 2000, 1000)), {
  kind: 'status',
  status: { kind: 'downloading', percent: 100 },
})
assert.deepEqual(effectOfProgress(event('downloading', null, null)), {
  kind: 'status',
  status: { kind: 'downloading', percent: null },
})
assert.deepEqual(effectOfProgress(event('downloading', 5, 0)), {
  kind: 'status',
  status: { kind: 'downloading', percent: null },
})

// A final event only says "ask the backend": it carries no state of its own.
for (const phase of ['complete', 'failed', 'cancelled']) {
  assert.deepEqual(effectOfProgress(event(phase)), { kind: 'reread' }, phase)
}

// The phase vocabulary is open: an unknown phase changes nothing.
assert.deepEqual(effectOfProgress(event('something_new')), { kind: 'ignore' })

// A health answer is applied unless an event has already arrived.
const ready: PrivacyFilterStatus = { kind: 'ready' }
const downloading: PrivacyFilterStatus = { kind: 'downloading', percent: 40 }
assert.deepEqual(mergeHealthRead(false, null, ready), ready)
assert.deepEqual(mergeHealthRead(false, downloading, ready), ready)
assert.deepEqual(mergeHealthRead(true, downloading, ready), downloading)
assert.equal(mergeHealthRead(true, null, ready), null)

console.log('privacyModelProgress.test.ts: ok')
