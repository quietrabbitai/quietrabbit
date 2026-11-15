// Plain-assertion test for the Tech Support device library screen
// (items.id=891 stage 7): the pure logic, the copy the delete confirmations carry,
// and source scans for what the screen must never do.
//
//   node --experimental-strip-types src/chat/deviceLibrary.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { deviceErrorKind, isRetryable, purgeCounts, removesRecordedData } from './deviceLibrary.ts'

// -- Error codes group by what the user is told ---------------------------------
const kinds: Record<string, ReturnType<typeof deviceErrorKind>> = {
  device_not_found: 'not_found',
  name_in_use: 'name_in_use',
  invalid_device_name: 'invalid_name',
  unknown_field: 'unknown_field',
  invalid_value: 'invalid_value',
  runs_still_running: 'runs_running',
  persona_not_found: 'unavailable',
  invalid_focus_id: 'unavailable',
  catalogue_unavailable: 'unavailable',
  'Database error: x': 'other',
  '': 'other',
}
for (const [code, kind] of Object.entries(kinds)) assert.equal(deviceErrorKind(code), kind, code)
// Only a chat still going is retryable: nothing was deleted and the user tries again.
for (const kind of new Set(Object.values(kinds))) {
  assert.equal(isRetryable(kind), kind === 'runs_running', kind)
}

// -- The counts a confirmation shows are the backend's -----------------------------
assert.deepEqual(purgeCounts({ cases: 2, strategies: 5, changes: 3, facts: 7 }), { facts: 7, cases: 2, changes: 3 })
assert.equal(removesRecordedData({ cases: 0, strategies: 0, changes: 0, facts: 0 }), false)
assert.equal(removesRecordedData({ cases: 0, strategies: 0, changes: 1, facts: 0 }), true)

// -- Files ---------------------------------------------------------------------------
const chatDir = path.dirname(fileURLToPath(import.meta.url))
const srcDir = path.resolve(chatDir, '..')
const read = (rel: string) => fs.readFileSync(path.join(chatDir, rel), 'utf8')
/** Source with comments removed, so a comment may NAME what the code must not do. */
const code = (rel: string) =>
  read(rel)
    .split('\n')
    .filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l))
    .join('\n')

const screenFiles = [
  'DeviceLibrary.tsx',
  'DeleteEverythingControl.tsx',
  'useDeviceLibrary.ts',
  'deviceLibraryApi.ts',
  'deviceLibrary.ts',
  'devSimulatedDeviceLibrary.ts',
]

// Devices are named by handle: no internal id is read, shown or sent.
for (const file of screenFiles) {
  const src = code(file)
  assert.ok(!/entity_?id|entityId|entity-id/i.test(src), `${file} names an internal id`)
}

// Not in R1: archiving, adding a device by hand, any visibility control.
for (const file of screenFiles) {
  const src = code(file)
  for (const forbidden of [
    /archive/i,
    /createDevice|create_device|newDevice/i,
    /redact/i,
    /hide_from_shared|hideFromShared|visibility/i,
  ]) {
    assert.ok(!forbidden.test(src), `${file} has ${forbidden}`)
  }
}

// Every simulated call is behind the literal import.meta.env.DEV (so the fake can
// never ship), and every real command the API uses exists in the bindings.
{
  const api = code('deviceLibraryApi.ts')
  const calls = api.match(/devSimulatedDeviceLibrary\(\)/g) ?? []
  const guards = api.match(/if \(import\.meta\.env\.DEV\) \{/g) ?? []
  assert.equal(calls.length, 8)
  assert.equal(guards.length, calls.length, 'each simulated call sits in an import.meta.env.DEV block')
  const bindings = fs.readFileSync(path.join(srcDir, 'bindings.ts'), 'utf8')
  const used = [...api.matchAll(/commands\.(\w+)\(/g)].map((m) => m[1])
  assert.equal(new Set(used).size, 8)
  for (const name of used) assert.ok(bindings.includes(`${name}: (`), `commands.${name} is not in the bindings`)
}
// The fake is imported by the API module and its own test only (this test names it
// to scan for exactly that).
{
  const importers: string[] = []
  const walk = (dir: string) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name)
      if (entry.isDirectory()) walk(full)
      else if (/\.(ts|tsx)$/.test(entry.name) && fs.readFileSync(full, 'utf8').includes('devSimulatedDeviceLibrary')) {
        importers.push(path.relative(srcDir, full))
      }
    }
  }
  walk(srcDir)
  assert.deepEqual(importers.sort(), [
    'chat/devSimulatedDeviceLibrary.test.ts',
    'chat/devSimulatedDeviceLibrary.ts',
    'chat/deviceLibrary.test.ts',
    'chat/deviceLibraryApi.ts',
  ])
}

