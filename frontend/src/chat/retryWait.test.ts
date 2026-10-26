// Plain-assertion test for retryWait.ts (items.id=817), same convention as
// lateReplyRecovery.test.ts: runs on Node's built-in TypeScript support, no
// framework.
//
//   node --experimental-strip-types src/chat/retryWait.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  MIN_VISIBLE_WAIT_SECS,
  RetryWaitStore,
  SELF_CLEAR_GRACE_MS,
  STALE_RUN_MS,
  applyRetryWaitEvent,
  cancelRetryWait,
  isRetryWaitPayload,
  retryWaitRow,
  type RetryWaitPayload,
} from './retryWait.ts'

function payload(overrides: Partial<RetryWaitPayload> = {}): RetryWaitPayload {
  return {
    focus_run_id: 'run-1',
    focus_name: 'Quick Ask',
    step_display_name: 'Draft',
    provider_id: 'prov-1',
    model_id: 'model-1',
    reason: 'busy',
    seconds: 5,
    attempt: 1,
    max_attempts: 3,
    state: 'waiting',
    ...overrides,
  }
}

const T0 = 1_000_000

// -- the 2-second minimum ------------------------------------------------------

{
  assert.equal(MIN_VISIBLE_WAIT_SECS, 2)
  const short = applyRetryWaitEvent(undefined, payload({ seconds: 1 }), T0)
  assert.equal(retryWaitRow(short, T0), null, 'a 1 s wait shows no row')
  const edge = applyRetryWaitEvent(undefined, payload({ seconds: 2 }), T0)
  assert.notEqual(retryWaitRow(edge, T0), null, 'a 2 s wait shows the row')
  assert.equal(retryWaitRow(undefined, T0), null, 'no state, no row')
}

// -- the countdown ticks -------------------------------------------------------

{
  const s = applyRetryWaitEvent(undefined, payload({ seconds: 5 }), T0)
  const left = (ms: number) => retryWaitRow(s, T0 + ms)?.secondsLeft
  assert.equal(left(0), 5)
  assert.equal(left(1), 5, 'rounds up: 4.999 s left reads 5')
  assert.equal(left(1000), 4)
  assert.equal(left(2500), 3)
  assert.equal(left(4999), 1)
  const row = retryWaitRow(s, T0 + 4999)
  assert.equal(row?.mode, 'countdown')
  const atZero = retryWaitRow(s, T0 + 5000)
  assert.equal(atZero?.secondsLeft, 0)
  assert.equal(atZero?.mode, 'again', 'at 0 it says "Trying again…", not "in 0 seconds"')
  const first = retryWaitRow(s, T0)
  assert.equal(first?.attempt, 1)
  assert.equal(first?.maxAttempts, 3)
  assert.equal(first?.reason, 'busy')
  assert.equal(first?.providerId, 'prov-1')
  assert.equal(first?.modelId, 'model-1')
  assert.equal(first?.focusName, 'Quick Ask')
  assert.equal(first?.stepDisplayName, 'Draft')
}

// -- the clear signal and the self-clear ----------------------------------------

{
  const waiting = applyRetryWaitEvent(undefined, payload({ seconds: 5 }), T0)
  const cleared = applyRetryWaitEvent(waiting, payload({ state: 'clear' }), T0 + 1200)
  assert.equal(retryWaitRow(cleared, T0 + 1300), null, 'the clear event removes the row')

  // A clear that never arrives: the row removes itself shortly after its deadline.
  const deadline = T0 + 5000
  assert.notEqual(retryWaitRow(waiting, deadline + SELF_CLEAR_GRACE_MS - 1), null)
  assert.equal(retryWaitRow(waiting, deadline + SELF_CLEAR_GRACE_MS), null, 'self-clears after the deadline')

  // A late clear for an OLDER attempt does not remove a newer wait.
  const second = applyRetryWaitEvent(cleared, payload({ attempt: 2, seconds: 6 }), T0 + 5100)
  const lateClear = applyRetryWaitEvent(second, payload({ attempt: 1, state: 'clear' }), T0 + 5200)
  assert.equal(lateClear, second, 'unchanged object: nothing to notify')
  assert.notEqual(retryWaitRow(lateClear, T0 + 5300), null)
  // A clear with nothing on hand changes nothing.
  assert.equal(applyRetryWaitEvent(undefined, payload({ state: 'clear' }), T0), undefined)
}

