// Plain-assertion test for copyDecisions.ts (items.id=501 slice 2).
//
//   node --experimental-strip-types src/chat/copyDecisions.test.ts
//
// (also wired as part of `npm test`.)
import assert from 'node:assert/strict'
import {
  applyAllCopyDecisions,
  applyCopyDecisions,
  COPY_REMOVED_PLACEHOLDER,
  type AutoResolved,
} from './copyDecisions.ts'
import type { ElementDecision } from '../cloudChatAccess/consentDecisions.ts'

function dec(span_id: string, overrides: Partial<ElementDecision>): ElementDecision {
  return {
    span_id,
    decision: 'generalize',
    suggestion_text: null,
    user_modified_text: null,
    category: 'private_person',
    fact_key: null,
    save_for_persona: false,
    ...overrides,
  }
}

const text = 'Email Ana at ana@example.com today'
const nameStart = text.indexOf('Ana')
const emailStart = text.indexOf('ana@')
const spans = [
  { span_id: 'a', start_byte: nameStart, end_byte: nameStart + 3 },
  { span_id: 'b', start_byte: emailStart, end_byte: emailStart + 'ana@example.com'.length },
]

// generalize uses the suggestion; release_original keeps the original.
assert.equal(
  applyCopyDecisions(text, spans, [
    dec('a', { decision: 'generalize', suggestion_text: '[a person]' }),
    dec('b', { decision: 'release_original' }),
  ]),
  'Email [a person] at ana@example.com today',
)

// the user's edit wins over the suggestion
assert.equal(
  applyCopyDecisions(text, spans, [
    dec('a', { suggestion_text: '[a person]', user_modified_text: 'my colleague' }),
    dec('b', { decision: 'release_original' }),
  ]),
  'Email my colleague at ana@example.com today',
)

// keep_private removes; generalize with no usable replacement removes
assert.equal(
  applyCopyDecisions(text, spans, [
    dec('a', { decision: 'keep_private' }),
    dec('b', { decision: 'generalize', suggestion_text: '   ' }),
  ]),
  `Email ${COPY_REMOVED_PLACEHOLDER} at ${COPY_REMOVED_PLACEHOLDER} today`,
)

// every span kept private still yields redacted text, never the original
const allPrivate = applyCopyDecisions(text, spans, [
  dec('a', { decision: 'keep_private' }),
  dec('b', { decision: 'keep_private' }),
])
assert.ok(allPrivate !== null && !allPrivate.includes('Ana') && !allPrivate.includes('ana@'))

// a span with no decision is removed, not leaked
assert.equal(
  applyCopyDecisions(text, [spans[0]], []),
  `Email ${COPY_REMOVED_PLACEHOLDER} at ana@example.com today`,
)

// spans given out of order are still applied in order
assert.equal(
  applyCopyDecisions(text, [spans[1], spans[0]], [
    dec('a', { suggestion_text: 'X' }),
    dec('b', { suggestion_text: 'Y' }),
  ]),
  'Email X at Y today',
)

// no spans (forced-High, nothing flagged) -> unchanged
assert.equal(applyCopyDecisions(text, [], []), text)

// UTF-8: offsets are BYTE offsets. "é" is 2 bytes.
const accented = 'Café Ana'
const anaByte = new TextEncoder().encode('Café ').length
assert.equal(
  applyCopyDecisions(accented, [{ span_id: 'a', start_byte: anaByte, end_byte: anaByte + 3 }], [
    dec('a', { suggestion_text: '[name]' }),
  ]),
  'Café [name]',
)

// invalid offsets -> null, never the unmodified text
assert.equal(
  applyCopyDecisions(text, [{ span_id: 'a', start_byte: 5, end_byte: 9999 }], [dec('a', {})]),
  null,
  'out-of-range span',
)
assert.equal(
  applyCopyDecisions(
    text,
    [
      { span_id: 'a', start_byte: 0, end_byte: 8 },
      { span_id: 'b', start_byte: 4, end_byte: 12 },
    ],
    [dec('a', {}), dec('b', {})],
  ),
  null,
  'overlapping spans',
)
assert.equal(
  applyCopyDecisions(accented, [{ span_id: 'a', start_byte: 4, end_byte: 6 }], [dec('a', {})]),
  null,
  'a span boundary inside a multi-byte character',
)

// -- applyAllCopyDecisions: silently resolved (auto) + interactive ----------
// items.id=501 slice 2: spans gate3 reapplied from a prior/standing decision
// are not in the modal payload but must still shape the copied text.

function auto(start: number, len: number, decision: string, extra?: Partial<AutoResolved>): AutoResolved {
  return {
    start_byte: start,
    end_byte: start + len,
    decision,
    suggestion_text: null,
    user_modified_text: null,
    ...extra,
  }
}

// all-auto, mixed keep_private / release_original: the kept-private span is
// redacted, the released one stays, nothing sensitive is lost or leaked.
{
  const out = applyAllCopyDecisions(
    text,
    [],
    [],
    [auto(nameStart, 3, 'keep_private'), auto(emailStart, 'ana@example.com'.length, 'release_original')],
  )
  assert.equal(out, `Email ${COPY_REMOVED_PLACEHOLDER} at ana@example.com today`)
}

// all-auto generalize uses the stored replacement text
assert.equal(
  applyAllCopyDecisions(text, [], [], [auto(nameStart, 3, 'generalize', { suggestion_text: '[a person]' })]),
  'Email [a person] at ana@example.com today',
)

// partly auto (name, kept private earlier) + partly interactive (email, user
// generalizes now): both apply, in order, whichever list they came from.
assert.equal(
  applyAllCopyDecisions(
    text,
    [spans[1]],
    [dec('b', { suggestion_text: '[email]' })],
    [auto(nameStart, 3, 'keep_private')],
  ),
  `Email ${COPY_REMOVED_PLACEHOLDER} at [email] today`,
)

// the interactive modal ends in "all kept private" while an auto span was
// released: the interactive span is still redacted.
assert.equal(
  applyAllCopyDecisions(
    text,
    [spans[1]],
    [dec('b', { decision: 'keep_private' })],
    [auto(nameStart, 3, 'release_original')],
  ),
  `Email Ana at ${COPY_REMOVED_PLACEHOLDER} today`,
)

// an auto keep_private span is NEVER unredacted: unrecognized decision
// strings are treated as keep_private, and bad/overlapping offsets fail the
// copy (null) instead of falling back to the original text.
assert.equal(
  applyAllCopyDecisions(text, [], [], [auto(nameStart, 3, 'something_new')]),
  `Email ${COPY_REMOVED_PLACEHOLDER} at ana@example.com today`,
)
assert.equal(applyAllCopyDecisions(text, [], [], [auto(5, 9999, 'keep_private')]), null, 'out of range')
assert.equal(
  applyAllCopyDecisions(text, [spans[0]], [dec('a', { decision: 'release_original' })], [auto(nameStart, 3, 'keep_private')]),
  null,
  'auto and interactive spans overlapping -> fail, never leak',
)

// nothing resolved at all -> unchanged text
assert.equal(applyAllCopyDecisions(text, [], [], []), text)

console.log('copyDecisions.test.ts: all assertions passed')
