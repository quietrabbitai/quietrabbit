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
 *  chat opened from History -- UNLESS there's a reply the user hasn't seen
 *  yet (decisions.id=843: "a chat in progress should not be filed to
 *  history since the user may never see the response"), tracked by
 *  hasUnseenReply (see nextHasUnseenReply below). Defaults to false so
 *  every existing 2-arg call site keeps its prior behavior. */
export function shouldStartFreshOnReturn(
  prevFloor: boolean,
  floor: boolean,
  hasUnseenReply = false,
): boolean {
  return prevFloor && !floor && !hasUnseenReply
}

/** Whether there is a reply the user may not have seen: a run that was
 *  still generating the moment they left Chat (floor false -> true), or one
 *  that finished while they were away (floor already true, generating true
 *  -> false). Cleared the moment they're back on Chat (floor true -> false)
 *  -- seen, or about to be watched live if still generating, either way
 *  they're looking at it now. */
export function nextHasUnseenReply(
  prevHasUnseenReply: boolean,
  prevFloor: boolean,
  floor: boolean,
  prevIsGenerating: boolean,
  isGenerating: boolean,
): boolean {
  if (!prevFloor && floor && isGenerating) return true
  if (floor && prevIsGenerating && !isGenerating) return true
  if (prevFloor && !floor) return false
  return prevHasUnseenReply
}

/** items.id=584 follow-up 3: the state shouldStartFreshOnReturn/
 *  nextHasUnseenReply need carried across renders -- held by the caller in
 *  a single ref, updated only via returnStep below. */
export interface ReturnStepState {
  floor: boolean
  isGenerating: boolean
  personaId: string | null
  hasUnseenReply: boolean
}

/** The one function CloudChatAccessPane's return-effect calls. Folds the
 *  reset decision and the hasUnseenReply update together so their ordering
 *  -- the decision must read the flag as it stood BEFORE this tick, then
 *  the flag gets updated for the next tick -- lives here, tested, rather
 *  than in the effect body. */
export function returnStep(
  state: ReturnStepState,
  inputs: { floor: boolean; isGenerating: boolean; personaId: string | null },
): { startFresh: boolean; next: ReturnStepState } {
  // A reply "unseen" for one Persona says nothing about another. If
  // personaId changed independently of this pane's own handleStartNewChat
  // (which already starts a fresh chat itself before this ever runs), a
  // carried-over flag is stale and must not block the fresh-chat-on-return
  // guarantee items.id=584 exists for.
  const personaChanged = inputs.personaId !== state.personaId
  const carriedHasUnseenReply = personaChanged ? false : state.hasUnseenReply

  const startFresh = shouldStartFreshOnReturn(state.floor, inputs.floor, carriedHasUnseenReply)

  const hasUnseenReply = nextHasUnseenReply(
    carriedHasUnseenReply,
    state.floor,
    inputs.floor,
    state.isGenerating,
    inputs.isGenerating,
  )

  return {
    startFresh,
    next: {
      floor: inputs.floor,
      // A persona change orphans the OLD chat's run inside ChatPane (its
      // contextKey effect calls setActiveRunId(null) once the new
      // contextKey lands), so the resulting onGenerating(false) for that
      // old run arrives a tick AFTER personaId actually changes, carrying
      // no information about the NEW persona's chat. Storing
      // inputs.isGenerating (still stale-true here) as prevIsGenerating
      // would make that later false-arrival read as "a reply just
      // finished while away" for the new persona and wrongly re-arm
      // hasUnseenReply. Forcing false here means that arrival is read as
      // false -> false: no transition.
      isGenerating: personaChanged ? false : inputs.isGenerating,
      personaId: inputs.personaId,
      hasUnseenReply,
    },
  }
}
