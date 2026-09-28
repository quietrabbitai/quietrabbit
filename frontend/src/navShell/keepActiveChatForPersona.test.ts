// Plain-assertion test for keepActiveChatForPersona (items.id=584), matching
// frictionGateDetail.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/keepActiveChatForPersona.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import { keepActiveChatForPersona } from './keepActiveChatForPersona.ts'
import type { ChatInfo } from '../bindings.ts'

function freshChat(personaId: string): ChatInfo {
  const id = crypto.randomUUID()
  const now = new Date().toISOString()
  return {
    id,
    persona_id: personaId,
    context_key: `chat-${id}`,
    title: null,
    archived_at: null,
    created_at: now,
    last_message_at: now,
  }
}

// (a) Picking a persona from no active persona: handleStartNewChat's fresh
// chat for P arrives alongside personaId P and must be kept.
const forNewPersona = freshChat('persona-p')
assert.equal(keepActiveChatForPersona(forNewPersona, 'persona-p'), forNewPersona)
assert.ok(keepActiveChatForPersona(forNewPersona, 'persona-p')?.context_key.startsWith('chat-'))

// (b) Switching from persona A to B: the fresh chat is B's, personaId is now
// B -- kept, not swapped back to B's flat tier3-access-* thread.
const forB = freshChat('persona-b')
assert.equal(keepActiveChatForPersona(forB, 'persona-b'), forB)

// (c) An external personaId change (Persona hub) with a stale activeChat that
// still belongs to the OLD persona is cleared.
const staleForA = freshChat('persona-a')
assert.equal(keepActiveChatForPersona(staleForA, 'persona-b'), null)

// No chat stays no chat; a chat with no active persona is stale.
assert.equal(keepActiveChatForPersona(null, 'persona-a'), null)
assert.equal(keepActiveChatForPersona(forNewPersona, null), null)

console.log('keepActiveChatForPersona.test.ts: all assertions passed')
