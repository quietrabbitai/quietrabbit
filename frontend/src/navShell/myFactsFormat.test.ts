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
  stop_fact_count: 0,
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

// A section stop states every fact it ends, and says when sections inside go too.
assert.match(stopSectionBody(t, 1, false), /the 1 fact in this section for/)
assert.match(stopSectionBody(t, 3, false), /the 3 facts in this section for/)
assert.match(stopSectionBody(t, 1, true), /the 1 fact in this section and the sections inside it/)
assert.match(stopSectionBody(t, 5, true), /the 5 facts in this section and the sections inside it/)
assert.match(stopSectionBody(t, 0, true), /sections inside it/)
assert.doesNotMatch(stopSectionBody(t, 0, false), /inside/)
// No sentence says a section inside is left running.
for (const inside of [true, false]) {
  for (const n of [0, 1, 2]) {
    assert.doesNotMatch(stopSectionBody(t, n, inside), /not stopped|can still be read/)
  }
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
// items.id=960: the Sharing block's words, navShell.facts.sharing.* and the
// fact_policy.* keys it shows. They keep the rules above (walked again here for
// fact_policy.*, which is outside navShell.facts) and four more: nothing is promised
// with "always" or "never" (decisions.id=866: say what the user controls), no claim
// of where a value stays, no tier and no digit, and no unqualified guarantee.
const sharing: string[] = []
const collect = (v: unknown) => {
  if (typeof v === 'string') sharing.push(v)
  else for (const child of Object.values(v as Record<string, unknown>)) collect(child)
}
collect(en.navShell.facts.sharing)
collect(en.fact_policy)
assert.equal(sharing.length, 18 + 12)
for (const s of sharing) {
  assert.doesNotMatch(s, /\bQR\b/, s)
  assert.doesNotMatch(s, /won.t use|will not use|no longer uses?/i, s)
  assert.doesNotMatch(s, /delet/i, s)
  assert.doesNotMatch(s, /\b(always|never)\b/i, s)
  assert.doesNotMatch(
    s,
    /this computer|this device|your (computer|device|machine)|on.device|\blocal/i,
    s,
  )
  assert.doesNotMatch(s, /\btier|\d/i, s)
  assert.doesNotMatch(s, /guarantee|\bcannot leave\b|\bprivate\b|\bsafe\b/i, s)
}
// What Keep back cannot catch is stated, and the fixed line claims no place.
for (const limit of [
  /exact text/,
  /typed into a chat/,
  /other words/,
  /under four characters/,
  /this saved fact only/,
]) {
  assert.match(t('navShell.facts.sharing.limits'), limit)
}
assert.equal(t('navShell.facts.sharing.fixed'), 'Kept back from cloud providers.')
assert.equal(t('fact_policy.policy.omit'), 'Keep back')
// Share on the anonymous route is scoped; Describe is a label, not the detail.
assert.match(t('navShell.facts.sharing.anonymousScope'), /only when/)
assert.match(t('navShell.facts.sharing.anonymousScope'), /follows your signed-in choice/)
assert.match(t('navShell.facts.sharing.describeNote'), /only a label .* not the detail itself/)
// A change is not claimed to wait for new work.
assert.match(t('navShell.facts.sharing.next'), /can also reach work that has already started/)

// The limits of a stop are stated.
assert.match(t('navShell.facts.stop.limits'), /keep what they say/)
assert.match(t('navShell.facts.stop.limits'), /already started may still use what you stop/)
assert.match(t('navShell.facts.stop.limits'), /shared/)
assert.match(t('navShell.facts.stop.syncMayRestore'), /can come back/)
// items.id=962, 964: a stop holds until the sharing side writes the fact anew, so
// the next update alone is not what brings it back.
assert.match(t('navShell.facts.stop.syncMayRestore'), /Removed list/)
assert.match(t('navShell.facts.stop.syncMayRestore'), /changes it or saves it again/)
assert.doesNotMatch(t('navShell.facts.stop.syncMayRestore'), /next time that Persona sends an update/)

console.log('myFactsFormat.test.ts: all assertions passed')
