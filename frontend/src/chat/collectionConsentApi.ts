// The three collection-consent calls the UI makes (items.id=920 stage 3), each
// answered by the real command or, in a dev build with the simulation switch on,
// by the in-memory fake (devSimulatedCollectionConsent.ts, which fakes the
// focus_settings row Tech Support does not have yet). The `import.meta.env.DEV`
// literal lets a production bundle drop the fake entirely.
import { commands, type CollectionConsentDecision, type CollectionConsentState } from '../bindings'
import { devSimulatedCollectionConsent } from './devSimulatedCollectionConsent'

export type ConsentResult =
  | { status: 'ok'; data: CollectionConsentState }
  | { status: 'error'; error: string }

export function readConsentState(personaId: string, focusId: string): Promise<ConsentResult> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedCollectionConsent()
    if (sim) return Promise.resolve(sim.get())
  }
  return commands.getCollectionConsentState(personaId, focusId)
}

/** `textVersion` is the version the user was SHOWN (the displayed version). */
export function sendConsentDecision(
  personaId: string,
  focusId: string,
  decision: CollectionConsentDecision,
  textVersion: number,
): Promise<ConsentResult> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedCollectionConsent()
    if (sim) return Promise.resolve(sim.submit(decision, textVersion))
  }
  return commands.submitCollectionConsentDecision(personaId, focusId, decision, textVersion)
}

export function sendConsentWithdrawal(personaId: string, focusId: string): Promise<ConsentResult> {
  if (import.meta.env.DEV) {
    const sim = devSimulatedCollectionConsent()
    if (sim) return Promise.resolve(sim.withdraw())
  }
  return commands.withdrawCollectionConsent(personaId, focusId)
}
