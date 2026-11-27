// Plain-assertion test for factSharingController.ts (items.id=960 unit 2), same
// convention as factSharing.test.ts: Node's built-in TypeScript support, fake
// commands answered by hand.
//
//   node --experimental-strip-types src/navShell/factSharingController.test.ts
//
// (also wired as part of `npm test`.) The last part reads FactSharing.tsx and
// MyFactsView.tsx as text: the repo has no DOM test runner, so the wiring between the
// controls, the controller and the two commands is pinned on the source.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  createSharingController,
  INITIAL_SHARING,
  type SharingCommands,
  type SharingState,
} from './factSharingController.ts'
import type { FactPolicyInfo } from '../bindings.ts'

const tier = (n: 2 | 3, effective: string) => ({
  tier: n,
  choice: null,
  default_policy: 'omit',
  effective,
  can_share: n === 2,
})
/** The policies, told apart by what the anonymous route of job_title holds. */
const policies = (effective: string): FactPolicyInfo[] => [
  {
    field_name: 'job_title',
    label: 'Job title',
    sensitivity: 'personal',
    tier2: tier(2, effective),
    tier3: tier(3, 'omit'),
  },
]

type Settle = { ok: (data?: unknown) => void; error: (code: string) => void; reject: () => void }

/** Commands whose every call waits to be answered by the test. */
function fake() {
  const gets: Settle[] = []
  const sets: (Settle & { args: unknown[] })[] = []
  const wait = (store: (s: Settle) => void) =>
    new Promise<never>((resolve, reject) => {
      store({
        ok: (data) => resolve({ status: 'ok', data } as never),
        error: (code) => resolve({ status: 'error', error: code } as never),
        reject: () => reject(new Error('sqlite: secret detail')),
      })
    })
  const commands: SharingCommands = {
    get: () => wait((s) => gets.push(s)),
    set: (...args) => wait((s) => sets.push({ ...s, args })),
  }
  const states: SharingState[] = []
  const controller = createSharingController(commands, (s) => states.push(s))
  const now = () => states.at(-1) ?? INITIAL_SHARING
  return { gets, sets, states, controller, now }
}

/** Let every settled promise run its continuation. */
const tick = () => new Promise((resolve) => setTimeout(resolve, 0))

async function ready(effective = 'omit') {
  const f = fake()
  void f.controller.load()
  f.gets[0].ok(policies(effective))
  await tick()
  return f
}

const shown = (s: SharingState) =>
  s.load.kind === 'ready' ? s.load.policies[0].tier2.effective : s.load.kind

// The first read: loading, then what was read, or failed with the code.
{
  const f = fake()
  void f.controller.load()
  assert.deepEqual(f.now(), { load: { kind: 'loading' }, busy: false, writeError: null })
  f.gets[0].ok(policies('summarize'))
  await tick()
  assert.equal(shown(f.now()), 'summarize')
}
for (const [answer, code] of [
  [(s: Settle) => s.error('persona_not_found'), 'persona_not_found'],
  [(s: Settle) => s.reject(), 'fact_policy_unavailable'],
] as const) {
  const f = fake()
  void f.controller.load()
  answer(f.gets[0])
  await tick()
  assert.deepEqual(f.now().load, { kind: 'failed', code })
}

// A write: the command gets the field, the route's tier number and the word. Nothing
// shown changes until the RE-READ answers: not when the write is sent, and not when
// the write answers, whose own entry is never used.
for (const [tierNumber, word] of [
  [2, 'pass'],
  [2, 'summarize'],
  [3, 'summarize'],
  [3, 'omit'],
] as const) {
  const f = await ready('omit')
  void f.controller.choose('job_title', tierNumber, word)
  assert.deepEqual(f.sets[0].args, ['job_title', tierNumber, word])
  assert.equal(f.now().busy, true)
  assert.equal(shown(f.now()), 'omit', 'no optimistic value')
  f.sets[0].ok(policies('ask')[0])
  await tick()
  assert.equal(f.gets.length, 2, 'read again after the write')
  assert.equal(shown(f.now()), 'omit', "the write's own entry is not shown")
  assert.equal(f.now().busy, true, 'disabled until the re-read answers')
  f.gets[1].ok(policies('summarize'))
  await tick()
  assert.equal(shown(f.now()), 'summarize')
  assert.equal(f.now().busy, false)
  assert.equal(f.now().writeError, null)
  assert.ok(
    f.states.every((s) => shown(s) !== 'pass' && shown(s) !== 'ask'),
    'only read values were ever shown',
  )
}

// A refused or failed write is read again too, and what the read says is shown: the
// command can fail after it stored the choice.
for (const [answer, code] of [
  [(s: Settle) => s.error('fact_policy_unavailable'), 'fact_policy_unavailable'],
  [(s: Settle) => s.error('pass_not_allowed'), 'pass_not_allowed'],
  [(s: Settle) => s.reject(), 'fact_policy_unavailable'],
] as const) {
  for (const stored of ['omit', 'pass']) {
    const f = await ready('omit')
    void f.controller.choose('job_title', 2, 'pass')
    answer(f.sets[0])
    await tick()
    assert.equal(f.gets.length, 2, `read again after ${code}`)
    f.gets[1].ok(policies(stored))
    await tick()
    assert.equal(shown(f.now()), stored)
    assert.deepEqual(f.now().writeError, { field: 'job_title', code })
    assert.equal(f.now().busy, false)
  }
}