// -- later retries keep showing 'Trying again…' --------------------------------

{
  const first = applyRetryWaitEvent(undefined, payload({ seconds: 4, attempt: 1 }), T0)
  const between = applyRetryWaitEvent(first, payload({ attempt: 1, state: 'clear' }), T0 + 4000)
  assert.equal(retryWaitRow(between, T0 + 4100), null, 'between waits the row is gone (the retry is running)')
  // The next wait is only 1 s: it still shows, as 'again', instead of vanishing.
  const next = applyRetryWaitEvent(between, payload({ attempt: 2, seconds: 1 }), T0 + 4200)
  const row = retryWaitRow(next, T0 + 4200)
  assert.notEqual(row, null, 'a short later wait keeps the row')
  assert.equal(row?.mode, 'again')
  assert.equal(row?.attempt, 2)
  // A later wait of 2 s or more counts down again.
  const third = applyRetryWaitEvent(next, payload({ attempt: 3, seconds: 3 }), T0 + 6000)
  assert.equal(retryWaitRow(third, T0 + 6000)?.mode, 'countdown')
  // A run that never had a long wait never shows a short one.
  const never = applyRetryWaitEvent(undefined, payload({ attempt: 1, seconds: 1 }), T0)
  const never2 = applyRetryWaitEvent(never, payload({ attempt: 2, seconds: 1 }), T0 + 1500)
  assert.equal(retryWaitRow(never2, T0 + 1500), null)
}

// -- one screen-reader announcement per wait ------------------------------------

{
  const s = applyRetryWaitEvent(undefined, payload({ seconds: 8, attempt: 1 }), T0)
  const keys = new Set<string | undefined>()
  for (let ms = 0; ms <= 8000; ms += 250) keys.add(retryWaitRow(s, T0 + ms)?.announcementKey)
  assert.equal(keys.size, 1, 'the key never changes while the countdown ticks')
  const s2 = applyRetryWaitEvent(s, payload({ seconds: 8, attempt: 2 }), T0 + 9000)
  assert.notEqual(
    retryWaitRow(s2, T0 + 9000)?.announcementKey,
    retryWaitRow(s, T0)?.announcementKey,
    'a new wait is a new announcement',
  )
  const other = applyRetryWaitEvent(undefined, payload({ focus_run_id: 'run-2', seconds: 8 }), T0)
  assert.notEqual(retryWaitRow(other, T0)?.announcementKey, retryWaitRow(s, T0)?.announcementKey)
}

// -- the module-level store: events before any subscriber ----------------------

