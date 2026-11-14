// Plain-assertion test for collectionConsent.ts (items.id=920 stage 3).
//
//   node --experimental-strip-types src/chat/collectionConsent.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import type { CollectionConsentState } from '../bindings.ts'
import {
  COLLECTION_CONSENT_FOCUS_IDS,
  consentErrorKind,
  displayedTextVersion,
  shouldAskForConsent,
  usesCollectionConsent,
} from './collectionConsent.ts'

const state = (over: Partial<CollectionConsentState>): CollectionConsentState => ({
  consent: 'not_asked',
  stored_version: 0,
  current_text_version: 1,
  needs_decision: true,
  promises: ['withdraw_any_time'],
  ...over,
})

// Only the backend's needs_decision decides who is asked: the other fields do
// not change the answer, whatever they say.
assert.equal(shouldAskForConsent(state({ needs_decision: true })), true)
assert.equal(shouldAskForConsent(state({ needs_decision: false })), false)
// A declined user at the current version, as the backend reports them: not asked.
assert.equal(
  shouldAskForConsent(state({ consent: 'declined', stored_version: 1, current_text_version: 1, needs_decision: false })),
  false,
)
// A withdrawn user: not asked again.
assert.equal(
  shouldAskForConsent(state({ consent: 'withdrawn', stored_version: 0, needs_decision: false })),
  false,
)
// A granted user at an older text, as the backend reports them: asked.
assert.equal(
  shouldAskForConsent(state({ consent: 'granted', stored_version: 1, current_text_version: 2, needs_decision: true })),
  true,
)
// The frontend does NOT second-guess the backend: versions that look stale with
// needs_decision false are not asked, and the reverse.
assert.equal(shouldAskForConsent(state({ consent: 'granted', stored_version: 1, current_text_version: 9, needs_decision: false })), false)
assert.equal(shouldAskForConsent(state({ consent: 'granted', stored_version: 5, current_text_version: 5, needs_decision: true })), true)

// A decision carries the version DISPLAYED (the one in force when read).
assert.equal(displayedTextVersion(state({ current_text_version: 3, stored_version: 1 })), 3)

// Which Focus asks.
assert.deepEqual([...COLLECTION_CONSENT_FOCUS_IDS], ['tech-support'])
assert.equal(usesCollectionConsent('tech-support'), true)
for (const other of ['quick-ask', 'writing-assistant', 'research-and-buy', 'api-chat', '']) {
  assert.equal(usesCollectionConsent(other), false, other)
}

// Error codes group by what the user is told.
assert.equal(consentErrorKind('consent_text_stale'), 'stale')
for (const code of ['focus_settings_not_found', 'consent_text_unavailable', 'persona_not_found', 'invalid_focus_id']) {
  assert.equal(consentErrorKind(code), 'unavailable', code)
}
for (const code of ['no_consent_to_withdraw', 'consent_audit_failed: x', 'something else']) {
  assert.equal(consentErrorKind(code), 'other', code)
}

// -- Source scan: the frontend never recomputes who is asked ---------------------
// The rule lives in the backend (needs_decision). No frontend file may compare the
// stored version with the current text version, or branch on consent === 'not_asked'
// to decide whether to ask. (collectionConsent.ts's own text names the fields in
// comments only, and the dev fake backend is the one place that holds a copy of the
// rule, because it IS a fake backend.)
const chatDir = path.dirname(fileURLToPath(import.meta.url))
const checked = [
  'CollectionConsentSection.tsx',
  'CollectionConsentConfirm.tsx',
  'CollectionConsentManage.tsx',
  'useCollectionConsent.ts',
  'collectionConsent.ts',
  'collectionConsentApi.ts',
  'ChatPane.tsx',
]
const code = (file: string) =>
  fs
    .readFileSync(path.join(chatDir, file), 'utf8')
    .split('\n')
    .filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l))
    .join('\n')
for (const file of checked) {
  const src = code(file)
  assert.ok(!/stored_version\s*[!=]==?\s*[\w.]*current_text_version/.test(src), `${file} compares versions`)
  assert.ok(!/current_text_version\s*[!=]==?\s*[\w.]*stored_version/.test(src), `${file} compares versions`)
  assert.ok(!/consent\s*===?\s*'not_asked'\s*\?/.test(src) || file === 'CollectionConsentConfirm.tsx', `${file} decides on not_asked`)
}
// The section does use the backend's field.
assert.ok(code('collectionConsent.ts').includes('state.needs_decision'))
assert.ok(code('CollectionConsentSection.tsx').includes('shouldAskForConsent(state)'))

console.log('collectionConsent.test.ts: all assertions passed')
