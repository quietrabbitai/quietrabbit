// Plain-assertion test for sessionLock.ts (items.id=775), same convention as
// the other *.test.ts files:
//
//   node --experimental-strip-types src/auth/sessionLock.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  allLockNoticeSuffixes,
  isSessionLockNotice,
  lockNoticeSuffix,
  nextBootState,
  noticeAfterLock,
  parseSessionLocked,
  recheckAfterListen,
  type BootState,
} from './sessionLock.ts'

const here = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(here, '..', 'i18n', 'en.json'), 'utf8')) as {
  auth: { sessionLocked: Record<string, string> }
}

// -- nextBootState: a lock always lands on the login screen ---------------
const states: BootState[] = ['checking', 'loginRequired', 'loggedIn']
for (const state of states) {
  for (const reason of [
    'idle',
    'session_expired',
    'key_epoch_changed',
    'logged_out',
    'password_changed',
    'recovery_phrase_changed',
    'change_unfinished',
    'brand_new',
  ]) {
    assert.equal(
      nextBootState(state, { type: 'session-locked', reason }),
      'loginRequired',
      `${state} + session-locked(${reason})`,
    )
  }
  assert.equal(nextBootState(state, { type: 'signed-out' }), 'loginRequired')
  assert.equal(nextBootState(state, { type: 'login-succeeded' }), 'loggedIn')
}

// The boot answer decides only while still checking.
assert.equal(nextBootState('checking', { type: 'boot-session', present: true }), 'loggedIn')
assert.equal(nextBootState('checking', { type: 'boot-session', present: false }), 'loginRequired')
assert.equal(nextBootState('loginRequired', { type: 'boot-session', present: true }), 'loginRequired')
assert.equal(nextBootState('loggedIn', { type: 'boot-session', present: false }), 'loggedIn')

// -- parseSessionLocked ---------------------------------------------------
assert.equal(parseSessionLocked({ reason: 'idle' }), 'idle')
assert.equal(parseSessionLocked({ reason: 'some_future_reason' }), 'some_future_reason')
for (const bad of [null, undefined, 'idle', 3, {}, { reason: 3 }, { reason: '' }, []]) {
  assert.equal(parseSessionLocked(bad), null, JSON.stringify(bad))
}

// -- the re-check after the listener attaches -----------------------------
assert.deepEqual(recheckAfterListen('loggedIn', 'absent'), {
  type: 'session-locked',
  reason: 'no_session',
})
assert.equal(recheckAfterListen('loggedIn', 'present'), null)
assert.equal(recheckAfterListen('loggedIn', null), null, 'a failed check proves nothing')
assert.equal(recheckAfterListen('checking', 'absent'), null, 'the boot check decides while checking')
assert.equal(recheckAfterListen('loginRequired', 'absent'), null)

// -- notices --------------------------------------------------------------
const rotationNotice = { error: { code: 'needs_resume' as const }, context: 'changePassword' as const }
assert.deepEqual(noticeAfterLock(null, 'idle'), { lockReason: 'idle' })
assert.deepEqual(noticeAfterLock(rotationNotice, 'idle'), { lockReason: 'idle' })
assert.equal(noticeAfterLock(rotationNotice, 'logged_out'), rotationNotice, 'logout keeps its own notice')
assert.equal(noticeAfterLock(null, 'logged_out'), null)
assert.equal(isSessionLockNotice({ lockReason: 'idle' }), true)
assert.equal(isSessionLockNotice(rotationNotice), false)

// -- copy -----------------------------------------------------------------
assert.equal(lockNoticeSuffix('idle'), 'idle')
assert.equal(lockNoticeSuffix('session_expired'), 'expired')
assert.equal(lockNoticeSuffix('key_epoch_changed'), 'keyEpochChanged')
// items.id=778: every rotation ends the session with its own line.
assert.equal(lockNoticeSuffix('password_changed'), 'passwordChanged')
assert.equal(lockNoticeSuffix('recovery_phrase_changed'), 'recoveryPhraseChanged')
assert.equal(lockNoticeSuffix('change_unfinished'), 'changeUnfinished')
assert.equal(
  en.auth.sessionLocked.passwordChanged,
  'Your password was changed. Sign in with your new password.',
)
assert.equal(
  en.auth.sessionLocked.recoveryPhraseChanged,
  'Your recovery phrase was changed. Sign in again with your password.',
)
for (const reason of ['no_session', 'check_failed', 'something_new', 'toString', '__proto__']) {
  assert.equal(lockNoticeSuffix(reason), 'generic', reason)
}
const copy = en.auth.sessionLocked
for (const suffix of allLockNoticeSuffixes()) {
  assert.equal(typeof copy[suffix], 'string', `en.json auth.sessionLocked.${suffix} is missing`)
}
for (const key of Object.keys(copy)) {
  assert.ok(allLockNoticeSuffixes().includes(key), `en.json auth.sessionLocked.${key} is unreachable`)
}
console.log('sessionLock.test.ts ok')
