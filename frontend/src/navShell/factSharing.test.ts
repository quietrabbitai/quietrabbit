// Plain-assertion test for factSharing.ts (items.id=960 unit 1), same convention as
// myFactsFormat.test.ts: Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/factSharing.test.ts
//
// (also wired as part of `npm test`.) It reads the REAL en.json, the REAL catalogue
// file and the REAL list of error codes in commands/fact_policy.rs.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  CATALOGUED_FIELDS,
  isPartlyShared,
  sharingBlock,
  sharingErrorText,
  sharingWordText,
  type PoliciesLoad,
  type SharingBlock,
} from './factSharing.ts'
import type { T } from './myFactsFormat.ts'
import type { FactPolicyInfo, FactPolicyTier } from '../bindings.ts'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const read = (rel: string) => fs.readFileSync(path.join(HERE, rel), 'utf8')
const en = JSON.parse(read('../i18n/en.json'))

const t: T = (key) => {
  let node: unknown = en
  for (const part of key.split('.')) node = (node as Record<string, unknown>)?.[part]
  assert.equal(typeof node, 'string', `missing copy: ${key}`)
  return node as string
}

// The names are the catalogue's, in its order.
const catalogue = JSON.parse(read('../../../src-tauri/block_data/personal_fact_fields.json'))
assert.deepEqual(
  [...CATALOGUED_FIELDS],
  catalogue.fields.map((f: { name: string }) => f.name),
)

const tier = (n: 2 | 3, over: Partial<FactPolicyTier> = {}): FactPolicyTier => ({
  tier: n,
  choice: null,
  default_policy: 'omit',
  effective: 'omit',
  can_share: n === 2,
  ...over,
})
const info = (
  over: {
    sensitivity?: string
    tier2?: Partial<FactPolicyTier>
    tier3?: Partial<FactPolicyTier>
    field_name?: string
  } = {},
): FactPolicyInfo => ({
  field_name: over.field_name ?? 'job_title',
  label: 'Job title',
  sensitivity: over.sensitivity ?? 'personal',
  tier2: tier(2, over.tier2),
  tier3: tier(3, over.tier3),
})
const ready = (...policies: FactPolicyInfo[]): PoliciesLoad => ({ kind: 'ready', policies })
const fact = (over: { label?: string; sensitivity?: string; copied_in?: boolean } = {}) => ({
  label: 'job_title',
  sensitivity: 'personal',
  copied_in: false,
  ...over,
})
const block = (load: PoliciesLoad, over: Parameters<typeof fact>[0] = {}) =>
  sharingBlock(fact(over), true, load)

// Who has a block: an exact catalogued name in the General row, and nothing else.
for (const name of CATALOGUED_FIELDS) {
  assert.equal(block(ready(info({ field_name: name })), { label: name }).kind, 'choices')
  assert.equal(sharingBlock(fact({ label: name }), false, ready(info())).kind, 'none')
}
for (const label of ['City', 'Job title', 'JOB_TITLE', 'job_title ', 'income_range', '']) {
  for (const load of [{ kind: 'loading' }, { kind: 'failed', code: 'x' }, ready(info())]) {
    assert.deepEqual(block(load as PoliciesLoad, { label }), { kind: 'none' }, label)
    // A fact that is not catalogued never gets the fixed line either.
    assert.deepEqual(
      block(load as PoliciesLoad, { label, sensitivity: 'medical', copied_in: true }),
      { kind: 'none' },
      label,
    )
  }
}

// The floors a fact carries itself hold whatever the policies are doing, even when
// they say Share.
const shared = ready(info({ tier2: { effective: 'pass', choice: 'pass' } }))
const loads: [string, PoliciesLoad][] = [
  ['loading', { kind: 'loading' }],
  ['failed', { kind: 'failed', code: 'fact_policy_unavailable' }],
  ['ready', shared],
]
for (const sensitivity of ['general', 'personal', 'medical', 'financial', 'mystery', '']) {
  for (const copied of [false, true]) {
    for (const [name, load] of loads) {
      const open = !copied && (sensitivity === 'general' || sensitivity === 'personal')
      const want = !open ? 'fixed' : name === 'ready' ? 'choices' : name
      assert.equal(
        block(load, { sensitivity, copied_in: copied }).kind,
        want,
        `sensitivity=${sensitivity} copied=${copied} load=${name}`,
      )
    }
  }
}
// A missing copied_in is not a clear "no".
assert.equal(
  sharingBlock({ label: 'job_title', sensitivity: 'personal' } as never, true, shared).kind,
  'fixed',
)

// The floors the policies carry.
for (const sensitivity of ['medical', 'financial', 'mystery']) {
  assert.deepEqual(block(ready(info({ sensitivity }))), { kind: 'fixed' }, sensitivity)
}
assert.deepEqual(block(ready(info({ tier2: { effective: 'not_permitted' } }))), { kind: 'fixed' })
assert.deepEqual(block(ready(info({ tier3: { effective: 'not_permitted' } }))), { kind: 'fixed' })

// A load failure keeps its code; no entry for the field fails closed.
assert.deepEqual(block({ kind: 'failed', code: 'persona_not_found' }), {
  kind: 'failed',
  code: 'persona_not_found',
})
assert.deepEqual(block(ready()), { kind: 'failed', code: 'catalogue_unavailable' })
assert.deepEqual(block(ready(info({ field_name: 'employer' }))), {
  kind: 'failed',
  code: 'catalogue_unavailable',
})

