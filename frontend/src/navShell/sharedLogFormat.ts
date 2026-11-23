// items.id=957 unit 4: the pure, React-free formatting of the "What QR shared"
// view (summary lines, day and month labels, field-label lists, sentence keys),
// so it can be tested on Node's built-in TypeScript support (same precedent as
// privacyCheckBanner.ts). Every word comes through `t`; the t() call sites stay
// LITERAL here so the i18n consistency test sees each key.
//
// The backend sends sentence KEYS and field LABELS, never an event string or a raw
// field name (commands/shared_log_deck.rs); nothing here can show one.
//
// CHAT-BRAND REVIEW: all copy under navShell.shared.* is placeholder wording.

// '../bindings.ts' with the extension: see frictionGateDetail.ts on TS2835.
import type {
  SharedLogGroup,
  SharedLogItem,
  SharedLogLabel,
  SharedLogMode,
  SharedLogProvider,
} from '../bindings.ts'

export type T = (key: string, params?: Record<string, unknown>) => string

/** The i18n key of a sentence key from the backend. A key this version does not
 *  know reads as the unknown background sentence, never as a raw string. */
export function sentenceKey(key: string): string {
  switch (key) {
    case 'sent.plain':
      return 'navShell.shared.sentence.sentPlain'
    case 'sent.reviewed':
      return 'navShell.shared.sentence.sentReviewed'
    case 'sent.noReview':
      return 'navShell.shared.sentence.sentNoReview'
    case 'sent.reapplied':
      return 'navShell.shared.sentence.sentReapplied'
    case 'held.tierCeiling':
      return 'navShell.shared.sentence.heldTierCeiling'
    case 'held.sensitivity':
      return 'navShell.shared.sentence.heldSensitivity'
    case 'held.pfUnavailable':
      return 'navShell.shared.sentence.heldPfUnavailable'
    case 'held.pfError':
      return 'navShell.shared.sentence.heldPfError'
    case 'held.tooLong':
      return 'navShell.shared.sentence.heldTooLong'
    case 'held.timeout':
      return 'navShell.shared.sentence.heldTimeout'
    case 'held.spansUnreviewed':
      return 'navShell.shared.sentence.heldSpansUnreviewed'
    case 'held.voiceProfile':
      return 'navShell.shared.sentence.heldVoiceProfile'
    case 'held.exportHeld':
      return 'navShell.shared.sentence.heldExport'
    case 'bg.replyFlagged':
      return 'navShell.shared.sentence.bgReplyFlagged'
    case 'bg.floorApplied':
      return 'navShell.shared.sentence.bgFloorApplied'
    case 'bg.scanTimeout':
      return 'navShell.shared.sentence.bgScanTimeout'
    case 'bg.forcedReview':
      return 'navShell.shared.sentence.bgForcedReview'
    case 'bg.reviewRaised':
      return 'navShell.shared.sentence.bgReviewRaised'
    case 'bg.scanRan':
      return 'navShell.shared.sentence.bgScanRan'
    case 'bg.scanNotRun':
      return 'navShell.shared.sentence.bgScanNotRun'
    case 'choice.granted':
      return 'navShell.shared.sentence.choiceGranted'
    case 'choice.regranted':
      return 'navShell.shared.sentence.choiceRegranted'
    case 'choice.withdrawn':
      return 'navShell.shared.sentence.choiceWithdrawn'
    case 'choice.ceilingRaised':
      return 'navShell.shared.sentence.choiceCeilingRaised'
    case 'choice.standingUsed':
      return 'navShell.shared.sentence.choiceStandingUsed'
    case 'choice.sensitiveOverride':
      return 'navShell.shared.sentence.choiceSensitiveOverride'
    case 'choice.generated':
      return 'navShell.shared.sentence.choiceGenerated'
    case 'choice.checked':
      return 'navShell.shared.sentence.choiceChecked'
    case 'choice.outputChecked':
      return 'navShell.shared.sentence.choiceOutputChecked'
    default:
      return 'navShell.shared.sentence.bgUnknown'
  }
}

/** "A", "A and B", "A, B and C". */
export function joinList(items: string[]): string {
  if (items.length === 0) return ''
  if (typeof Intl !== 'undefined' && 'ListFormat' in Intl) {
    return new Intl.ListFormat('en', { style: 'long', type: 'conjunction' }).format(items)
  }
  if (items.length === 1) return items[0]
  return `${items.slice(0, -1).join(', ')} and ${items[items.length - 1]}`
}

export function providerName(p: SharedLogProvider | null, t: T): string {
  return p?.name ?? t('navShell.shared.outsideService')
}

export function providerNames(providers: SharedLogProvider[], t: T): string {
  return joinList(providers.map((p) => providerName(p, t)))
}

