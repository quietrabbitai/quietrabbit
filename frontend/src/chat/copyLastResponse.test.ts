// Plain-assertion test for copyLastResponse.ts (items.id=501 slice 2),
// matching lateReplyRecovery.test.ts's convention: run directly on Node's
// built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/chat/copyLastResponse.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { findLastCopyableReply } from './copyLastResponse.ts'
import type { MessageInfo } from '../bindings.ts'

function msg(overrides: Partial<MessageInfo>): MessageInfo {
  return {
    id: crypto.randomUUID(),
    context_key: 'chat-1',
    sender: 'user',
    content: '',
    focus_run_id: null,
    created_at: '2026-10-04T00:00:00Z',
    is_error: false,
    ...overrides,
  }
}

assert.equal(findLastCopyableReply([]), null, 'no messages -> nothing to copy')

assert.equal(
  findLastCopyableReply([msg({ sender: 'user', content: 'hi' })]),
  null,
  'no assistant row -> nothing to copy',
)

const first = msg({ sender: 'assistant', content: 'first reply' })
const newest = msg({ sender: 'assistant', content: 'newest reply' })
assert.equal(
  findLastCopyableReply([msg({ content: 'q1' }), first, msg({ content: 'q2' }), newest]),
  newest,
  'newest assistant reply wins',
)

// A trailing user message does not hide the newest assistant reply.
const answered = msg({ sender: 'assistant', content: 'ok' })
assert.equal(
  findLastCopyableReply([answered, msg({ content: 'a follow-up not yet answered' })]),
  answered,
  'the newest ASSISTANT row is what counts, not the newest row',
)

// Newest assistant row is an is_error row: disabled, NO fallback to the
// earlier good reply.
assert.equal(
  findLastCopyableReply([
    msg({ sender: 'assistant', content: 'a good earlier reply' }),
    msg({ content: 'retry' }),
    msg({ sender: 'assistant', content: 'Something went wrong.', is_error: true }),
  ]),
  null,
  'an is_error newest reply is not copyable and must not fall back',
)

// Newest assistant row is an empty placeholder (still generating / not yet
// backfilled): same.
assert.equal(
  findLastCopyableReply([
    msg({ sender: 'assistant', content: 'a good earlier reply' }),
    msg({ content: 'next question' }),
    msg({ sender: 'assistant', content: '' }),
  ]),
  null,
  'an empty newest reply is not copyable and must not fall back',
)
assert.equal(
  findLastCopyableReply([msg({ sender: 'assistant', content: '   \n' })]),
  null,
  'whitespace-only content counts as empty',
)

console.log('copyLastResponse.test.ts: all assertions passed')
