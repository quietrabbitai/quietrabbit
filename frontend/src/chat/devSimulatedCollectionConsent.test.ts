// Plain-assertion test for devSimulatedCollectionConsent.ts (items.id=920 stage 3).
//
//   node --experimental-strip-types src/chat/devSimulatedCollectionConsent.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  SIMULATED_MODES,
  SimulatedCollectionConsent,
  devSimulatedCollectionConsent,
  initialSimulatedState,
} from './devSimulatedCollectionConsent.ts'

// Guard: under Node there is no Vite env, so the guarded entry point is inert,
// the same inertness a production build gets.
assert.equal(devSimulatedCollectionConsent(), null)

// The six modes start where they say, and need_decision follows the same rule the
// backend has.
const expectAsked: Record<(typeof SIMULATED_MODES)[number], boolean> = {
  not_asked: true,
  stale_granted: true,
  stale_declined: true,
  declined: false,
  granted: false,
  withdrawn: false,
}
for (const mode of SIMULATED_MODES) {
  assert.equal(initialSimulatedState(mode).needs_decision, expectAsked[mode], mode)
}

// A stale version is rejected, like the real command.
{
  const sim = new SimulatedCollectionConsent('not_asked')
  const stale = sim.submit('granted', 7)
  assert.deepEqual(stale, { status: 'error', error: 'consent_text_stale' })
  assert.equal(sim.get().status === 'ok' && sim.get().status === 'ok' ? 'ok' : '', 'ok')
}

// Decline then grant, withdraw, and the chain of states the UI sees.
{
  const sim = new SimulatedCollectionConsent('not_asked')
  const declined = sim.submit('declined', 1)
  assert.ok(declined.status === 'ok')
  assert.equal(declined.data.consent, 'declined')
  assert.equal(declined.data.needs_decision, false, 'a declined user at the current text is not asked')
  const granted = sim.submit('granted', 1)
  assert.ok(granted.status === 'ok')
  assert.equal(granted.data.consent, 'granted')
  // Declining over a current grant is a withdrawal, as in the backend.
  assert.deepEqual(sim.submit('declined', 1), { status: 'error', error: 'consent_granted_use_withdraw' })
  const withdrawn = sim.withdraw()
  assert.ok(withdrawn.status === 'ok')
  assert.equal(withdrawn.data.consent, 'withdrawn')
  assert.equal(withdrawn.data.needs_decision, false, 'a withdrawn user is not asked again')
  assert.equal(withdrawn.data.stored_version, 0)
  // Withdrawing again is fine, and only a grant can be withdrawn at all.
  assert.ok(sim.withdraw().status === 'ok')
  assert.deepEqual(new SimulatedCollectionConsent('declined').withdraw(), {
    status: 'error',
    error: 'no_consent_to_withdraw',
  })
}

// A stale grant is asked again at the new text and the decision carries v2.
{
  const sim = new SimulatedCollectionConsent('stale_granted')
  const first = sim.get()
  assert.ok(first.status === 'ok')
  assert.equal(first.data.needs_decision, true)
  assert.equal(first.data.current_text_version, 2)
  assert.deepEqual(sim.submit('granted', 1), { status: 'error', error: 'consent_text_stale' })
  const ok = sim.submit('granted', 2)
  assert.ok(ok.status === 'ok')
  assert.equal(ok.data.needs_decision, false)
}

console.log('devSimulatedCollectionConsent.test.ts: all assertions passed')