/** "Claude · Cloud · Web". An unknown mode shows the name alone. */
export function providerWithMode(p: SharedLogProvider, t: T): string {
  const mode = modeLabel(p.mode, t)
  return mode === '' ? providerName(p, t) : `${providerName(p, t)} · ${mode}`
}

export function modeLabel(mode: SharedLogMode, t: T): string {
  switch (mode) {
    case 'web':
      return t('navShell.shared.modeWeb')
    case 'api':
      return t('navShell.shared.modeApi')
    case 'unknown':
      return ''
  }
}

/** " over the web", " through your API key", or "" (the leading space is part of it). */
export function modePhrase(mode: SharedLogMode | undefined, t: T): string {
  switch (mode) {
    case 'web':
      return t('navShell.shared.phraseWeb')
    case 'api':
      return t('navShell.shared.phraseApi')
    default:
      return ''
  }
}

/** One label, as the Details grid shows it ("City", "A draft", "Other details"). */
export function labelText(l: SharedLogLabel, t: T): string {
  switch (l.kind) {
    case 'catalogued':
      return l.text ?? t('navShell.shared.labelOther')
    case 'draft':
      return t('navShell.shared.labelDraft')
    case 'message':
      return t('navShell.shared.labelMessage')
    case 'other':
      return t('navShell.shared.labelOther')
  }
}

/** A label in running text: "your city", "a draft", "other details". Only the first
 *  catalogued field of a list takes "your" ("your job title and industry"). */
function inlineLabel(l: SharedLogLabel, t: T, first: boolean): string {
  switch (l.kind) {
    case 'catalogued':
      return first
        ? t('navShell.shared.yourField', { field: (l.text ?? '').toLowerCase() })
        : (l.text ?? '').toLowerCase()
    case 'draft':
      return t('navShell.shared.inlineDraft')
    case 'message':
      return t('navShell.shared.inlineMessage')
    case 'other':
      return t('navShell.shared.inlineOther')
  }
}

function capitalize(s: string): string {
  return s.charAt(0).toUpperCase() + s.slice(1)
}

/** "Your job title and industry", capitalised, or "Nothing" for an empty list. */
export function labelList(labels: SharedLogLabel[], t: T): string {
  if (labels.length === 0) return t('navShell.shared.nothing')
  let seenCatalogued = false
  const parts = labels.map((l) => {
    const first = l.kind === 'catalogued' && !seenCatalogued
    if (l.kind === 'catalogued') seenCatalogued = true
    return inlineLabel(l, t, first)
  })
  return capitalize(joinList(parts))
}

/** The kept-back clause of a send: "Your city was kept back." or "Nothing was kept back." */
export function keptBackClause(labels: SharedLogLabel[], t: T): string {
  if (labels.length === 0) return t('navShell.shared.nothingKeptBack')
  return t('navShell.shared.keptBackClause', { list: labelList(labels, t) })
}

/** The one sentence of an item (first open). */
export function itemSentence(item: SharedLogItem, t: T): string {
  const params = {
    shared: labelList(item.shared, t),
    kept: keptBackClause(item.kept_back, t),
    provider: providerName(item.provider, t),
    mode: modePhrase(item.provider?.mode, t),
    document: item.document_type ?? t('navShell.shared.aDocument'),
  }
  return t(sentenceKey(item.sentence), params)
}

export function tagLabel(item: SharedLogItem, t: T): string {
  switch (item.tag) {
    case 'sent':
      return t('navShell.shared.tagSent')
    case 'kept_back':
      return t('navShell.shared.tagKeptBack')
    case 'choice':
      return t('navShell.shared.tagChoice')
    case 'hidden':
      return t('navShell.shared.tagBackground')
  }
}

/** The Focus display name ("Quick chat" for quick-ask, "A Focus" when unresolved). */
export function focusName(g: Pick<SharedLogGroup, 'focus'>, t: T): string {
  if (g.focus.quick_chat) return t('navShell.shared.quickChat')
  return g.focus.name ?? t('navShell.shared.aFocus')
}

function runsPart(runs: number, t: T): string {
  return (runs === 1 ? t('navShell.shared.runsOne', { count: runs }) : t('navShell.shared.runsOther', { count: runs }))
}

function keptPart(kept: number, t: T): string {
  return kept === 0
    ? t('navShell.shared.nothingKeptBackShort')
    : (kept === 1 ? t('navShell.shared.keptBackOne', { count: kept }) : t('navShell.shared.keptBackOther', { count: kept }))
}

/** "Writing Assistant: 1 run · 1 reviewed send to Claude · nothing kept back". */
export function groupSummary(g: SharedLogGroup, t: T): string {
  const sends = sendsPart(g.sends, g.all_reviewed, g.providers, t)
  const parts = [runsPart(g.runs, t), sends, keptPart(g.kept_back, t)]
  if (g.choices > 0) parts.push((g.choices === 1 ? t('navShell.shared.choicesOne', { count: g.choices }) : t('navShell.shared.choicesOther', { count: g.choices })))
  return `${focusName(g, t)}: ${parts.join(' · ')}`
}

