// Pure selection logic for the "Copy last response" button (items.id=501
// slice 2, decisions.id=846 addendum Q3). A plain .ts module (no JSX) so its
// test runs directly on Node's type-stripping, same convention as
// lateReplyRecovery.ts.
import type { MessageInfo } from '../bindings.ts'

/** The reply the button copies: the NEWEST assistant row, and only if it is
 *  a real reply -- non-empty content and not an is_error row. If the newest
 *  assistant row is an error or still-empty placeholder the answer is null
 *  (button disabled); it deliberately does NOT fall back to an earlier good
 *  reply, which would copy something other than "the last response" the
 *  user is looking at. */
export function findLastCopyableReply(messages: MessageInfo[]): MessageInfo | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m.sender !== 'assistant') continue
    if (m.is_error || m.content.trim() === '') return null
    return m
  }
  return null
}
