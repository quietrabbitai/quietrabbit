// items.id=587 (late-reply recovery): pure logic extracted out of
// ChatPane.tsx so it can be tested React-free on Node's built-in
// TypeScript support (same precedent as navShell/unsentChat.ts).
//
// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type { MessageInfo } from '../bindings.ts'

/** A fresh mount (e.g. reopening a chat from History) has no live
 *  activeRunId of its own -- this is how it notices a run might still be
 *  in flight for this context_key: the signature a not-yet-backfilled
 *  placeholder leaves behind. Every run that has actually finished,
 *  success or failure, now backfills *something* into content (see
 *  finalize_chat_reply, commands/messages.rs) -- empty content with a
 *  focus_run_id set is specifically "nothing has been written back yet."
 *  Returns the most recent such row, or null if none exists. */
export function findInFlightAssistantMessage(
  messages: MessageInfo[],
): MessageInfo | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m.sender === 'assistant' && m.content === '' && m.focus_run_id) {
      return m
    }
  }
  return null
}

export type RunStatusClass = 'active' | 'dead' | 'inactive'

/** Classifies a focus_runs.status string (commands.getRunStatus's return
 *  value) for the late-reply-recovery decision:
 *  - 'active': still genuinely being worked on -- resume/keep polling.
 *  - 'dead': failed or cancelled -- stop, and the UI may say so plainly
 *    ("That reply didn't finish.").
 *  - 'inactive': anything else -- paused, awaiting_user/feedback/
 *    extract_confirm/schedule, complete, or an unrecognized/null status
 *    (including a run_id the backend doesn't know about at all, Ok(None)
 *    from get_run_status). These are NOT this component's concern: a
 *    consent/Gate3 pause has its own UI elsewhere, and this function's
 *    only job is to keep ChatPane from showing a live "Generating..."
 *    indicator for them, not to explain or resolve them. Stop
 *    tracking/polling, same as 'dead', but say nothing. */
export function classifyRunStatus(status: string | null): RunStatusClass {
  if (status === 'running' || status === 'initializing') return 'active'
  if (status === 'failed' || status === 'cancelled') return 'dead'
  return 'inactive'
}

/** Which plain notice stands in for an assistant reply with no text (items.id=817).
 *  - 'stopped': the reply was cancelled before any text arrived -- a user's
 *    cancel of a run waiting to retry (the run ends cancelled, the empty
 *    reply row is marked cancelled and its chat is filed in History), or a
 *    rotation's 'Stop and save'. Read from the saved row, so it also holds
 *    after the transcript is re-fetched or reopened from History.
 *  - 'didNotFinish': the run was confirmed dead (failed or cancelled) while
 *    this pane tracked it (deadRunIds), with the placeholder still empty.
 *  - null: show the message as it is (it has text, or it is still in flight). */
export type EmptyReplyNotice = 'stopped' | 'didNotFinish'

export function emptyReplyNotice(
  m: MessageInfo,
  deadRunIds: ReadonlySet<string>,
): EmptyReplyNotice | null {
  if (m.sender !== 'assistant' || m.content !== '') return null
  if (m.reply_status === 'cancelled') return 'stopped'
  if (m.focus_run_id && deadRunIds.has(m.focus_run_id)) return 'didNotFinish'
  return null
}
