// Plain-assertion test for unsentChat.ts (items.id=584), matching
// frictionGateDetail.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/unsentChat.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { createUnsentChat, shouldStartFreshOnReturn } from './unsentChat.ts'

// A placeholder chat belongs to the given persona and uses the chat-{id} key.
const chat = createUnsentChat('persona-p')
assert.equal(chat.persona_id, 'persona-p')
assert.equal(chat.context_key, `chat-${chat.id}`)
assert.equal(chat.title, null)
assert.equal(chat.archived_at, null)
assert.notEqual(createUnsentChat('persona-p').context_key, chat.context_key)

// Only the floor -> not-floor transition (returning to Chat from Board/
// Library) starts fresh.
assert.equal(shouldStartFreshOnReturn(true, false), true)
assert.equal(shouldStartFreshOnReturn(false, false), false, 'staying in Chat')
assert.equal(shouldStartFreshOnReturn(false, true), false, 'leaving Chat')
assert.equal(shouldStartFreshOnReturn(true, true), false, 'staying away')

console.log('unsentChat.test.ts: all assertions passed')
