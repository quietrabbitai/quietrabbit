// Plain-assertion test for stepReview.ts (items.id=884 stage 2).
//
//   node --experimental-strip-types src/stepReview/stepReview.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import type { PendingStepReview, PendingStepReviewSpan } from '../bindings.ts'
import type { ElementDecision } from '../cloudChatAccess/consentDecisions.ts'
import {
  INITIAL_STATE,
  MAX_AUTO_RESUMES,
  errorKeyOf,
  isStepReviewRunId,
  modalPayload,
  reduce,
  shown,
  type Effect,
  type HostEvent,
  type HostState,
} from './stepReview.ts'

const RUN = '11111111-1111-4111-8111-111111111111'
const OTHER = '22222222-2222-4222-8222-222222222222'

const span = (id: string, tier: 'low' | 'medium' | 'high' = 'high'): PendingStepReviewSpan => ({
  span_id: id,
  category: 'private_person',
  user_label: 'Person name',
  original_text: 'Ann Smith',
  suggestion: '[person]',
  start_byte: 9,
  end_byte: 18,
  score: 0.95,
  review_tier: tier,
  fact_key: null,
})

const review = (id: string, state: 'ready' | 'needs_resume' = 'ready', run = RUN): PendingStepReview => ({
  review_id: id,
  run_id: run,
  action_id: 'draft',
  state,
  spans: state === 'ready' ? [span('s1')] : [],
})

const decision = (id: string): ElementDecision => ({
  span_id: id,
  decision: 'generalize',
  suggestion_text: '[person]',
  user_modified_text: null,
  category: 'private_person',
  fact_key: null,
  save_for_persona: false,
})

function run(events: HostEvent[], from: HostState = INITIAL_STATE): { state: HostState; effects: Effect[] } {
  let state = from
  const effects: Effect[] = []
  for (const e of events) {
    const step = reduce(state, e)
    state = step.state
    effects.push(...step.effects)
  }
  return { state, effects }
}

// -- which run ids are a step review's ---------------------------------------------
assert.equal(isStepReviewRunId(RUN), true)
assert.equal(isStepReviewRunId('clipboard-copy-chat-p1-123'), false, 'a copy review is left to ChatPane')
assert.equal(isStepReviewRunId('api-chat-send-p1-123'), false, 'an API Chat send is not ours')
assert.equal(isStepReviewRunId(''), false)
assert.deepEqual(reduce(INITIAL_STATE, { type: 'consent_request', runId: 'clipboard-copy-chat-x' }).effects, [])
assert.deepEqual(reduce(INITIAL_STATE, { type: 'run_status', runId: 'api-chat-send-x', status: 'awaiting_user' }).effects, [])

// -- the event is a hint: it only asks the backend; nothing opens without an answer
{
  const a = reduce(INITIAL_STATE, { type: 'consent_request', runId: RUN })
  assert.deepEqual(a.effects, [{ kind: 'pull', runId: RUN }])
  assert.equal(shown(a.state), null, 'no modal from an event alone')
  const b = reduce(INITIAL_STATE, { type: 'run_status', runId: RUN, status: 'awaiting_user' })
  assert.deepEqual(b.effects, [{ kind: 'pull', runId: RUN }])
  // Other statuses ask nothing.
  assert.deepEqual(reduce(INITIAL_STATE, { type: 'run_status', runId: RUN, status: 'running' }).effects, [])
  // A pull that finds nothing (a Gate 3 promotion's request, or the race before the
  // record exists) opens nothing.
  const c = reduce(INITIAL_STATE, { type: 'pulled', runId: RUN, review: null })
  assert.equal(shown(c.state), null)
}

// -- a ready review opens the modal, and the same review keeps the modal as it is
{
  const { state } = run([{ type: 'pulled', runId: RUN, review: review('r1') }])
  const s = shown(state)
  assert.equal(s?.kind, 'modal')
  assert.equal(s?.kind === 'modal' && s.review.review_id, 'r1')
  assert.equal(s?.kind === 'modal' && s.sending, false)
  // Another pull of the same review changes nothing (the modal keeps its edits).
  const again = reduce(state, { type: 'pulled', runId: RUN, review: review('r1') })
  assert.equal(again.state, state, 'the very same state object')
  // A different review of the same run replaces it (the old one was superseded).
  const swapped = reduce(state, { type: 'pulled', runId: RUN, review: review('r2') })
  const t = shown(swapped.state)
  assert.equal(t?.kind === 'modal' && t.review.review_id, 'r2')
}

