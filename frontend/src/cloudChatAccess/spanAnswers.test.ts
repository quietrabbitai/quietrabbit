// items.id=924: what the modal offers for a span, from its allowed answers.
//   node --experimental-strip-types src/cloudChatAccess/spanAnswers.test.ts
import assert from 'node:assert/strict'
import {
  ALL_ANSWERS,
  allowedOf,
  editCarriesOriginal,
  releaseAvailable,
  rememberReleaseAvailable,
  saveForPersonaFor,
} from './spanAnswers.ts'

const span = (allowed?: { release: boolean; standing_release: boolean }) => ({
  original_text: 'PF2ABC3D',
  fact_key: 'device_fact:abc',
  allowed,
})

// No `allowed` (the copy review, every filter span): every answer, as before.
assert.deepEqual(allowedOf(span()), ALL_ANSWERS)
assert.equal(releaseAvailable(span()), true)
assert.equal(rememberReleaseAvailable(span()), true)
assert.equal(editCarriesOriginal(span(), 'PF2ABC3D'), false)
assert.equal(saveForPersonaFor(span(), 'release_original', true), true)

// A span the server restricts to a live release (an access-credential field; the serial
// number is not one since decisions.id=956): release offered, remembering it is not.
const liveOnly = span({ release: true, standing_release: false })
assert.equal(releaseAvailable(liveOnly), true)
assert.equal(rememberReleaseAvailable(liveOnly), false)
assert.equal(saveForPersonaFor(liveOnly, 'release_original', true), false)
assert.equal(saveForPersonaFor(liveOnly, 'generalize', true), true)
assert.equal(saveForPersonaFor(liveOnly, 'keep_private', true), true)

// A span with no release (a never value that reached the review, D1).
const never = span({ release: false, standing_release: false })
assert.equal(releaseAvailable(never), false)
assert.equal(rememberReleaseAvailable(never), false)
assert.equal(editCarriesOriginal(never, 'serial pf2abc3d, the laptop'), true)
assert.equal(editCarriesOriginal(never, '[serial number]'), false)
assert.equal(editCarriesOriginal(span({ release: true, standing_release: true }), 'PF2ABC3D'), false)

// No fact key or not ticked: never saved.
assert.equal(saveForPersonaFor({ ...span(), fact_key: null }, 'generalize', true), false)
assert.equal(saveForPersonaFor(span(), 'generalize', false), false)
