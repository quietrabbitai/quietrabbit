// Plain-assertion test for rotationProgress.ts, blockingOverlay.ts and
// rotationTask.ts (items.id=769 stage 1), same convention as the other
// *.test.ts files: runs on Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/auth/rotationProgress.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  INITIAL_OVERLAY_STATE,
  isFinalPhase,
  isKeyRotationProgress,
  isNotStartedPhase,
  isWaitingOnLargeStep,
  outcomeSuffix,
  overlayVisible,
  percentOf,
  phaseLabelSuffix,
  reduceOverlay,
  titleSuffix,
  type OverlayState,
  type TaskProgressPayload,
} from './rotationProgress.ts'
import {
  acquireBlockingOverlay,
  isBlockingOverlayActive,
  subscribeBlockingOverlay,
} from './blockingOverlay.ts'
import { beginRotationTask, getRotationTask, subscribeRotationTask } from './rotationTask.ts'

const event = (phase: string, current: number | null = null, total: number | null = null) =>
  ({ type: 'event', payload: { task_id: 'r1', kind: 'key_rotation', phase, current, total } }) as const
const status = (
  running: boolean,
  last: 'none' | 'complete' | 'rolled_back' | 'failed',
  percent: number | null = 0,
  phase = 'rekeying',
) =>
  ({
    type: 'status',
    status: { running, phase: running ? phase : null, percent, last_outcome: last },
  }) as const

// --- payload guard ----------------------------------------------------------
const good: TaskProgressPayload = {
  task_id: 'r1',
  kind: 'key_rotation',
  phase: 'rekeying',
  current: 5,
  total: 10,
  message: 'ignored',
}
assert.equal(isKeyRotationProgress(good), true)
assert.equal(isKeyRotationProgress({ ...good, current: null, total: null }), true)
assert.equal(isKeyRotationProgress({ ...good, kind: 'ollama_pull' }), false, 'other tasks share the channel')
assert.equal(isKeyRotationProgress({ ...good, phase: 3 }), false)
assert.equal(isKeyRotationProgress({ ...good, current: '5' }), false)
assert.equal(isKeyRotationProgress(null), false)
assert.equal(isKeyRotationProgress('key_rotation'), false)

// --- percent ----------------------------------------------------------------
assert.equal(percentOf(0, 100), 0)
assert.equal(percentOf(1, 3), 33)
assert.equal(percentOf(100, 100), 100)
assert.equal(percentOf(150, 100), 100, 'clamped')
assert.equal(percentOf(5, 0), null, 'zero total is indeterminate')
assert.equal(percentOf(null, 100), null)
assert.equal(percentOf(5, null), null)

// --- labels -----------------------------------------------------------------
assert.equal(phaseLabelSuffix('rolling_back'), 'rollingBack')
assert.equal(phaseLabelSuffix('rekeying'), 'rekeying')
assert.equal(phaseLabelSuffix('something_new'), 'working', 'open vocabulary falls back')
assert.equal(phaseLabelSuffix(null), 'working')
assert.equal(titleSuffix('new_phrase'), 'newPhrase')
assert.equal(titleSuffix('password_change'), 'passwordChange')
assert.equal(titleSuffix('cancel'), 'cancel')
assert.equal(titleSuffix('resume'), 'resume')
assert.equal(titleSuffix(null), 'generic')
assert.equal(outcomeSuffix('rolled_back'), 'rolledBack')
assert.equal(outcomeSuffix('failed'), 'failed')
assert.equal(outcomeSuffix(null), 'none')
assert.equal(isFinalPhase('complete'), true)
assert.equal(isFinalPhase('rekeying'), false)

// --- state machine ----------------------------------------------------------
// Cold mount: idle with a stale last_outcome must announce nothing.
let s: OverlayState = reduceOverlay(INITIAL_OVERLAY_STATE, status(false, 'complete'))
assert.equal(s.outcome, null, 'last_outcome is ignored without a running -> idle transition')
assert.equal(overlayVisible(s), false)

// Reload mid-rotation: status says running, later idle -> announce last_outcome.
s = reduceOverlay(INITIAL_OVERLAY_STATE, status(true, 'none', 40))
assert.equal(s.running, true)
assert.equal(s.percent, 40)
assert.equal(overlayVisible(s), true)
s = reduceOverlay(s, status(false, 'rolled_back'))
assert.equal(s.running, false)
assert.equal(s.outcome, 'rolled_back')
assert.equal(overlayVisible(s), true, 'an outcome keeps the overlay up until dismissed')
s = reduceOverlay(s, { type: 'dismiss' })
assert.equal(overlayVisible(s), false)
// "none" after a watched run (an interruption) announces nothing.
s = reduceOverlay(reduceOverlay(INITIAL_OVERLAY_STATE, status(true, 'none')), status(false, 'none'))
assert.equal(s.outcome, null)