function sendsPart(sends: number, reviewed: boolean, providers: SharedLogProvider[], t: T): string {
  if (sends === 0) return t('navShell.shared.nothingLeft')
  const to = providerNames(providers, t)
  if (to === '') {
    return reviewed
      ? (sends === 1 ? t('navShell.shared.reviewedSendOne', { count: sends }) : t('navShell.shared.reviewedSendOther', { count: sends }))
      : (sends === 1 ? t('navShell.shared.sendOne', { count: sends }) : t('navShell.shared.sendOther', { count: sends }))
  }
  return reviewed
    ? (sends === 1 ? t('navShell.shared.reviewedSendToOne', { count: sends, providers: to }) : t('navShell.shared.reviewedSendToOther', { count: sends, providers: to }))
    : (sends === 1 ? t('navShell.shared.sendToOne', { count: sends, providers: to }) : t('navShell.shared.sendToOther', { count: sends, providers: to }))
}

/** "October 2026: 2 runs · 1 send · nothing kept back". */
export function monthSummary(
  m: { month: string; runs: number; sends: number; kept_back: number },
  t: T,
): string {
  const sends =
    m.sends === 0 ? t('navShell.shared.nothingSent') : (m.sends === 1 ? t('navShell.shared.sendOne', { count: m.sends }) : t('navShell.shared.sendOther', { count: m.sends }))
  return t('navShell.shared.monthLine', {
    month: monthLabel(m.month),
    parts: [runsPart(m.runs, t), sends, keptPart(m.kept_back, t)].join(' · '),
  })
}

/** "2026-10" to "October 2026". */
export function monthLabel(month: string): string {
  const [y, mo] = month.split('-').map(Number)
  if (!y || !mo) return month
  return new Intl.DateTimeFormat('en', { month: 'long', year: 'numeric' }).format(
    new Date(y, mo - 1, 1),
  )
}

/** "2026-11-20" to "Friday 20 Nov" (a local calendar day, built without UTC drift). */
export function dayLabel(day: string): string {
  const [y, mo, d] = day.split('-').map(Number)
  if (!y || !mo || !d) return day
  const date = new Date(y, mo - 1, d)
  const weekday = new Intl.DateTimeFormat('en', { weekday: 'long' }).format(date)
  const rest = new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short' }).format(date)
  return `${weekday} ${rest}`
}

/** The right-aligned count of a day: "2 Focuses · 1 send left this computer". */
export function dayCount(groups: SharedLogGroup[], t: T): string {
  const sends = groups.reduce((n, g) => n + g.sends, 0)
  const focuses = (groups.length === 1 ? t('navShell.shared.dayFocusesOne', { count: groups.length }) : t('navShell.shared.dayFocusesOther', { count: groups.length }))
  const tail =
    sends === 0
      ? t('navShell.shared.dayNothingLeft')
      : (sends === 1 ? t('navShell.shared.daySendsOne', { count: sends }) : t('navShell.shared.daySendsOther', { count: sends }))
  return `${focuses} · ${tail}`
}

/** Groups of one day, in the order given. */
export function groupByDay(groups: SharedLogGroup[]): { day: string; groups: SharedLogGroup[] }[] {
  const out: { day: string; groups: SharedLogGroup[] }[] = []
  for (const g of groups) {
    const last = out[out.length - 1]
    if (last && last.day === g.day) last.groups.push(g)
    else out.push({ day: g.day, groups: [g] })
  }
  return out
}

/** A stable key for a group (day and Focus id). */
export function groupKey(g: Pick<SharedLogGroup, 'day' | 'focus'>): string {
  return `${g.day}|${g.focus.id}`
}

/** The local time of an item, "14:05". */
export function itemTime(time: string): string {
  const d = new Date(time)
  if (Number.isNaN(d.getTime())) return ''
  return new Intl.DateTimeFormat('en', { hour: '2-digit', minute: '2-digit', hour12: false }).format(d)
}

/** The date of the permanent cleared-on line, "20 Nov 2026". */
export function clearedDate(time: string): string {
  const d = new Date(time)
  if (Number.isNaN(d.getTime())) return ''
  return new Intl.DateTimeFormat('en-GB', { day: 'numeric', month: 'short', year: 'numeric' }).format(d)
}

/** Items of an opened group split for display: the visible ones and the
 *  background checks (shown only under "Show background checks"). */
export function splitItems(items: SharedLogItem[]): {
  visible: SharedLogItem[]
  background: SharedLogItem[]
} {
  return {
    visible: items.filter((i) => i.tag !== 'hidden'),
    background: items.filter((i) => i.tag === 'hidden'),
  }
}
