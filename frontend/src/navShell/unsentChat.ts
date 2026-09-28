// items.id=584: client-side placeholders for a not-yet-persisted chat, plus the
// "returning to Chat starts fresh" trigger. Extracted out of
// CloudChatAccessPane.tsx so they can be tested React-free on Node's built-in
// TypeScript support (same precedent as keepActiveChatForPersona.ts).

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { ChatInfo } from '../bindings.ts'

/** items.id=546: a fresh "chat-{uuid}" context_key held locally. No `chats`
 *  row exists until the first send under that key (chat_store::
 *  ensure_chat_and_bump_activity). */
export function createUnsentChat(personaId: string): ChatInfo {
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

/** True on the transition from the pane's collapsed floor (Board/Library is
 *  the dominant rail) back to Chat being visible. Leaving Chat and returning
 *  starts a fresh chat rather than resuming whatever was open, e.g. a past
 *  chat opened from History. */
export function shouldStartFreshOnReturn(prevFloor: boolean, floor: boolean): boolean {
  return prevFloor && !floor
}