// Live events drive progress and the final event is authoritative.
s = reduceOverlay(INITIAL_OVERLAY_STATE, event('preparing'))
assert.equal(s.running, true)
assert.equal(s.percent, null, 'no bytes yet')
s = reduceOverlay(s, event('rekeying', 25, 100))
assert.equal(s.percent, 25)
assert.equal(s.phase, 'rekeying')
s = reduceOverlay(s, event('complete', 100, 100))
assert.equal(s.running, false)
assert.equal(s.outcome, 'complete')
s = reduceOverlay(s, event('rekeying', 1, 100))
assert.equal(s.outcome, null, 'a new rotation clears the old outcome')
s = reduceOverlay(s, event('failed', 0, 0))
assert.equal(s.outcome, 'failed')

// items.id=780: the overlay is up while preparing, with no number to show.
s = reduceOverlay(INITIAL_OVERLAY_STATE, event('preparing', null, null))
assert.equal(overlayVisible(s), true, 'visible from the first report, before any bytes exist')
assert.equal(s.percent, null)
assert.equal(isWaitingOnLargeStep(s), false, 'preparing has its own phase line')
// A status read taken while preparing says the same (percent null, not 0).
s = reduceOverlay(s, status(true, 'none', null, 'preparing'))
assert.equal(s.phase, 'preparing')
assert.equal(s.percent, null, 'a poll must not turn "unknown" into 0%')
assert.equal(overlayVisible(s), true)

// Honest progress across the large database item: determinate, then
// indeterminate for the one opaque step, then determinate again.
s = reduceOverlay(s, event('rekeying', 10, 100))
assert.equal(s.percent, 10)
assert.equal(isWaitingOnLargeStep(s), false)
s = reduceOverlay(s, event('rekeying', null, null))
assert.equal(s.running, true)
assert.equal(s.phase, 'rekeying')
assert.equal(s.percent, null, 'no percentage across a single step with no progress inside it')
assert.equal(isWaitingOnLargeStep(s), true)
s = reduceOverlay(s, status(true, 'none', null))
assert.equal(s.percent, null, 'the status poll keeps the step indeterminate')
assert.equal(isWaitingOnLargeStep(s), true)
s = reduceOverlay(s, event('rekeying', 56, 100))
assert.equal(s.percent, 56)
assert.equal(isWaitingOnLargeStep(s), false, 'determinate again at the next item')
assert.equal(isWaitingOnLargeStep(reduceOverlay(s, event('complete', 100, 100))), false)
assert.equal(isWaitingOnLargeStep(INITIAL_OVERLAY_STATE), false)
// items.id=792: rolling back a large database is the same opaque step as
// re-keying it, so it gets the same hint (and only without a percentage).
s = reduceOverlay(INITIAL_OVERLAY_STATE, event('rekeying', null, null))
assert.equal(isWaitingOnLargeStep(s), true)
s = reduceOverlay(s, event('rolling_back', null, null))
assert.equal(s.phase, 'rolling_back')
assert.equal(s.percent, null)
assert.equal(isWaitingOnLargeStep(s), true, 'rollback of a large database has the hint')
s = reduceOverlay(s, status(true, 'none', null, 'rolling_back'))
assert.equal(isWaitingOnLargeStep(s), true, 'the status poll keeps it')
s = reduceOverlay(s, event('rolling_back', 40, 100))
assert.equal(s.percent, 40)
assert.equal(isWaitingOnLargeStep(s), false, 'a rollback item with counts has a percentage, no hint')
s = reduceOverlay(s, event('rolling_back', null, null))
assert.equal(isWaitingOnLargeStep(s), true)
s = reduceOverlay(s, event('rolled_back', 0, 0))
assert.equal(isWaitingOnLargeStep(s), false, 'ended')
assert.equal(
  isWaitingOnLargeStep(reduceOverlay(INITIAL_OVERLAY_STATE, event('rolling_back', null, null))),
  true,
  'a webview that joins mid-rollback shows it too',
)
for (const phase of ['preparing', 'verifying', 'finishing', 'resuming', 'working']) {
  assert.equal(
    isWaitingOnLargeStep(reduceOverlay(INITIAL_OVERLAY_STATE, event(phase, null, null))),
    false,
    `${phase} never shows the large-step hint`,
  )
}
// Not running (stale phase left on the state) never shows it.
assert.equal(isWaitingOnLargeStep({ ...INITIAL_OVERLAY_STATE, phase: 'rolling_back' }), false)
// A real percent from the status read is still taken as it is.
assert.equal(reduceOverlay(INITIAL_OVERLAY_STATE, status(true, 'none', 0)).percent, 0)

