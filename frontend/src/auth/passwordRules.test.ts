// Plain-assertion test for passwordRules.ts (items.id=777):
//
//   node --experimental-strip-types src/auth/passwordRules.test.ts
//
// (also wired as part of `npm test`.) Every password here is fake. The cases
// mirror the Rust tests in src-tauri/src/auth/password_policy.rs.
import assert from 'node:assert/strict'
import {
  MIN_PASSWORD_LENGTH,
  passwordCount,
  passwordLength,
  validateNewPassword,
} from './passwordRules.ts'

assert.equal(MIN_PASSWORD_LENGTH, 15)

// Length: code points of the NFKC form.
assert.equal(passwordLength(''), 0)
assert.equal(passwordLength('abc'), 3)
assert.equal(passwordLength('café'), 4)
assert.equal(passwordLength('café'), 4, 'decomposed accent is one character')
assert.equal(passwordLength('日本語漢字'), 5, 'not bytes')
assert.equal('\u{1f600}'.length, 2, 'sanity: JS length counts an emoji as 2')
assert.equal(passwordLength('\u{1f600}'), 1, 'not UTF-16 units')
assert.equal(passwordLength('ﬃ'), 3, 'a ligature folds to three letters')
assert.equal(passwordLength('１２'), 2)

// The boundary.
assert.equal(validateNewPassword('', ''), 'empty')
assert.equal(validateNewPassword('', 'x'), 'empty', 'empty wins over mismatch')
const fourteen = 'fourteen chars'
const fifteen = 'fifteen chars!!'
assert.equal(passwordLength(fourteen), 14)
assert.equal(passwordLength(fifteen), 15)
assert.equal(validateNewPassword(fourteen, fourteen), 'too_short')
assert.equal(validateNewPassword(fourteen, ''), 'too_short', 'too short wins over mismatch')
assert.equal(validateNewPassword(fifteen, fifteen), null)
assert.equal(validateNewPassword(fifteen, 'something else!!'), 'mismatch')
assert.equal(validateNewPassword(fifteen, ''), 'mismatch')

// 15 emoji pass (60 bytes, 30 UTF-16 units, 15 characters); 8 CJK do not.
assert.equal(validateNewPassword('\u{1f600}'.repeat(15), '\u{1f600}'.repeat(15)), null)
const cjk = '日本語漢字人民水'
assert.equal(validateNewPassword(cjk, cjk), 'too_short')
// Five "ffi" ligatures fold to 15 letters; fourteen decomposed accents are 14.
assert.equal(validateNewPassword('ﬃ'.repeat(5), 'ﬃ'.repeat(5)), null)
assert.equal(validateNewPassword('é'.repeat(14), 'é'.repeat(14)), 'too_short')

// Spaces and passphrases are fine, and nothing is trimmed.
assert.equal(validateNewPassword('gentle river stone lantern', 'gentle river stone lantern'), null)
assert.equal(validateNewPassword('   fifteen spaces!   ', '   fifteen spaces!   '), null)
assert.equal(validateNewPassword(' fifteen chars!!', 'fifteen chars!!'), 'mismatch', 'a leading space is part of it')
assert.equal(validateNewPassword('x'.repeat(5000), 'x'.repeat(5000)), null, 'no upper cap')

// Case matters; normalisation does not make two typings differ.
assert.equal(validateNewPassword('Fifteen chars!!', 'fifteen chars!!'), 'mismatch')
assert.equal(
  validateNewPassword('café au lait okay', 'café au lait okay'),
  null,
  'NFC and NFD of the same text are the same password',
)

// The live count.
assert.deepEqual(passwordCount(''), { count: 0, min: 15, ok: false })
assert.deepEqual(passwordCount(fourteen), { count: 14, min: 15, ok: false })
assert.deepEqual(passwordCount(fifteen), { count: 15, min: 15, ok: true })

console.log('passwordRules.test.ts: all assertions passed')