// The default: Keep back on both routes, with the note. The routes carry the numbers
// set_fact_policy takes, anonymous first.
assert.deepEqual(block(ready(info())), {
  kind: 'choices',
  fieldName: 'job_title',
  routes: [
    {
      route: 'anonymous',
      tier: 2,
      control: { kind: 'choose', value: 'omit', options: ['pass', 'summarize', 'omit'] },
    },
    {
      route: 'signedIn',
      tier: 3,
      control: { kind: 'choose', value: 'omit', options: ['summarize', 'omit'] },
    },
  ],
  defaultNote: true,
})

const choices = (b: SharingBlock) => {
  assert.equal(b.kind, 'choices')
  return b as Extract<SharingBlock, { kind: 'choices' }>
}
const control = (b: SharingBlock, i: 0 | 1) => choices(b).routes[i].control

// The note goes once either route has a stored choice or is not at Keep back.
for (const over of [
  { tier2: { choice: 'omit' } },
  { tier3: { choice: 'omit' } },
  { tier2: { effective: 'summarize' } },
  { tier3: { effective: 'summarize' } },
]) {
  assert.equal(choices(block(ready(info(over)))).defaultNote, false, JSON.stringify(over))
}

// The signed-in route never offers Share, even if the backend said it could; the
// anonymous one offers it only when it can.
const lying = block(ready(info({ tier3: { can_share: true } })))
assert.deepEqual(control(lying, 1), {
  kind: 'choose',
  value: 'omit',
  options: ['summarize', 'omit'],
})
const closed = block(ready(info({ tier2: { can_share: false } })))
assert.deepEqual(control(closed, 0), {
  kind: 'choose',
  value: 'omit',
  options: ['summarize', 'omit'],
})

// The value is the EFFECTIVE word, never the stored choice.
const refused = block(
  ready(
    info({
      tier2: { choice: 'pass', effective: 'omit' },
      tier3: { choice: 'pass', effective: 'omit' },
    }),
  ),
)
for (const i of [0, 1] as const) {
  assert.equal((control(refused, i) as { value: string }).value, 'omit')
}
const unset = block(ready(info({ tier2: { choice: null, effective: 'summarize' } })))
assert.equal((control(unset, 0) as { value: string }).value, 'summarize')

// Every effective word on each route. A word a route cannot hold fails the whole
// block: never Keep back, never Share.
const failedClosed = { kind: 'failed', code: 'fact_policy_unavailable' }
for (const word of ['pass', 'summarize', 'omit']) {
  assert.equal(
    (control(block(ready(info({ tier2: { effective: word } }))), 0) as { value: string }).value,
    word,
  )
}
for (const word of ['summarize', 'omit']) {
  assert.equal(
    (control(block(ready(info({ tier3: { effective: word } }))), 1) as { value: string }).value,
    word,
  )
}
for (const word of ['range_only', 'share', 'PASS', '', '???']) {
  assert.deepEqual(block(ready(info({ tier2: { effective: word } }))), failedClosed, word)
  assert.deepEqual(block(ready(info({ tier3: { effective: word } }))), failedClosed, word)
}
assert.deepEqual(block(ready(info({ tier3: { effective: 'pass' } }))), failedClosed)
assert.deepEqual(block(ready(info({ tier3: { effective: 'ask' } }))), failedClosed)
assert.deepEqual(
  block(ready(info({ tier2: { effective: 'pass', can_share: false } }))),
  failedClosed,
)
// A stored ask is shown as itself and can be replaced by Describe or Keep back only:
// never Share, even where the route could share, and never ask again.
for (const canShare of [true, false]) {
  const asking = block(
    ready(info({ tier2: { choice: 'ask', effective: 'ask', can_share: canShare } })),
  )
  assert.deepEqual(control(asking, 0), { kind: 'ask', options: ['summarize', 'omit'] })
}

// Partly shared: some route is not at Keep back. Never for a fixed, loading, failed or
// absent block.
assert.equal(isPartlyShared(block(ready(info()))), false)
for (const over of [
  { tier2: { effective: 'pass' } },
  { tier2: { effective: 'summarize' } },
  { tier2: { effective: 'ask' } },
  { tier3: { effective: 'summarize' } },
]) {
  assert.equal(isPartlyShared(block(ready(info(over)))), true, JSON.stringify(over))
}
assert.equal(isPartlyShared(block(shared, { sensitivity: 'medical' })), false)
assert.equal(isPartlyShared(block(shared, { copied_in: true })), false)
assert.equal(isPartlyShared(block({ kind: 'loading' })), false)
assert.equal(isPartlyShared(block({ kind: 'failed', code: 'x' })), false)
assert.equal(isPartlyShared(sharingBlock(fact(), false, shared)), false)

// Words and errors come from the existing keys; every code the commands can return has
// its own sentence, and anything else reads as unavailable.
for (const word of ['pass', 'summarize', 'omit', 'ask'] as const) {
  assert.equal(sharingWordText(t, word), en.fact_policy.policy[word])
}
const rust = read('../../../src-tauri/src/commands/fact_policy.rs')
const list = /pub const ERROR_CODES: \[&str; (\d+)\] = \[([^\]]*)\]/.exec(rust)
assert.ok(list)
const codes = [...list[2].matchAll(/"([a-z0-9_]+)"/g)].map((m) => m[1])
assert.equal(codes.length, Number(list[1]))
for (const code of codes) {
  assert.equal(sharingErrorText(t, code), en.fact_policy.error[code], code)
}
for (const code of ['sqlite: boom', '', 'not logged in']) {
  assert.equal(sharingErrorText(t, code), en.fact_policy.error.fact_policy_unavailable)
}

console.log('factSharing.test.ts: all assertions passed')
