// The four backend calls the step review host makes (items.id=884 stage 2), kept
// apart from the hook so each is one obvious call. Every one answers with a value,
// never throws: a rejected invoke becomes an error result.
import { commands, type PendingStepReview } from '../bindings'
import type { ElementDecision } from '../cloudChatAccess/consentDecisions'

export type Pulled =
  | { status: 'ok'; review: PendingStepReview | null }
  | { status: 'error'; error: string }

export type Done = { status: 'ok' } | { status: 'error'; error: string }

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

/** The review a run is parked on, if any (get_pending_step_review). */
export async function pullPendingReview(personaId: string, runId: string): Promise<Pulled> {
  try {
    const r = await commands.getPendingStepReview({ persona_id: personaId, run_id: runId })
    return r.status === 'ok' ? { status: 'ok', review: r.data } : { status: 'error', error: r.error }
  } catch (e) {
    return { status: 'error', error: message(e) }
  }
}

/** resume_run: asks the run to carry on from the step it parked on. */
export async function resumeParkedRun(
  userId: string,
  personaId: string,
  runId: string,
): Promise<Done> {
  try {
    const r = await commands.resumeRun({ run_id: runId, user_id: userId, persona_id: personaId })
    return r.status === 'ok' ? { status: 'ok' } : { status: 'error', error: r.error }
  } catch (e) {
    return { status: 'error', error: message(e) }
  }
}

/** The user's answers, recorded exactly as the modal built them
 *  (submit_element_consent_decision), then the run resumed. The server applies the
 *  recorded answers to the text it renders: nothing here supplies that text. */
export async function sendAnswers(
  userId: string,
  personaId: string,
  runId: string,
  decisions: ElementDecision[],
): Promise<Done> {
  try {
    const recorded = await commands.submitElementConsentDecision({
      run_id: runId,
      user_id: userId,
      persona_id: personaId,
      decisions_json: JSON.stringify(decisions),
    })
    if (recorded.status !== 'ok') return { status: 'error', error: recorded.error }
  } catch (e) {
    return { status: 'error', error: message(e) }
  }
  return resumeParkedRun(userId, personaId, runId)
}

/** decline_step_review: refuses the pending review and the backend cancels the run.
 *  `ok` with `false` means there was no pending review to refuse (it ended on its
 *  own), which is as good as done. */
export async function declinePendingReview(personaId: string, runId: string): Promise<Done> {
  try {
    const r = await commands.declineStepReview({ persona_id: personaId, run_id: runId })
    return r.status === 'ok' ? { status: 'ok' } : { status: 'error', error: r.error }
  } catch (e) {
    return { status: 'error', error: message(e) }
  }
}
