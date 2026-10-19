import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  allWriterTextKeys,
  blockerOf,
  canStop,
  IDLE,
  isActive,
  stateAfterChoice,
  stateAfterError,
} from './rotationWriters.ts'

// writers_active -> the prompt, with the counts.
assert.deepEqual(stateAfterError({ code: 'writers_active', replies: 1, focus_runs: 2 }), {
  kind: 'ask',
  replies: 1,
  focusRuns: 2,
})
// The ceiling and the stop grace have their own screens.
assert.deepEqual(stateAfterError({ code: 'writers_still_running', replies: 0, focus_runs: 1 }), {
  kind: 'ceiling',
  replies: 0,
  focusRuns: 1,
})
assert.deepEqual(stateAfterError({ code: 'writer_not_stopping' }), { kind: 'not_stopping' })
// A cancelled wait and every ordinary failure are not prompts.
assert.equal(stateAfterError({ code: 'drain_cancelled' }), null)
assert.equal(stateAfterError({ code: 'invalid_credentials' }), null)
assert.equal(stateAfterError({ code: 'busy' }), null)

// Answering keeps the counts that were on screen, from any prompt.
const ask = { kind: 'ask', replies: 2, focusRuns: 0 } as const
assert.deepEqual(stateAfterChoice(ask, 'finish_first'), {
  kind: 'waiting',
  choice: 'finish_first',
  replies: 2,
  focusRuns: 0,
})
const ceiling = { kind: 'ceiling', replies: 1, focusRuns: 1 } as const
assert.deepEqual(stateAfterChoice(ceiling, 'stop_and_save'), {
  kind: 'waiting',
  choice: 'stop_and_save',
  replies: 1,
  focusRuns: 1,
})
// 'Keep waiting' is finish-first again from the ceiling screen.
assert.equal(stateAfterChoice(ceiling, 'finish_first').kind, 'waiting')

assert.equal(blockerOf(1, 0), 'replies')
assert.equal(blockerOf(0, 3), 'tasks')
assert.equal(blockerOf(2, 1), 'both')
assert.equal(blockerOf(0, 0), 'none')
// With only short work left there is nothing to stop.
assert.equal(canStop(0, 0), false)
assert.equal(canStop(0, 1), true)

assert.equal(isActive(IDLE), false)
assert.equal(isActive(ask), true)
assert.equal(isActive({ kind: 'not_stopping' }), true)

// Every text key the prompt uses exists in en.json.
const here = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(here, '../i18n/en.json'), 'utf8'))
for (const key of allWriterTextKeys()) {
  let node: unknown = en.auth.rotationWriters
  for (const part of key.split('.')) {
    assert.ok(
      typeof node === 'object' && node !== null && Object.hasOwn(node, part),
      `en.json is missing auth.rotationWriters.${key}`,
    )
    node = (node as Record<string, unknown>)[part]
  }
  assert.equal(typeof node, 'string', `auth.rotationWriters.${key} must be a string`)
}

console.log('rotationWriters.test.ts: all assertions passed')
