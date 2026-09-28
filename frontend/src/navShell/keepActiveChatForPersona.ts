// items.id=584: the "should activeChat survive this personaId change" decision,
// extracted out of CloudChatAccessPane.tsx's [personaId] reset effect so it can
// be tested React-free on Node's built-in TypeScript support (same precedent as
// frictionGateDetail.ts / cloudChatAccess/consentDecisions.ts).
//
// A chat belongs to exactly one Persona (decisions.id=741). handleStartNewChat
// sets a fresh chat for the NEW persona and calls onPersonaChange in the same
// tick (decisions.id=823: a persona switch starts a fresh chat), so by the time
// the reset effect sees the new personaId, activeChat already belongs to it and
// must be kept. A chat left over from a DIFFERENT persona (an external persona
// change, e.g. the Persona hub) is stale and must still be cleared.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { ChatInfo } from '../bindings.ts'

export function keepActiveChatForPersona(
  activeChat: ChatInfo | null,
  personaId: string | null,
): ChatInfo | null {
  return activeChat !== null && activeChat.persona_id === personaId ? activeChat : null
}
