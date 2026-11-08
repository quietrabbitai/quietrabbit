// Plain-assertion test for navPromptState.ts and the wiring that keeps the URL
// out of the frontend (items.id=873), same convention as popupCloseKey.test.ts:
//
//   node --experimental-strip-types src/cloudChatAccess/navPromptState.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { initialNavPromptState, reduceNavPrompt, type NavConfirm } from './navPromptState.ts'

const a: NavConfirm = { requestId: 'nav-1', providerId: 'p1', host: 'a.example.test' }
const b: NavConfirm = { requestId: 'nav-2', providerId: 'p1', host: 'b.example.test' }
const other: NavConfirm = { requestId: 'nav-3', providerId: 'p2', host: 'c.example.test' }

// A confirm shows, and a newer one for the pane replaces it.
let s = reduceNavPrompt(initialNavPromptState, { type: 'confirm', confirm: a })
assert.deepEqual(s.confirm, a)
s = reduceNavPrompt(s, { type: 'confirm', confirm: b })
assert.deepEqual(s.confirm, b)

// A cancelled event for another pane leaves the open confirm alone; its own pane's dismisses it.
assert.equal(reduceNavPrompt(s, { type: 'confirm-cancelled', providerId: 'p2' }), s)
assert.equal(reduceNavPrompt(s, { type: 'confirm-cancelled', providerId: 'p1' }).confirm, null)

// An answer clears only the request it answers.
assert.equal(reduceNavPrompt(s, { type: 'answered', requestId: 'nav-1' }), s)
assert.equal(reduceNavPrompt(s, { type: 'answered', requestId: 'nav-2' }).confirm, null)
assert.deepEqual(reduceNavPrompt(initialNavPromptState, { type: 'confirm', confirm: other }).confirm, other)

// Notices are independent of the confirm and dismiss on their own.
s = reduceNavPrompt(s, { type: 'notice', notice: { providerId: 'p1', scheme: 'data' } })
assert.deepEqual(s.notice, { providerId: 'p1', scheme: 'data' })
assert.deepEqual(s.confirm, b)
s = reduceNavPrompt(s, { type: 'notice-dismissed' })
assert.equal(s.notice, null)
assert.equal(reduceNavPrompt(s, { type: 'notice-dismissed' }), s)

// Wiring (source scan): the prompts answer through the command with only the
// request id and the decision, hold the panes hidden while the confirm is open,
// detach every listener, and the host is rendered as text, never as markup.
const SRC = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const prompts = fs.readFileSync(path.join(SRC, 'cloudChatAccess/CloudChatNavPrompts.tsx'), 'utf8')
const pane = fs.readFileSync(path.join(SRC, 'navShell/CloudChatAccessPane.tsx'), 'utf8')
assert.match(prompts, /commands\.answerCloudChatNavConfirm\(requestId, openInBrowser\)/)
assert.match(prompts, /answer\(confirm\.requestId, true\)/)
assert.match(prompts, /answer\(confirm\.requestId, false\)/)
assert.match(prompts, /acquireBlockingOverlay\(\)/)
assert.match(prompts, /'cloud-chat-nav-confirm'/)
assert.match(prompts, /'cloud-chat-nav-confirm-cancelled'/)
assert.match(prompts, /'cloud-chat-nav-notice'/)
assert.match(prompts, /unlisten/i)
assert.doesNotMatch(prompts, /dangerouslySetInnerHTML/)
const promptsCode = prompts
  .split('\n')
  .filter((line) => !line.trim().startsWith('//'))
  .join('\n')
assert.doesNotMatch(promptsCode, /\burl\b/i, 'the frontend never holds a URL for the confirm')
assert.match(pane, /<CloudChatNavPrompts \/>/)

console.log('navPromptState: all assertions passed')