// The entry sits beside the memory entry, and the delete-everything control is on
// the memory screen and calls the separate command (not the withdrawal).
{
  const section = code('CollectionConsentSection.tsx')
  assert.ok(section.includes("navShell.chat.devices.button"))
  assert.ok(section.includes("navShell.chat.collectionConsent.manage.button"))
  assert.ok(section.includes('<DeviceLibrary'))
  assert.ok(code('CollectionConsentManage.tsx').includes('<DeleteEverythingControl'))
  const del = code('DeleteEverythingControl.tsx')
  assert.ok(del.includes('purgeEveryDevice('))
  assert.ok(!/withdraw/i.test(del), 'deleting is not part of withdrawing')
}

// -- The copy ----------------------------------------------------------------------
type Json = { [key: string]: string | Json }
const en = JSON.parse(fs.readFileSync(path.join(srcDir, 'i18n/en.json'), 'utf8')) as Json
const at = (p: string): string => {
  let node: string | Json = en
  for (const part of p.split('.')) {
    assert.ok(typeof node === 'object' && part in node, `missing en.json key ${p}`)
    node = node[part]
  }
  assert.equal(typeof node, 'string', p)
  return node as string
}
const chat = 'navShell.chat'

// Both delete confirmations say what they delete: what QuietRabbit recorded about
// the device(s): facts, tickets and configuration changes.
for (const key of [`${chat}.devices.confirmBody`, `${chat}.collectionConsent.manage.deleteAll.confirmBody`]) {
  const text = at(key)
  assert.match(text, /permanently deletes/, key)
  assert.match(text, /recorded/, key)
  assert.match(text, /facts/, key)
  assert.match(text, /tickets/, key)
  assert.match(text, /configuration changes/, key)
}
assert.match(at(`${chat}.devices.confirmBody`), /\{\{facts\}\} facts, \{\{cases\}\} tickets and \{\{changes\}\} configuration changes/)

// And they say what they do NOT delete, never the reverse: no confirmation or
// result sentence claims chat transcripts, earlier replies, run outputs or the
// audit/consent record are deleted.
for (const key of [`${chat}.devices.confirmNotDeleted`, `${chat}.collectionConsent.manage.deleteAll.confirmNotDeleted`]) {
  const text = at(key)
  assert.match(text, /does not delete/, key)
  assert.match(text, /chat history/, key)
  assert.match(text, /earlier replies/, key)
}
const deleteCopy = [
  `${chat}.devices.confirmTitle`,
  `${chat}.devices.confirmBody`,
  `${chat}.devices.confirmPermanent`,
  `${chat}.devices.confirmDelete`,
  `${chat}.devices.purged`,
  `${chat}.collectionConsent.manage.deleteAll.button`,
  `${chat}.collectionConsent.manage.deleteAll.confirmTitle`,
  `${chat}.collectionConsent.manage.deleteAll.confirmBody`,
  `${chat}.collectionConsent.manage.deleteAll.confirm`,
  `${chat}.collectionConsent.manage.deleteAll.done`,
]
for (const key of deleteCopy) {
  const text = at(key)
  assert.ok(!/transcript|audit|run output|conversation|everything linked|all your data/i.test(text), `${key}: ${text}`)
}

// The consent text's delete promise carries the approved wording.
assert.equal(
  at(`${chat}.collectionConsent.promise.delete_device_and_linked_data`),
  'You can delete a device and what QuietRabbit recorded about it (its facts, tickets and configuration changes).',
)

// A refusal because a chat is still going is a retry message, in both places.
for (const key of [`${chat}.devices.error.runsRunning`, `${chat}.collectionConsent.manage.deleteAll.error.runsRunning`]) {
  const text = at(key)
  assert.match(text, /nothing was deleted/i, key)
  assert.match(text, /try again/i, key)
}
// Every error kind has copy.
for (const k of ['notFound', 'nameInUse', 'invalidName', 'unknownField', 'invalidValue', 'runsRunning', 'unavailable', 'other']) {
  assert.ok(at(`${chat}.devices.error.${k}`).length > 0, k)
}

console.log('deviceLibrary.test.ts: all assertions passed')