// -- needs_resume is answered by resume_run, at most MAX_AUTO_RESUMES times in a row
{
  const r = review('r1', 'needs_resume')
  let { state, effects } = run([{ type: 'pulled', runId: RUN, review: r }])
  assert.deepEqual(effects, [{ kind: 'resume', runId: RUN }])
  assert.equal(shown(state), null, 'nothing to show while the run is asked again')
  assert.equal(state.runs[RUN].phase, 'resuming')
  for (let i = 1; i < MAX_AUTO_RESUMES; i++) {
    const step = reduce(state, { type: 'pulled', runId: RUN, review: review(`r${i + 1}`, 'needs_resume') })
    assert.deepEqual(step.effects, [{ kind: 'resume', runId: RUN }])
    state = step.state
  }
  // One more needs_resume, and the host stops and says so.
  const last = reduce(state, { type: 'pulled', runId: RUN, review: review('rx', 'needs_resume') })
  assert.deepEqual(last.effects, [])
  assert.deepEqual(shown(last.state), { kind: 'stuck', runId: RUN })
  // Check again starts over from nothing and pulls.
  const re = reduce(last.state, { type: 'recheck', runId: RUN })
  assert.deepEqual(re.effects, [{ kind: 'pull', runId: RUN }])
  assert.equal(shown(re.state), null)
  assert.equal(re.state.resumes[RUN], undefined)
  // Dismissing the stuck notice leaves the run parked and forgets it.
  assert.equal(shown(reduce(last.state, { type: 'dismiss', runId: RUN }).state), null)
  void effects
}

// -- a review that can be shown resets the resume count; the resumed run's review opens
{
  const a = reduce(INITIAL_STATE, { type: 'pulled', runId: RUN, review: review('r1', 'needs_resume') })
  assert.equal(a.state.resumes[RUN], 1)
  const b = reduce(a.state, { type: 'pulled', runId: RUN, review: review('r2') })
  assert.equal(shown(b.state)?.kind, 'modal')
  assert.equal(b.state.resumes[RUN], undefined)
}

// -- a failed resume (the run is gone or finished) drops the run
{
  const a = reduce(INITIAL_STATE, { type: 'pulled', runId: RUN, review: review('r1', 'needs_resume') })
  assert.equal(shown(reduce(a.state, { type: 'resume_failed', runId: RUN }).state), null)
}

// -- a modal whose run parks again or ends is dismissed
{
  const open = run([{ type: 'pulled', runId: RUN, review: review('r1') }]).state
  // The run parked blocked (or at the spend gate): awaiting_user, and the backend has
  // no pending review any more.
  const parked = run([
    { type: 'run_status', runId: RUN, status: 'awaiting_user' },
    { type: 'pulled', runId: RUN, review: null },
  ], open)
  assert.equal(shown(parked.state), null, 'the stale modal is dismissed')
  assert.deepEqual(parked.effects, [{ kind: 'pull', runId: RUN }])
  // The run ended.
  for (const status of ['failed', 'cancelled', 'complete']) {
    assert.equal(shown(reduce(open, { type: 'run_status', runId: RUN, status }).state), null, status)
  }
  // A failed pull does not dismiss a modal (the answer is unknown).
  assert.equal(reduce(open, { type: 'pulled', runId: RUN, review: null, failed: true }).state, open)
  // Another run's events leave this modal alone.
  assert.equal(shown(reduce(open, { type: 'run_status', runId: OTHER, status: 'complete' }).state)?.kind, 'modal')
}

// -- answering: record, resume, then the run's own status says the rest
{
  const open = run([{ type: 'pulled', runId: RUN, review: review('r1') }]).state
  const d = [decision('s1')]
  const a = reduce(open, { type: 'answer', runId: RUN, decisions: d })
  assert.deepEqual(a.effects, [{ kind: 'send', runId: RUN, decisions: d }])
  const s = shown(a.state)
  assert.equal(s?.kind === 'modal' && s.sending, true)
  // While the answer is on its way a pull that finds nothing must NOT dismiss it.
  assert.equal(reduce(a.state, { type: 'pulled', runId: RUN, review: null }).state, a.state)
  // Done: the run is released from the host.
  assert.equal(shown(reduce(a.state, { type: 'send_done', runId: RUN }).state), null)
  // An answer outside the review phase does nothing.
  assert.deepEqual(reduce(INITIAL_STATE, { type: 'answer', runId: RUN, decisions: d }).effects, [])
  // A double click sends once: the second answer arrives in the sending phase.
  assert.deepEqual(reduce(a.state, { type: 'answer', runId: RUN, decisions: d }).effects, [])

  // Failure keeps the review and the choices, and Try again sends the same ones.
  const failed = reduce(a.state, { type: 'send_failed', runId: RUN, message: 'step_review_unavailable' })
  assert.deepEqual(shown(failed.state), {
    kind: 'error',
    runId: RUN,
    message: 'step_review_unavailable',
    canRetry: true,
  })
  const retry = reduce(failed.state, { type: 'retry_send', runId: RUN })
  assert.deepEqual(retry.effects, [{ kind: 'send', runId: RUN, decisions: d }])
  assert.equal(shown(retry.state)?.kind, 'modal')
}

