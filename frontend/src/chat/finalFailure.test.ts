// Plain-assertion test for finalFailure.ts (items.id=831), same convention as
// retryWait.test.ts: runs on Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/chat/finalFailure.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  FinalFailureStore,
  MAX_REMEMBERED_FAILURES,
  isFinalFailurePayload,
  isFreshReply,
  noticeFor,
  type FinalFailurePayload,
} from './finalFailure.ts'

function payload(overrides: Partial<FinalFailurePayload> = {}): FinalFailurePayload {
  return {
    focus_run_id: 'run-1',
    message_id: 'msg-1',
    reason: 'busy',
    provider_id: 'prov-1',
    model_id: 'model-1',
    ...overrides,
  }
}

// -- the payload guard ------------------------------------------------------

assert.equal(isFinalFailurePayload(payload()), true)
for (const reason of ['busy', 'slow', 'unavailable']) {
  assert.equal(isFinalFailurePayload(payload({ reason: reason as 'busy' })), true)
}
assert.equal(isFinalFailurePayload(payload({ reason: 'mystery' as 'busy' })), false)
assert.equal(isFinalFailurePayload(null), false)
assert.equal(isFinalFailurePayload('x'), false)
for (const key of Object.keys(payload())) {
  const partial: Record<string, unknown> = { ...payload() }
  delete partial[key]
  assert.equal(isFinalFailurePayload(partial), false, `missing ${key}`)
}

// -- the store ---------------------------------------------------------------

{
  const store = new FinalFailureStore()
  let woken = 0
  const off = store.subscribe(() => {
    woken += 1
  })
  const v0 = store.version()
  assert.equal(store.get('run-1'), undefined)
  assert.equal(store.get(null), undefined)
  store.apply(payload())
  assert.equal(store.get('run-1')?.message_id, 'msg-1')
  assert.equal(woken, 1)
  assert.notEqual(store.version(), v0)

  // A newer failure of the same run replaces the older one.
  store.apply(payload({ message_id: 'msg-2', reason: 'slow' }))
  assert.equal(store.get('run-1')?.message_id, 'msg-2')
  assert.equal(store.get('run-1')?.reason, 'slow')

  // clear forgets it, wakes subscribers once, and is silent when nothing is there.
  const before = woken
  store.clear('run-1')
  assert.equal(store.get('run-1'), undefined)
  assert.equal(woken, before + 1)
  store.clear('run-1')
  assert.equal(woken, before + 1, 'clearing nothing wakes nobody')

  off()
  store.apply(payload())
  assert.equal(woken, before + 1, 'an unsubscribed listener hears nothing')
}

{
  const store = new FinalFailureStore()
  for (let i = 0; i < MAX_REMEMBERED_FAILURES + 5; i++) {
    store.apply(payload({ focus_run_id: `run-${i}`, message_id: `msg-${i}` }))
  }
  assert.equal(store.get('run-0'), undefined, 'the oldest is dropped')
  assert.equal(store.get('run-4'), undefined)
  assert.ok(store.get('run-5'))
  assert.ok(store.get(`run-${MAX_REMEMBERED_FAILURES + 4}`))
}

// -- which reply carries the notice -----------------------------------------

{
  const store = new FinalFailureStore()
  store.apply(payload())
  const row = { id: 'msg-1', is_error: true, focus_run_id: 'run-1' }
  assert.ok(noticeFor(row, store))
  // Not a failed reply: a real reply to the same run has no notice.
  assert.equal(noticeFor({ ...row, is_error: false }, store), undefined)
  // The event names a different reply row.
  assert.equal(noticeFor({ ...row, id: 'msg-9' }, store), undefined)
  // No run, or a run with no event (for example after an app restart).
  assert.equal(noticeFor({ ...row, focus_run_id: null }, store), undefined)
  assert.equal(noticeFor({ ...row, focus_run_id: 'run-2' }, store), undefined)
}

// -- Try again must not take the old sentence for the new reply -------------

{
  const baseline = { runId: 'run-1', staleContent: 'the failure sentence' }
  assert.equal(isFreshReply('', 'run-1', null), false, 'empty is never a reply')
  assert.equal(isFreshReply('hello', 'run-1', null), true, 'no Try again, any text counts')
  assert.equal(isFreshReply('the failure sentence', 'run-1', baseline), false)
  assert.equal(isFreshReply('a real reply', 'run-1', baseline), true)
  assert.equal(
    isFreshReply('another failure sentence', 'run-1', baseline),
    true,
    'a different failure is a result too',
  )
  assert.equal(
    isFreshReply('the failure sentence', 'run-2', baseline),
    true,
    'the baseline belongs to one run only',
  )
}

console.log('finalFailure.test.ts: ok')