// A refusal after preparing ends the run without announcing anything, owned
// or not, and a later rotation starts clean.
assert.equal(isNotStartedPhase('not_started'), true)
assert.equal(isNotStartedPhase('failed'), false)
assert.equal(isFinalPhase('not_started'), false, 'not an outcome')
s = reduceOverlay(reduceOverlay(INITIAL_OVERLAY_STATE, event('preparing')), event('not_started'))
assert.equal(s.running, false)
assert.equal(s.outcome, null, 'nothing to announce')
assert.equal(overlayVisible(s), false, 'the overlay closes at once')
s = reduceOverlay(
  reduceOverlay(reduceOverlay(INITIAL_OVERLAY_STATE, { type: 'owner', kind: 'restore' }), event('preparing')),
  event('not_started'),
)
assert.equal(overlayVisible(s), false)
assert.equal(s.owned, true, 'ownership is the form\'s to release')
// A poll that lands after the run has already ended changes nothing.
assert.equal(reduceOverlay(s, status(false, 'complete')).outcome, null)
s = reduceOverlay(s, event('preparing'))
assert.equal(overlayVisible(s), true, 'the next attempt shows again')

// items.id=790: a resume or cancel reports from the start with no counts (the
// items are not read yet), and a refusal after that closes it the same way.
for (const first of ['resuming', 'rolling_back']) {
  s = reduceOverlay(INITIAL_OVERLAY_STATE, event(first, null, null))
  assert.equal(s.running, true, `${first} opens the overlay`)
  assert.equal(s.percent, null, `${first} without counts is indeterminate, not 0%`)
  assert.equal(overlayVisible(s), true)
  s = reduceOverlay(s, event('not_started'))
  assert.equal(overlayVisible(s), false, `a refusal after ${first} closes at once`)
  assert.equal(s.outcome, null, 'and announces nothing')
}
// The second resuming report carries counts and takes the bar to a number.
s = reduceOverlay(reduceOverlay(INITIAL_OVERLAY_STATE, event('resuming', null, null)), event('resuming', 0, 100))
assert.equal(s.percent, 0)

// A starter form owns the outcome: progress shows, the overlay stays quiet.
let owned = reduceOverlay(INITIAL_OVERLAY_STATE, { type: 'owner', kind: 'password_change' })
assert.equal(owned.owned, true)
assert.equal(owned.ownerKind, 'password_change')
owned = reduceOverlay(owned, event('rekeying', 1, 2))
assert.equal(overlayVisible(owned), true)
let ownedDone = reduceOverlay(owned, event('rolled_back', 0, 0))
assert.equal(ownedDone.outcome, null)
assert.equal(overlayVisible(ownedDone), false)
ownedDone = reduceOverlay(reduceOverlay(owned, status(true, 'none')), status(false, 'complete'))
assert.equal(ownedDone.outcome, null, 'the status path respects ownership too')
// Releasing ownership again.
assert.equal(reduceOverlay(owned, { type: 'owner', kind: null }).owned, false)

// --- blocking layer store ---------------------------------------------------
let blockingNotifications = 0
const stopBlocking = subscribeBlockingOverlay(() => {
  blockingNotifications++
})
assert.equal(isBlockingOverlayActive(), false)
const releaseA = acquireBlockingOverlay()
const releaseB = acquireBlockingOverlay()
assert.equal(isBlockingOverlayActive(), true)
releaseA()
releaseA() // idempotent
assert.equal(isBlockingOverlayActive(), true, 'one holder is still left')
releaseB()
assert.equal(isBlockingOverlayActive(), false)
assert.equal(blockingNotifications, 4, 'two acquires and the two effective releases')
stopBlocking()
acquireBlockingOverlay()() // no listener left: must not throw
assert.equal(blockingNotifications, 4)

// --- rotation starter registration ------------------------------------------
let taskNotifications = 0
const stopTask = subscribeRotationTask(() => {
  taskNotifications++
})
assert.equal(getRotationTask(), null)
const releaseFirst = beginRotationTask('restore')
assert.equal(getRotationTask(), 'restore')
const releaseSecond = beginRotationTask('new_phrase')
assert.equal(getRotationTask(), 'new_phrase')
releaseFirst() // a stale release must not clear the newer registration
assert.equal(getRotationTask(), 'new_phrase')
releaseSecond()
releaseSecond()
assert.equal(getRotationTask(), null)
assert.equal(taskNotifications, 3)
stopTask()

console.log('rotationProgress.test.ts: all assertions passed')