{
  const store = new RetryWaitStore()
  // The event arrives before any pane knows the run id.
  store.apply(payload({ focus_run_id: 'run-early', seconds: 10 }), T0)
  assert.equal(store.get('run-other'), undefined, 'keyed by run id')
  // A late subscriber (a pane that mounted after the event) still sees the row...
  let woken = 0
  const unsubscribe = store.subscribe(() => woken++)
  const late = retryWaitRow(store.get('run-early'), T0 + 3000)
  assert.notEqual(late, null, 'a late subscriber sees the wait')
  assert.equal(late?.secondsLeft, 7, 'counted from the event, not from the mount')
  // ...until its deadline (plus the self-clear grace).
  assert.equal(retryWaitRow(store.get('run-early'), T0 + 10_000 + SELF_CLEAR_GRACE_MS), null)

  // A remount keeps it too: nothing about the store belongs to a component.
  unsubscribe()
  assert.notEqual(retryWaitRow(store.get('run-early'), T0 + 4000), null, 'survives an unmount')

  // Subscribers are woken by a change, and not by a no-op.
  const seen: number[] = []
  const off = store.subscribe(() => seen.push(1))
  store.apply(payload({ focus_run_id: 'run-early', state: 'clear', attempt: 1 }), T0 + 5000)
  assert.equal(seen.length, 1, 'a clear wakes subscribers')
  assert.equal(retryWaitRow(store.get('run-early'), T0 + 5001), null)
  store.apply(payload({ focus_run_id: 'run-never-seen', state: 'clear' }), T0 + 5002)
  assert.equal(seen.length, 1, 'a clear for an unknown run is not a change')
  off()
  assert.equal(woken, 0, 'the unsubscribed listener was not called')

  // Stale runs are forgotten so the store stays bounded.
  store.apply(payload({ focus_run_id: 'run-old', seconds: 3 }), T0)
  store.apply(payload({ focus_run_id: 'run-new', seconds: 3 }), T0 + STALE_RUN_MS + 1)
  assert.equal(store.get('run-old'), undefined)
  assert.notEqual(store.get('run-new'), undefined)
  store.forget('run-new')
  store.forget('run-early')
  assert.equal(store.size, 0)
}

// -- Cancel calls cancel_run, once, with the right arguments --------------------

{
  const calls: unknown[][] = []
  const err = await cancelRetryWait(
    async (...args) => {
      calls.push(args)
      return { status: 'ok' }
    },
    'run-9',
    'user-1',
    'persona-1',
  )
  assert.equal(err, null)
  assert.deepEqual(calls, [['run-9', 'user-1', 'persona-1']], 'exactly one cancel_run call')

  const failed = await cancelRetryWait(
    async () => ({ status: 'error', error: 'run not found' }),
    'run-9',
    'user-1',
    'persona-1',
  )
  assert.equal(failed, 'run not found', 'an error is returned for the pane to show')
}

// -- the payload guard -----------------------------------------------------------

{
  assert.equal(isRetryWaitPayload(payload()), true)
  assert.equal(isRetryWaitPayload(null), false)
  assert.equal(isRetryWaitPayload({ ...payload(), reason: 'mystery' }), false)
  assert.equal(isRetryWaitPayload({ ...payload(), state: 'paused' }), false)
  assert.equal(isRetryWaitPayload({ ...payload(), seconds: '5' }), false)
  const { model_id: _omit, ...missing } = payload()
  assert.equal(isRetryWaitPayload(missing), false)
}

// -- wording: every key the row uses exists in en.json ---------------------------

{
  const here = path.dirname(fileURLToPath(import.meta.url))
  const en = JSON.parse(fs.readFileSync(path.join(here, '..', 'i18n', 'en.json'), 'utf8'))
  const rw = en.navShell.chat.retryWait
  for (const key of [
    'providerFallback',
    'reasonBusy',
    'reasonSlow',
    'reasonUnavailable',
    'where',
    'whereNoFocus',
    'countdownOne',
    'countdownOther',
    'again',
    'announce',
    'cancel',
    'cancelling',
    'cancelError',
  ]) {
    assert.equal(typeof rw[key], 'string', `navShell.chat.retryWait.${key}`)
  }
  assert.match(rw.reasonBusy, /is busy/)
  assert.match(rw.reasonSlow, /slow to answer/)
  assert.match(rw.reasonUnavailable, /isn't available right now/)
  assert.match(rw.countdownOther, /Trying again in \{\{count\}\} seconds · Retry \{\{attempt\}\} of \{\{max\}\}/)
  // The announcement carries no per-tick number.
  assert.doesNotMatch(rw.announce, /\{\{(count|seconds|secondsLeft)\}\}/)
}

console.log('retryWait.test.ts: all assertions passed')
