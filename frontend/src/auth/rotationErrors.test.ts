// Plain-assertion test for rotationErrors.ts (items.id=769 stage 1), same
// convention as the other *.test.ts files:
//
//   node --experimental-strip-types src/auth/rotationErrors.test.ts
//
// (also wired as part of `npm test`.)
//
// The exhaustiveness of the code table is enforced by tsc (BASE is a
// Record<KeyRotationError['code'], string>); this test covers what tsc cannot:
// that every suffix the table can return has copy in en.json, and the
// per-screen choices.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  allLoginErrorSuffixes,
  allRotationErrorSuffixes,
  classifyLoginError,
  loginErrorSuffix,
  requiresSignOut,
  rotationErrorMessage,
} from './rotationErrors.ts'

const here = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(here, '..', 'i18n', 'en.json'), 'utf8')) as {
  auth: { rotationErrors: Record<string, string>; loginErrors: Record<string, string> }
}
const copy = en.auth.rotationErrors
const loginCopy = en.auth.loginErrors

// Every suffix has a string (plural keys count as present via _one/_other).
for (const suffix of allRotationErrorSuffixes()) {
  const present =
    typeof copy[suffix] === 'string' ||
    (typeof copy[`${suffix}_one`] === 'string' && typeof copy[`${suffix}_other`] === 'string')
  assert.ok(present, `en.json auth.rotationErrors.${suffix} is missing`)
}
// ...and no string is unreachable from the table.
const reachable = new Set<string>()
for (const suffix of allRotationErrorSuffixes()) {
  reachable.add(suffix)
  reachable.add(`${suffix}_one`)
  reachable.add(`${suffix}_other`)
}
for (const key of Object.keys(copy)) {
  assert.ok(reachable.has(key), `en.json auth.rotationErrors.${key} is not returned by rotationErrors.ts`)
}

// Params travel only for the two codes that carry numbers.
assert.deepEqual(rotationErrorMessage({ code: 'active_runs', count: 2 }, 'changePassword'), {
  suffix: 'activeRuns',
  params: { count: 2 },
})
assert.deepEqual(
  rotationErrorMessage({ code: 'insufficient_space', needed_mb: 900, available_mb: 120 }, 'restore'),
  { suffix: 'insufficientSpace', params: { needed: 900, available: 120 } },
)
assert.equal(rotationErrorMessage({ code: 'busy' }, 'newPhrase').params, undefined)

// The required rollback wording, and its per-screen variants.
assert.equal(copy.rolledBack, 'Nothing was changed. Your old password still works.')
assert.equal(
  copy.rolledBackNewPhrase,
  'Nothing was changed. Your old password and recovery phrase still work.',
)
assert.equal(rotationErrorMessage({ code: 'rolled_back' }, 'changePassword').suffix, 'rolledBack')
assert.equal(rotationErrorMessage({ code: 'rolled_back' }, 'cancel').suffix, 'rolledBack')
assert.equal(rotationErrorMessage({ code: 'rolled_back' }, 'newPhrase').suffix, 'rolledBackNewPhrase')
assert.equal(rotationErrorMessage({ code: 'rolled_back' }, 'restore').suffix, 'rolledBackRestore')

// Wrong credential wording differs by screen; the cancel screen never reveals
// whether the OLD password was typed (it is just wrong there).
assert.equal(rotationErrorMessage({ code: 'invalid_credentials' }, 'changePassword').suffix, 'currentPasswordWrong')
assert.equal(rotationErrorMessage({ code: 'invalid_credentials' }, 'newPhrase').suffix, 'currentPasswordWrong')
assert.equal(rotationErrorMessage({ code: 'invalid_credentials' }, 'restore').suffix, 'restoreInvalidCredentials')
assert.equal(rotationErrorMessage({ code: 'invalid_credentials' }, 'cancel').suffix, 'cancelInvalidCredentials')
assert.equal(rotationErrorMessage({ code: 'invalid_credentials' }, 'login').suffix, 'invalidCredentials')
assert.ok(!/old password/i.test(copy.cancelInvalidCredentials ?? ''))

// account_locked makes no promise about how long.
assert.ok(!/\d|minute|hour|second|day/i.test(copy.accountLocked ?? ''), 'no duration promised')
// The four called out cases name their cause.
assert.match(copy.activeRuns_one ?? '', /task/)
assert.match(copy.insufficientSpace ?? '', /disk space/)
assert.match(copy.busy ?? '', /chat reply/)
assert.match(copy.unrecognisedFiles ?? '', /recognise/)
assert.match(copy.rotationAlreadyComplete ?? '', /new password or your recovery phrase/)

// Which codes mean "leave the session".
assert.equal(requiresSignOut('needs_resume'), true)
assert.equal(requiresSignOut('epoch_changed'), true)
assert.equal(requiresSignOut('not_logged_in'), true)
assert.equal(requiresSignOut('rolled_back'), false)
assert.equal(requiresSignOut('invalid_credentials'), false)

// --- login() strings ----------------------------------------------------------
assert.equal(classifyLoginError('rotation_interrupted'), 'interrupted')
assert.equal(classifyLoginError('rotation_rolled_back'), 'rolledBack')
assert.equal(classifyLoginError('rotation_resume_failed'), 'resumeFailed')
assert.equal(classifyLoginError('account locked, try again later'), 'locked')
assert.equal(classifyLoginError('invalid credentials'), 'invalidCredentials')
assert.equal(
  classifyLoginError("This account was created before the data-key format and can't be opened"),
  'legacyAccount',
)
assert.equal(classifyLoginError("couldn't verify credentials: unsupported key wrapper format 1"), 'other')
assert.equal(classifyLoginError('internal error: account exists with no salt record'), 'other')
assert.equal(classifyLoginError(''), 'other')
assert.equal(classifyLoginError('Rotation_Interrupted'), 'other', 'exact match only')
assert.equal(classifyLoginError('toString'), 'other', 'inherited object keys are not codes')
assert.equal(classifyLoginError('__proto__'), 'other')

assert.equal(loginErrorSuffix('interrupted'), null, 'opens its own screen')
assert.equal(loginErrorSuffix('other'), null, 'shows the raw message')
for (const suffix of allLoginErrorSuffixes()) {
  assert.equal(typeof loginCopy[suffix], 'string', `en.json auth.loginErrors.${suffix} is missing`)
}
for (const key of Object.keys(loginCopy)) {
  assert.ok(allLoginErrorSuffixes().includes(key), `en.json auth.loginErrors.${key} is not returned by rotationErrors.ts`)
}
assert.equal(loginCopy.rolledBack, 'Nothing was changed. Your old password still works.')
assert.ok(!/\d|minute|hour|second|day/i.test(loginCopy.accountLocked ?? ''), 'no duration promised')

console.log('rotationErrors.test.ts: all assertions passed')
