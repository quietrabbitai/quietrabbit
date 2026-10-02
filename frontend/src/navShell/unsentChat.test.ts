// Plain-assertion test for unsentChat.ts (items.id=584), matching
// frictionGateDetail.test.ts's convention: run directly on Node's built-in
// TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/unsentChat.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  createUnsentChat,
  nextHasUnseenReply,
  returnStep,
  shouldStartFreshOnReturn,
  type ReturnStepState,
} from './unsentChat.ts'

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

// items.id=584 follow-up 3 (decisions.id=843): a reply the user hasn't seen
// yet blocks the fresh reset on return.
assert.equal(
  shouldStartFreshOnReturn(true, false, true),
  false,
  'unseen reply keeps the chat instead of resetting',
)
assert.equal(
  shouldStartFreshOnReturn(true, false, false),
  true,
  'no unseen reply still resets',
)

// nextHasUnseenReply: the small state machine behind the flag above.
assert.equal(
  nextHasUnseenReply(false, false, true, false, true),
  true,
  'leaving while generating marks unseen',
)
assert.equal(
  nextHasUnseenReply(false, true, true, true, false),
  true,
  'generating finishes while away marks unseen',
)
assert.equal(
  nextHasUnseenReply(true, true, false, false, false),
  false,
  'returning clears the flag regardless of its prior value',
)
assert.equal(
  nextHasUnseenReply(false, false, true, false, false),
  false,
  'leaving while not generating leaves the flag alone',
)
assert.equal(
  nextHasUnseenReply(false, false, false, false, false),
  false,
  'no floor/generating transition is a no-op',
)

// returnStep: the single folded function CloudChatAccessPane's effect
// calls -- decision read BEFORE the flag update, in one place.
const initialReturnState: ReturnStepState = {
  floor: false,
  isGenerating: false,
  personaId: 'persona-p',
  hasUnseenReply: false,
}

{
  // Scenario 1: a reply is still generating when the user leaves -> keep.
  const leave = returnStep(initialReturnState, {
    floor: true,
    isGenerating: true,
    personaId: 'persona-p',
  })
  assert.equal(leave.startFresh, false, 'leaving never resets')
  assert.equal(leave.next.hasUnseenReply, true, 'leave-while-generating arms the flag')

  const back = returnStep(leave.next, { floor: false, isGenerating: true, personaId: 'persona-p' })
  assert.equal(back.startFresh, false, 'return keeps the chat: reply was generating at leave')
}

{
  // Scenario 2: nothing generating at leave, but the reply finishes while away -> keep.
  const leave = returnStep(initialReturnState, {
    floor: true,
    isGenerating: false,
    personaId: 'persona-p',
  })
  assert.equal(leave.next.hasUnseenReply, false, 'not generating at leave: flag stays clear')

  const finishesWhileAway = returnStep(leave.next, {
    floor: true,
    isGenerating: true,
    personaId: 'persona-p',
  })
  const completes = returnStep(finishesWhileAway.next, {
    floor: true,
    isGenerating: false,
    personaId: 'persona-p',
  })
  assert.equal(completes.next.hasUnseenReply, true, 'generating finishes while away arms the flag')

  const back = returnStep(completes.next, { floor: false, isGenerating: false, personaId: 'persona-p' })
  assert.equal(back.startFresh, false, 'return keeps the chat: reply finished while away')
}

{
  // Scenario 3: nothing generating, nothing happens while away -> fresh.
  const leave = returnStep(initialReturnState, {
    floor: true,
    isGenerating: false,
    personaId: 'persona-p',
  })
  const back = returnStep(leave.next, { floor: false, isGenerating: false, personaId: 'persona-p' })
  assert.equal(back.startFresh, true, 'return with no unseen reply resets')
}

{
  // Scenario 4 (Jason, personaId changing while away): a stale unseen flag
  // from the OLD persona must not block the fresh chat for the NEW one, and
  // the orphaned run's detached onGenerating(false) -- arriving a tick after
  // the persona actually changed -- must not be misread as "the new
  // persona's reply just finished while away."
  const leave = returnStep(initialReturnState, {
    floor: true,
    isGenerating: true,
    personaId: 'persona-p',
  })
  assert.equal(leave.next.hasUnseenReply, true, 'leave-while-generating arms the flag (persona-p)')

  // The persona switch itself: floor stays true, isGenerating is still
  // stale-true (ChatPane hasn't reported the detach yet).
  const personaSwitch = returnStep(leave.next, {
    floor: true,
    isGenerating: true,
    personaId: 'persona-q',
  })
  assert.equal(
    personaSwitch.next.hasUnseenReply,
    false,
    'switching persona clears a flag that belonged to the old persona',
  )

  // A tick later: the orphaned old run's detach arrives as isGenerating
  // going true -> false, floor/personaId unchanged.
  const detachArrives = returnStep(personaSwitch.next, {
    floor: true,
    isGenerating: false,
    personaId: 'persona-q',
  })
  assert.equal(
    detachArrives.next.hasUnseenReply,
    false,
    'the orphaned run detaching is not the new persona\'s reply finishing',
  )

  const back = returnStep(detachArrives.next, { floor: false, isGenerating: false, personaId: 'persona-q' })
  assert.equal(back.startFresh, true, 'return starts fresh for the new persona, not a stale keep')

  // Without forcing next.isGenerating to false on a persona change, the
  // detach tick would read prevIsGenerating: true and wrongly re-arm the
  // flag -- confirm that failure mode directly against the un-forced value.
  const unforcedPersonaSwitchIsGenerating = true // what inputs.isGenerating was, unforced
  const wronglyArmed = nextHasUnseenReply(
    false,
    true,
    true,
    unforcedPersonaSwitchIsGenerating,
    false,
  )
  assert.equal(
    wronglyArmed,
    true,
    'sanity check: without the fix this transition would wrongly re-arm the flag',
  )
}

console.log('unsentChat.test.ts: all assertions passed')
