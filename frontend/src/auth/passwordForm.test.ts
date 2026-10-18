// Plain-assertion test for passwordForm.ts (items.id=769 stage 2):
//
//   node --experimental-strip-types src/auth/passwordForm.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { canSubmitPasswordChange, newPasswordProblem } from './passwordForm.ts'

assert.equal(newPasswordProblem('', ''), 'empty')
assert.equal(newPasswordProblem('', 'x'), 'empty', 'empty wins over mismatch')
assert.equal(newPasswordProblem('abc', 'abd'), 'mismatch')
assert.equal(newPasswordProblem('abc', ''), 'mismatch')
assert.equal(newPasswordProblem('abc', 'abc'), null)

// Whitespace is part of a password: never trimmed, never folded.
assert.equal(newPasswordProblem(' abc', 'abc'), 'mismatch')
assert.equal(newPasswordProblem('abc ', 'abc'), 'mismatch')
assert.equal(newPasswordProblem('   ', '   '), null, 'a password of spaces is not empty')
assert.equal(newPasswordProblem('Abc', 'abc'), 'mismatch', 'case matters')

// No invented rules: a short or reused password is the backend's call.
assert.equal(newPasswordProblem('a', 'a'), null)

assert.equal(canSubmitPasswordChange('old', 'new', 'new'), true)
assert.equal(canSubmitPasswordChange('', 'new', 'new'), false)
assert.equal(canSubmitPasswordChange('old', '', 'new'), false)
assert.equal(canSubmitPasswordChange('old', 'new', ''), false)

console.log('passwordForm.test.ts: all assertions passed')
