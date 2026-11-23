// Plain-assertion test for sharedLogFormat.ts (items.id=957), same convention as
// privacyCheckBanner.test.ts: Node's built-in TypeScript support, no framework.
//
//   node --experimental-strip-types src/navShell/sharedLogFormat.test.ts
//
// (also wired as part of `npm test`.) It runs against the REAL en.json copy, and
// reads the Rust deck so a sentence key the backend can send but the frontend cannot
// translate fails here.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  dayCount,
  dayLabel,
  groupByDay,
  groupSummary,
  itemSentence,
  keptBackClause,
  labelList,
  monthSummary,
  sentenceKey,
  splitItems,
  type T,
} from './sharedLogFormat.ts'
import type { SharedLogGroup, SharedLogItem, SharedLogLabel } from '../bindings.ts'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const en = JSON.parse(fs.readFileSync(path.join(HERE, '..', 'i18n', 'en.json'), 'utf8'))

const t: T = (key, params) => {
  let node: unknown = en
  for (const seg of key.split('.')) node = (node as Record<string, unknown>)?.[seg]
  assert.equal(typeof node, 'string', `no en.json string for ${key}`)
  return (node as string).replace(/\{\{(\w+)\}\}/g, (_m, name) => String(params?.[name] ?? ''))
}

const group = (over: Partial<SharedLogGroup> = {}): SharedLogGroup => ({
  day: '2026-11-20',
  focus: { id: 'writing-assistant', name: 'Writing Assistant', quick_chat: false },
  runs: 1,
  sends: 1,
  all_reviewed: true,
  providers: [{ id: 'p', name: 'Claude', mode: 'web' }],
  kept_back: 0,
  choices: 0,
  background: 0,
  ...over,
})

// The spec's own examples (design record section 3).
assert.equal(
  groupSummary(group(), t),
  'Writing Assistant: 1 run · 1 reviewed send to Claude · nothing kept back',
)
assert.equal(
  groupSummary(
    group({
      runs: 3,
      sends: 2,
      all_reviewed: false,
      providers: [
        { id: 'a', name: 'Claude', mode: 'web' },
        { id: 'b', name: 'ChatGPT', mode: 'api' },
      ],
      kept_back: 2,
      choices: 1,
    }),
    t,
  ),
  'Writing Assistant: 3 runs · 2 sends to Claude and ChatGPT · 2 things kept back · 1 choice you made',
)
assert.equal(
  groupSummary(group({ sends: 0, providers: [], kept_back: 1, all_reviewed: false }), t),
  'Writing Assistant: 1 run · nothing left this computer · 1 thing kept back',
)
// Quick chat, an unresolved Focus and an unnamed provider.
assert.match(
  groupSummary(group({ focus: { id: 'quick-ask', name: null, quick_chat: true } }), t),
  /^Quick chat: /,
)
assert.match(groupSummary(group({ focus: { id: '', name: null, quick_chat: false } }), t), /^A Focus: /)
assert.match(
  groupSummary(group({ providers: [{ id: 'gone', name: null, mode: 'unknown' }] }), t),
  /send to an outside service/,
)

// Months and days.
assert.equal(
  monthSummary({ month: '2026-10', runs: 2, sends: 1, kept_back: 0 }, t),
  'October 2026: 2 runs · 1 send · nothing kept back',
)
assert.equal(
  monthSummary({ month: '2026-09', runs: 1, sends: 0, kept_back: 0 }, t),
  'September 2026: 1 run · nothing sent · nothing kept back',
)
assert.equal(dayLabel('2026-11-20'), 'Friday 20 Nov')
assert.equal(dayCount([group(), group({ sends: 0 })], t), '2 Focuses · 1 send left this computer')
assert.equal(dayCount([group({ sends: 0 })], t), '1 Focus · nothing left this computer')
assert.equal(groupByDay([group(), group(), group({ day: '2026-11-19' })]).length, 2)

// Item sentences and labels.
const cat = (text: string): SharedLogLabel => ({ kind: 'catalogued', text })
assert.equal(labelList([cat('Job title'), cat('Industry')], t), 'Your job title and industry')
assert.equal(labelList([], t), 'Nothing')
assert.equal(keptBackClause([], t), 'Nothing was kept back.')
assert.equal(keptBackClause([cat('City')], t), 'Your city was kept back.')
const sent: SharedLogItem = {
  time: '2026-11-20T10:00:00+00:00',
  tag: 'sent',
  sentence: 'sent.plain',
  provider: { id: 'p', name: 'Claude', mode: 'web' },
  shared: [cat('Job title')],
  kept_back: [{ kind: 'other', text: null }],
  document_type: null,
  summary: null,
}
assert.equal(
  itemSentence(sent, t),
  'Your job title went to Claude over the web. Other details was kept back.',
)

// A raw event string (or any key this version does not know) never shows: it reads
// as the unknown background sentence.
for (const raw of ['gate1_pass', 'something.new', '']) {
  assert.equal(sentenceKey(raw), 'navShell.shared.sentence.bgUnknown')
  assert.equal(
    itemSentence({ ...sent, sentence: raw }, t),
    'Something was recorded that this version can\'t describe.',
  )
}

// Every sentence key the Rust deck can send translates to a real, non-unknown string.
const deck = fs.readFileSync(path.join(HERE, '..', '..', '..', 'src-tauri', 'src', 'commands', 'shared_log_deck.rs'), 'utf8')
const production = deck.split('#[cfg(test)]')[0]
const keys = new Set([...production.matchAll(/"((?:sent|held|bg|choice)\.[A-Za-z]+)"/g)].map((m) => m[1]))
assert.ok(keys.size >= 30, `found only ${keys.size} sentence keys in the deck`)
for (const k of keys) {
  if (k === 'bg.unknown') continue
  assert.notEqual(sentenceKey(k), 'navShell.shared.sentence.bgUnknown', `the frontend cannot translate ${k}`)
  assert.ok(t(sentenceKey(k), { shared: 'x', kept: 'y', provider: 'z', mode: '', document: 'd' }).length > 0)
}
assert.equal(sentenceKey('bg.unknown'), 'navShell.shared.sentence.bgUnknown')

// Hidden items split off (shown only under "Show background checks").
const parts = splitItems([sent, { ...sent, tag: 'hidden' }])
assert.deepEqual([parts.visible.length, parts.background.length], [1, 1])

console.log('sharedLogFormat.test.ts: all assertions passed')
