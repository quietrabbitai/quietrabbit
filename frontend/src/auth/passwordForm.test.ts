// Plain-assertion test for passwordForm.ts (items.id=769 stage 2):
//
//   node --experimental-strip-types src/auth/passwordForm.test.ts
//
// (also wired as part of `npm test`.) The new-password rules are tested in
// passwordRules.test.ts.
import assert from 'node:assert/strict'
import { canSubmitPasswordChange } from './passwordForm.ts'

assert.equal(canSubmitPasswordChange('old', 'new', 'new'), true)
assert.equal(canSubmitPasswordChange('', 'new', 'new'), false)
assert.equal(canSubmitPasswordChange('old', '', 'new'), false)
assert.equal(canSubmitPasswordChange('old', 'new', ''), false)

console.log('passwordForm.test.ts: all assertions passed')
