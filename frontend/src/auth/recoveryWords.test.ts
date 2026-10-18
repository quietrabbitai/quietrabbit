// Plain-assertion test for recoveryWords.ts (items.id=769 stage 3):
//
//   node --experimental-strip-types src/auth/recoveryWords.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  PHRASE_WORD_COUNT,
  answersComplete,
  buildAnswers,
  joinWords,
  looksLikePhrase,
  normaliseWord,
  splitWords,
} from './recoveryWords.ts'

// Twenty-four letters-only stand-ins: worda .. wordx.
const words24 = Array.from({ length: PHRASE_WORD_COUNT }, (_, i) => `word${String.fromCharCode(97 + i)}`)

// Any whitespace separates words, including a CRLF paste.
assert.deepEqual(splitWords('abandon  ability\r\nable\tabout'), ['abandon', 'ability', 'able', 'about'])
assert.deepEqual(splitWords('  Abandon \n'), ['abandon'], 'trimmed and lowercased')
assert.deepEqual(splitWords('abandon ability'), ['abandon', 'ability'], 'non-breaking space')
assert.deepEqual(splitWords('abandon\r\n\r\nability\r\n'), ['abandon', 'ability'], 'blank lines and trailing CRLF')
assert.deepEqual(splitWords(''), [])
assert.deepEqual(splitWords(' \r\n '), [])

assert.equal(normaliseWord('  Zoo\r\n'), 'zoo')

// Phrase shape.
assert.equal(words24.length, 24)
assert.equal(looksLikePhrase(words24.join(' ')), true)
assert.equal(looksLikePhrase(words24.join('\r\n')), true, 'one word per line, Windows line endings')
assert.equal(looksLikePhrase(words24.slice(1).join(' ')), false, '23 words')
assert.equal(looksLikePhrase(`${words24.join(' ')} extra`), false, '25 words')
assert.equal(looksLikePhrase([...words24.slice(1), 'w0rd'].join(' ')), false, 'digits')
assert.equal(looksLikePhrase(words24.map((w) => w.toUpperCase()).join(' ')), true, 'case is folded')
assert.equal(joinWords(' A\r\nB   c '), 'a b c')

// Confirmation answers.
const positions = [3, 11, 20]
assert.equal(answersComplete(positions, { 3: 'a', 11: 'b', 20: 'c' }), true)
assert.equal(answersComplete(positions, { 3: 'a', 11: ' ', 20: 'c' }), false, 'blank counts as missing')
assert.equal(answersComplete(positions, { 3: 'a', 20: 'c' }), false)
assert.equal(answersComplete([], {}), false, 'nothing asked is not complete')
assert.deepEqual(buildAnswers(positions, { 20: ' C ', 3: 'A', 11: 'b' }), [
  { position: 3, word: 'a' },
  { position: 11, word: 'b' },
  { position: 20, word: 'c' },
])
assert.deepEqual(buildAnswers([5], {}), [{ position: 5, word: '' }])

console.log('recoveryWords.test.ts: all assertions passed')
