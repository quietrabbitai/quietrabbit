// Plain-assertion test for myFactsFormat.ts (items.id=959), same convention as
// sharedLogFormat.test.ts: Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/myFactsFormat.test.ts
//
// (also wired as part of `npm test`.) It runs against the REAL en.json copy, and also
// pins what that copy must never claim.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  descendantCount,
  fieldLabel,
  nestSections,
  provenanceText,
  sensitivityTag,
  stopSectionBody,
  stopSectionInside,
  writeErrorText,
  type T,
} from './myFactsFormat.ts'
import type { SectionView } from '../bindings.ts'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(HERE, '..', 'i18n', 'en.json'), 'utf8'))

const t: T = (key, params) => {
  let node: unknown = en
  for (const part of key.split('.')) node = (node as Record<string, unknown>)?.[part]
  assert.equal(typeof node, 'string', `missing copy: ${key}`)
  return (node as string).replace(/\{\{(\w+)\}\}/g, (_, n) => String(params?.[n]))
}

const section = (id: string, parent: string | null): SectionView => ({
  id,
  entity_type: 'person',
  name: id,
  parent_id: parent,
  facts: [],
})

// Labels.
assert.equal(fieldLabel(t, 'location_city'), 'City')
assert.equal(fieldLabel(t, 'job_title'), 'Job title')
assert.equal(fieldLabel(t, 'preferred_airline'), 'Preferred airline')
assert.equal(fieldLabel(t, 'City'), 'City')

// Provenance: no invented line.
assert.equal(provenanceText(t, 'you_confirmed'), 'You confirmed this')
assert.equal(provenanceText(t, 'you_corrected'), 'You corrected this')
assert.equal(provenanceText(t, 'from_chat'), 'From a chat')
assert.equal(provenanceText(t, 'found_by_quiet_rabbit'), 'Found by Quiet Rabbit')
assert.equal(provenanceText(t, null), null)

assert.equal(sensitivityTag(t, 'medical'), 'Medical')
assert.equal(sensitivityTag(t, 'financial'), 'Financial')
assert.equal(sensitivityTag(t, 'personal'), null)

// Nesting: descendants of any depth sit under their top-level section.
const flat = [section('a', null), section('b', 'a'), section('c', 'b'), section('d', null)]
const nodes = nestSections(flat)
assert.deepEqual(
  nodes.map((n) => [n.section.id, n.children.map((c) => c.id)]),
  [
    ['a', ['b', 'c']],
    ['d', []],
  ],
)
assert.deepEqual(
  nestSections([section('x', 'gone')]).map((n) => n.section.id),
  ['x'],
)
assert.equal(descendantCount(flat, 'a'), 2)
assert.equal(descendantCount(flat, 'b'), 1)
assert.equal(descendantCount(flat, 'd'), 0)

// A section stop states its own facts, and says sections inside are NOT stopped.
assert.match(stopSectionBody(t, 1), /the 1 fact/)
assert.match(stopSectionBody(t, 3), /3 facts/)
assert.ok(stopSectionBody(t, 0).length > 0)
for (const text of [stopSectionInside(t, 1), stopSectionInside(t, 2)]) {
  assert.match(text, /not stopped/)
  assert.match(text, /can still be read/)
}

assert.equal(writeErrorText(t, 'newer_value_exists'), t('navShell.facts.error.newer'))
assert.equal(writeErrorText(t, 'fact_not_found'), t('navShell.facts.error.gone'))
assert.equal(writeErrorText(t, 'section_not_found'), t('navShell.facts.error.gone'))
assert.equal(writeErrorText(t, 'sqlite: boom'), t('navShell.facts.error.generic'))

// The copy never says "QR" (decisions.id=889 addendum), never claims an unqualified
// "won't use it", never claims deletion, and never names a computer-only guarantee.
const strings: string[] = []
const walk = (v: unknown) => {
  if (typeof v === 'string') strings.push(v)
  else for (const child of Object.values(v as Record<string, unknown>)) walk(child)
}
walk(en.navShell.facts)
assert.ok(strings.length > 40)
for (const s of strings) {
  assert.doesNotMatch(s, /\bQR\b/, s)
  assert.doesNotMatch(s, /won.t use|will not use|never use|no longer uses?/i, s)
  assert.doesNotMatch(s, /delet/i, s)
  assert.doesNotMatch(s, /this computer/i, s)
}
// The limits of a stop are stated.
assert.match(t('navShell.facts.stop.limits'), /keep what they say/)
assert.match(t('navShell.facts.stop.limits'), /already started may still use what you stop/)
assert.match(t('navShell.facts.stop.limits'), /shared/)
assert.match(t('navShell.facts.stop.syncMayRestore'), /can come back/)

console.log('myFactsFormat.test.ts: all assertions passed')
