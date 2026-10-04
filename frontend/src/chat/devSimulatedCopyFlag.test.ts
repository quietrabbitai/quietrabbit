// Plain-assertion test for devSimulatedCopyFlag.ts (items.id=501 slice 2).
//
//   node --experimental-strip-types src/chat/devSimulatedCopyFlag.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { buildSimulatedFlag, devSimulatedCopyFlag } from './devSimulatedCopyFlag.ts'
import { applyAllCopyDecisions, applyCopyDecisions } from './copyDecisions.ts'

// Guard: under Node there is no Vite env (and no window), so the guarded
// entry point must be inert -- the same inertness a production build gets.
assert.equal(devSimulatedCopyFlag('Email Ana today please', 'run-1'), null)

const text = 'Email Café Ana today please'
const { payload: maybePayload, auto: auto1 } = buildSimulatedFlag(text, 'clipboard-copy-chat-p-1', 1)
assert.ok(maybePayload)
assert.equal(auto1.length, 0)
const payload = maybePayload
assert.equal(payload.focus_run_id, 'clipboard-copy-chat-p-1')
assert.equal(payload.spans.length, 2)
assert.equal(payload.review_tier, 'high')

// Offsets are valid UTF-8 byte offsets that slice back to the original word.
const bytes = new TextEncoder().encode(text)
for (const span of payload.spans) {
  assert.equal(new TextDecoder().decode(bytes.subarray(span.start_byte, span.end_byte)), span.original_text)
}
assert.deepEqual(
  payload.spans.map((s) => s.original_text),
  ['Email', 'please'],
)

// Round-trips through the real decision-application code.
const applied = applyCopyDecisions(text, payload.spans, [
  { span_id: 'sim-1', decision: 'generalize', suggestion_text: '[a person]', user_modified_text: null, category: 'private_person', fact_key: null, save_for_persona: false },
  { span_id: 'sim-2', decision: 'keep_private', suggestion_text: null, user_modified_text: null, category: 'private_email', fact_key: null, save_for_persona: false },
])
assert.equal(applied, '[a person] Café Ana today [removed]')

// No words -> empty spans (the forced-High, nothing-to-replace shape).
assert.equal(buildSimulatedFlag('a b', 'r', 1).payload?.spans.length, 0)

// Mode 2: first word interactive, last word silently kept private.
{
  const { payload: p2, auto: a2 } = buildSimulatedFlag(text, 'r', 2)
  assert.ok(p2)
  assert.equal(p2.spans.length, 1)
  assert.equal(a2.length, 1)
  assert.equal(a2[0].decision, 'keep_private')
  const out = applyAllCopyDecisions(
    text,
    p2.spans,
    [{ span_id: 'sim-1', decision: 'generalize', suggestion_text: '[a person]', user_modified_text: null, category: 'private_person', fact_key: null, save_for_persona: false }],
    a2,
  )
  assert.equal(out, '[a person] Café Ana today [removed]')
}

// Mode 3: no modal, everything silently resolved; last word redacted.
{
  const { payload: p3, auto: a3 } = buildSimulatedFlag(text, 'r', 3)
  assert.equal(p3, null)
  assert.equal(applyAllCopyDecisions(text, [], [], a3), 'Email Café Ana today [removed]')
}

console.log('devSimulatedCopyFlag.test.ts: all assertions passed')