// A re-read that fails drops the old values, whatever the write answered.
for (const write of [(s: Settle) => s.ok(), (s: Settle) => s.error('invalid_policy')]) {
  for (const [reread, code] of [
    [(s: Settle) => s.error('catalogue_unavailable'), 'catalogue_unavailable'],
    [(s: Settle) => s.reject(), 'fact_policy_unavailable'],
  ] as const) {
    const f = await ready('pass')
    void f.controller.choose('job_title', 2, 'omit')
    write(f.sets[0])
    await tick()
    reread(f.gets[1])
    await tick()
    assert.deepEqual(f.now().load, { kind: 'failed', code })
    assert.equal(f.now().busy, false)
    // No control over a failed read: a choice does nothing.
    void f.controller.choose('job_title', 2, 'pass')
    assert.equal(f.sets.length, 1)
    // Retry reads again and clears the line.
    void f.controller.load()
    assert.equal(f.now().writeError, null)
    f.gets[2].ok(policies('omit'))
    await tick()
    assert.equal(shown(f.now()), 'omit')
  }
}

// One write at a time, whatever the field or route; nothing before the first read.
{
  const f = await ready()
  void f.controller.choose('job_title', 2, 'pass')
  void f.controller.choose('job_title', 2, 'summarize')
  void f.controller.choose('employer', 3, 'omit')
  assert.equal(f.sets.length, 1)
  f.sets[0].ok()
  await tick()
  void f.controller.choose('job_title', 2, 'summarize')
  assert.equal(f.sets.length, 1, 'still busy until the re-read answers')
  f.gets[1].ok(policies('pass'))
  await tick()
  void f.controller.choose('job_title', 2, 'summarize')
  assert.equal(f.sets.length, 2)
}
{
  const f = fake()
  void f.controller.load()
  void f.controller.choose('job_title', 2, 'pass')
  assert.equal(f.sets.length, 0)
}

// Only the newest read is applied.
{
  const f = fake()
  void f.controller.load()
  void f.controller.load()
  f.gets[1].ok(policies('summarize'))
  await tick()
  f.gets[0].ok(policies('pass'))
  await tick()
  assert.equal(shown(f.now()), 'summarize')
}
// A reload during a write (a stop elsewhere) cannot put the older value back.
{
  const f = await ready('pass')
  void f.controller.choose('job_title', 2, 'omit')
  void f.controller.load()
  f.sets[0].ok()
  await tick()
  f.gets[2].ok(policies('omit'))
  await tick()
  f.gets[1].ok(policies('pass'))
  await tick()
  assert.equal(shown(f.now()), 'omit')
  assert.equal(f.now().busy, false)
}

// After dispose nothing is emitted and nothing new is written.
{
  const f = await ready('omit')
  void f.controller.choose('job_title', 2, 'pass')
  f.controller.dispose()
  const emitted = f.states.length
  f.sets[0].error('fact_policy_unavailable')
  await tick()
  f.gets[1]?.ok(policies('pass'))
  await tick()
  void f.controller.choose('job_title', 2, 'summarize')
  void f.controller.load()
  assert.equal(f.states.length, emitted)
  assert.equal(f.sets.length, 1)
}

// The error line can be cleared (opening or closing a panel).
{
  const f = await ready()
  void f.controller.choose('job_title', 2, 'pass')
  f.sets[0].error('invalid_policy')
  await tick()
  f.gets[1].ok(policies('omit'))
  await tick()
  f.controller.clearError()
  assert.equal(f.now().writeError, null)
  assert.equal(shown(f.now()), 'omit')
}

// The wiring, on the source.
const HERE = path.dirname(fileURLToPath(import.meta.url))
const SRC = path.resolve(HERE, '..')
const read = (rel: string) => fs.readFileSync(path.join(HERE, rel), 'utf8')
const component = read('FactSharing.tsx')
const view = read('MyFactsView.tsx')

// Every control sends its OWN route's tier number: the select and the ask buttons.
assert.deepEqual(component.match(/onChoose\([^)]*\)/g), [
  'onChoose(route.tier, word)',
  'onChoose(route.tier, word)',
])
// The select's word is one of the route's options; the ask buttons are its options.
assert.match(component, /const word = control\.options\.find\(\(w\) => w === e\.target\.value\)/)
assert.match(component, /\{control\.options\.map\(\(word\) => \(\s*<button/)
// What a select shows is the read value, the component keeps no state of its own and
// calls no command, and both kinds of control are disabled while a write is in flight.
assert.match(component, /value=\{control\.value\}/)
assert.doesNotMatch(component, /useState|useReducer|defaultValue|commands\.|bindings/)
assert.equal(component.match(/disabled=\{busy\}/g)?.length, 2)

// The view binds the two commands to its Persona, passing the arguments through.
const flat = view.replace(/\s+/g, ' ')
assert.ok(flat.includes('get: () => commands.getFactPolicies(personaId),'))
assert.ok(
  flat.includes(
    'set: (fieldName, tier, policy) => commands.setFactPolicy(personaId, fieldName, tier, policy),',
  ),
)
assert.ok(flat.includes('return () => controller.dispose()'))
assert.ok(flat.includes('sharingController.current?.choose(fact.label, tier, word)'))
// Nothing else in the app writes the policy.
const files: string[] = []
const walk = (dir: string) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) walk(full)
    else if (/\.tsx?$/.test(entry.name) && !/\.test\.ts$|bindings\.ts$/.test(entry.name)) {
      files.push(full)
    }
  }
}
walk(SRC)
const writers = files.filter((f) => fs.readFileSync(f, 'utf8').includes('setFactPolicy'))
assert.deepEqual(
  writers.map((f) => path.relative(SRC, f)),
  ['navShell/MyFactsView.tsx'],
)
// Only a General fact is told it is in General.
assert.ok(flat.includes('factLine(visible.general, true)'))
assert.ok(flat.includes('factLine(section.facts, false)'))

console.log('factSharingController.test.ts: all assertions passed')