// -- refusing: record it; the backend cancels the run
{
  const open = run([{ type: 'pulled', runId: RUN, review: review('r1') }]).state
  const a = reduce(open, { type: 'decline', runId: RUN })
  assert.deepEqual(a.effects, [{ kind: 'decline', runId: RUN }])
  assert.equal(shown(reduce(a.state, { type: 'decline_done', runId: RUN }).state), null)
  const failed = reduce(a.state, { type: 'decline_failed', runId: RUN, message: 'step_review_unavailable' })
  const s = shown(failed.state)
  assert.equal(s?.kind === 'error' && s.canRetry, false, 'a failed refusal cannot "send again"')
  // From the error notice the user can still refuse.
  assert.deepEqual(reduce(failed.state, { type: 'decline', runId: RUN }).effects, [{ kind: 'decline', runId: RUN }])
  assert.deepEqual(reduce(INITIAL_STATE, { type: 'decline', runId: RUN }).effects, [])
}

// -- one thing at a time, the earliest run first
{
  const two = run([
    { type: 'pulled', runId: RUN, review: review('r1') },
    { type: 'pulled', runId: OTHER, review: review('r2', 'ready', OTHER) },
  ]).state
  const s = shown(two)
  assert.equal(s?.kind === 'modal' && s.runId, RUN)
  const after = reduce(two, { type: 'run_status', runId: RUN, status: 'cancelled' }).state
  const t = shown(after)
  assert.equal(t?.kind === 'modal' && t.runId, OTHER)
}

// -- the mount read: each review the list finds goes through the same `pulled` event
{
  // After a restart: one review's text is gone (needs_resume), another's is held.
  const listed = [review('r1', 'needs_resume', RUN), review('r2', 'ready', OTHER)]
  const { state, effects } = run(
    listed.map((r) => ({ type: 'pulled', runId: r.run_id, review: r }) as HostEvent),
  )
  assert.deepEqual(effects, [{ kind: 'resume', runId: RUN }], 'only the one that needs it is resumed')
  const s = shown(state)
  assert.equal(s?.kind === 'modal' && s.runId, OTHER, 'the ready one is shown meanwhile')
  assert.equal(state.runs[RUN].phase, 'resuming')
  // The resumed run's review comes back ready and takes its place in the queue.
  const after = reduce(state, { type: 'pulled', runId: RUN, review: review('r3', 'ready', RUN) }).state
  assert.equal(after.runs[RUN].phase, 'review')
  // An empty list changes nothing.
  assert.equal(run([]).state, INITIAL_STATE)
}

// -- the hook reads the list once on mount, after its listeners are attached ------------
{
  const dir = path.dirname(fileURLToPath(import.meta.url))
  const hook = fs
    .readFileSync(path.join(dir, 'useStepReview.ts'), 'utf8')
    .split('\n')
    .filter((l) => !l.trim().startsWith('//'))
    .join('\n')
  assert.equal(hook.match(/listPendingReviews\(/g)?.length, 1, 'one list call, on mount')
  assert.ok(
    hook.indexOf("listen<ConsentRequestEvent>('consent_request'") < hook.indexOf('listPendingReviews('),
    'the listeners are attached before the list is read',
  )
  assert.ok(hook.includes("type: 'pulled', runId: review.run_id, review"))
  assert.equal(hook.includes('knownRunIds'), false, 'no run id has to be known any more')
}

// -- the modal's payload ---------------------------------------------------------
{
  const p = modalPayload({ ...review('r1'), spans: [span('a', 'low'), span('b', 'medium')] }, 'a request')
  assert.equal(p.focus_run_id, RUN)
  assert.equal(p.focus_name, 'a request')
  assert.equal(p.review_tier, 'medium', 'the worst tier of the spans')
  assert.equal(p.spans.length, 2)
  assert.equal(p.spans[0].span_id, 'a')
  assert.equal(p.spans[0].original_text, 'Ann Smith')
  // A score the backend could not export (NaN) reads as 0, never as undefined.
  const nan = modalPayload({ ...review('r1'), spans: [{ ...span('a'), score: null }] }, 'x')
  assert.equal(nan.spans[0].score, 0)
  assert.equal(modalPayload({ ...review('r1'), spans: [] }, 'x').review_tier, 'high')
}

// -- the command's error codes ----------------------------------------------------
assert.equal(errorKeyOf('persona_not_found'), 'persona_not_found')
assert.equal(errorKeyOf('invalid_run_id'), 'invalid_run_id')
assert.equal(errorKeyOf('step_review_unavailable'), 'step_review_unavailable')
assert.equal(errorKeyOf('anything else'), 'step_review_unavailable')

// -- the source: the answer is the backend's, never recomputed here -----------------
{
  const dir = path.dirname(fileURLToPath(import.meta.url))
  const src = fs.readFileSync(path.join(dir, 'stepReview.ts'), 'utf8')
  const code = src
    .split('\n')
    .filter((l) => !l.trim().startsWith('//'))
    .join('\n')
  // This module neither builds nor sends text: it names no prompt, no outbound text
  // and no clipboard.
  for (const banned of ['prompt', 'outbound', 'clipboard', 'navigator', 'invoke(']) {
    assert.equal(code.includes(banned), false, `stepReview.ts mentions ${banned}`)
  }
}

console.log('stepReview.test.ts: all assertions passed')
