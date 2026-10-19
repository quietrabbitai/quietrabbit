import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  addFailure,
  allWriteFailedSuffixes,
  parseWriteFailed,
  writeFailedSuffix,
} from './writeFailed.ts'

assert.equal(parseWriteFailed({ kind: 'reply' }), 'reply')
for (const bad of [null, undefined, 'reply', 3, {}, { kind: 3 }, { kind: '' }, []]) {
  assert.equal(parseWriteFailed(bad), null, JSON.stringify(bad))
}
assert.equal(writeFailedSuffix('reply'), 'reply')
assert.equal(writeFailedSuffix('run'), 'run')
assert.equal(writeFailedSuffix('history'), 'history')
assert.equal(writeFailedSuffix('reply_status'), 'replyStatus')
assert.equal(writeFailedSuffix('personal_flag'), 'personalFlag')
assert.equal(writeFailedSuffix('ocr_text'), 'ocrText')
for (const kind of ['something_new', '', 'toString', '__proto__']) {
  assert.equal(writeFailedSuffix(kind), 'generic', kind)
}
assert.deepEqual(addFailure([], 'reply'), ['reply'])
assert.deepEqual(addFailure(['reply'], 'reply'), ['reply'], 'each kind once')
assert.deepEqual(addFailure(['reply'], 'run'), ['reply', 'run'])

const here = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(here, '..', 'i18n', 'en.json'), 'utf8')) as {
  auth: { writeFailed: Record<string, string> }
}
for (const suffix of allWriteFailedSuffixes()) {
  assert.equal(typeof en.auth.writeFailed[suffix], 'string', `auth.writeFailed.${suffix} is missing`)
}
console.log('writeFailed.test.ts: all assertions passed')
