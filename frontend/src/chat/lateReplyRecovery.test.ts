// Plain-assertion test for lateReplyRecovery.ts (items.id=587), matching
// navShell/unsentChat.test.ts's convention: run directly on Node's
// built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/chat/lateReplyRecovery.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  classifyRunStatus,
  findInFlightAssistantMessage,
} from './lateReplyRecovery.ts'
import type { MessageInfo } from '../bindings.ts'

function msg(overrides: Partial<MessageInfo>): MessageInfo {
  return {
    id: crypto.randomUUID(),
    context_key: 'chat-1',
    sender: 'user',
    content: '',
    focus_run_id: null,
    gate3_review_status: null,
    created_at: '2026-08-09T00:00:00Z',
    ...overrides,
  }
}

// -- findInFlightAssistantMessage --------------------------------------

assert.equal(
  findInFlightAssistantMessage([]),
  null,
  'empty history has nothing in flight',
)

assert.equal(
  findInFlightAssistantMessage([
    msg({ sender: 'user', content: 'hi' }),
    msg({ sender: 'assistant', content: 'hello there', focus_run_id: 'run-1' }),
  ]),
  null,
  'a backfilled (non-empty) assistant row is not in flight',
)

assert.equal(
  findInFlightAssistantMessage([
    msg({ sender: 'user', content: 'hi' }),
    msg({ sender: 'assistant', content: '', focus_run_id: null }),
  ]),
  null,
  'an empty row with no focus_run_id is not a run placeholder at all',
)

{
  const inFlight = msg({ sender: 'assistant', content: '', focus_run_id: 'run-2' })
  const found = findInFlightAssistantMessage([
    msg({ sender: 'user', content: 'hi' }),
    inFlight,
  ])
  assert.equal(found?.id, inFlight.id, 'finds the empty placeholder with a focus_run_id')
}

{
  // Only the MOST RECENT in-flight row matters -- an older empty row from
  // a prior, now-resolved send (reached via a genuinely dead/paused
  // classification, not backfilled text) must not shadow a real later one.
  const older = msg({ sender: 'assistant', content: '', focus_run_id: 'run-old' })
  const newer = msg({ sender: 'assistant', content: '', focus_run_id: 'run-new' })
  const found = findInFlightAssistantMessage([
    msg({ sender: 'user', content: 'hi' }),
    older,
    msg({ sender: 'user', content: 'follow-up' }),
    newer,
  ])
  assert.equal(found?.id, newer.id, 'picks the most recent in-flight row, not an older one')
}

// -- classifyRunStatus ---------------------------------------------------

assert.equal(classifyRunStatus('running'), 'active')
assert.equal(classifyRunStatus('initializing'), 'active')
assert.equal(classifyRunStatus('failed'), 'dead')
assert.equal(classifyRunStatus('cancelled'), 'dead')
assert.equal(classifyRunStatus('paused'), 'inactive', 'a consent/Gate3 pause is not this component\'s concern')
assert.equal(classifyRunStatus('awaiting_user'), 'inactive')
assert.equal(classifyRunStatus('awaiting_feedback'), 'inactive')
assert.equal(classifyRunStatus('awaiting_extract_confirm'), 'inactive')
assert.equal(classifyRunStatus('awaiting_schedule'), 'inactive')
assert.equal(classifyRunStatus('complete'), 'inactive', 'should not normally coexist with empty content, but is not "dead" copy')
assert.equal(classifyRunStatus(null), 'inactive', 'an unknown run_id (Ok(None) from get_run_status) is treated conservatively, not as dead')

console.log('lateReplyRecovery.test.ts: all assertions passed')
