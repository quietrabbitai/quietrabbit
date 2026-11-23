// items.id=957 unit 4 (decisions.id=967; design record items.id=956): the
// "What QR shared" view, a sibling view button on the Library screen after Chat
// History. Per Persona (the screen's Persona pill row governs it, no "All").
//
// ORGANISED, NEVER A FLAT LIST: recent days (default 30), then an Earlier block of
// collapsed months. A day holds one summary line per Focus; opening it shows one
// sentence per item; "Details" opens the field LABELS (never values, never raw
// field names). The view loads from the summary command only (one row per day and
// Focus); a group's items load when it is opened.
//
// WHAT THIS FILE CAN SHOW: sentence keys and labels the backend already translated
// (commands/shared_log_deck.rs). It never receives a raw event string, a raw field
// name or a field value, so it cannot render one.
//
// CHAT-BRAND REVIEW: all copy under navShell.shared.* is placeholder wording; the
// layout follows the mockup of items.id=956.

import { useEffect, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import {
  commands,
  type SharedLogGroup,
  type SharedLogItem,
  type SharedLogSummary,
  type SharedLogWindow,
} from '../bindings'
import {
  clearedDate,
  dayCount,
  dayLabel,
  groupByDay,
  groupKey,
  groupSummary,
  itemSentence,
  itemTime,
  labelText,
  monthSummary,
  providerWithMode,
  splitItems,
  tagLabel,
  type T,
} from './sharedLogFormat'
import './SharedLogView.css'

export interface SharedLogViewProps {
  personaId: string
  personaName: string
  personaColor: string | null
}

type ItemsState = SharedLogItem[] | 'loading' | 'error'

export function SharedLogView({ personaId, personaName, personaColor }: SharedLogViewProps) {
  const { t } = useTranslation()
  const tt = t as unknown as T
  const [windowSel, setWindowSel] = useState<SharedLogWindow>('last30')
  const [focusFilter, setFocusFilter] = useState<string | null>(null)
  const [providerFilter, setProviderFilter] = useState<string | null>(null)
  const [summary, setSummary] = useState<SharedLogSummary | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [openGroups, setOpenGroups] = useState<Set<string>>(new Set())
  const [openMonths, setOpenMonths] = useState<Set<string>>(new Set())
  const [openDetails, setOpenDetails] = useState<Set<string>>(new Set())
  const [openBackground, setOpenBackground] = useState<Set<string>>(new Set())
  const [items, setItems] = useState<Record<string, ItemsState>>({})
  // Minutes east of UTC, so a day is the user's own calendar day.
  const offset = useMemo(() => -new Date().getTimezoneOffset(), [])
  const filterSig = `${windowSel}|${focusFilter ?? ''}|${providerFilter ?? ''}`

  useEffect(() => {
    let cancelled = false
    setError(null)
    commands
      .getSharedLogSummary(personaId, windowSel, offset, focusFilter, providerFilter)
      .then((result) => {
        if (cancelled) return
        if (result.status === 'ok') setSummary(result.data)
        else setError(result.error)
      })
    return () => {
      cancelled = true
    }
  }, [personaId, windowSel, focusFilter, providerFilter, offset])

  // A new filter or window changes what a group holds: drop what was loaded.
  useEffect(() => {
    setItems({})
  }, [filterSig])

  // Load the items of every open group that has none yet.
  useEffect(() => {
    let cancelled = false
    for (const key of openGroups) {
      if (items[key] !== undefined) continue
      const [day, focusId] = key.split('|')
      setItems((prev) => ({ ...prev, [key]: 'loading' }))
      commands
        .getSharedLogGroupItems(personaId, day, focusId, offset, providerFilter)
        .then((result) => {
          if (cancelled) return
          setItems((prev) => ({
            ...prev,
            [key]: result.status === 'ok' ? result.data : 'error',
          }))
        })
    }
    return () => {
      cancelled = true
    }
  }, [openGroups, items, personaId, offset, providerFilter])

  function toggle(set: Set<string>, key: string, apply: (s: Set<string>) => void) {
    const next = new Set(set)
    if (next.has(key)) next.delete(key)
    else next.add(key)
    apply(next)
  }

  const filtersChanged = windowSel !== 'last30' || focusFilter !== null || providerFilter !== null

  if (error) {
    return <p role="alert">{t('navShell.shared.loadError', { message: error })}</p>
  }
  if (summary === null) {
    return <p className="history-screen__notice">{t('navShell.shared.loading')}</p>
  }

  const dayBlocks = (groups: SharedLogGroup[]) =>
    groupByDay(groups).map((block) => (
      <section className="shared-log__day" key={block.day}>
        <header className="shared-log__day-head">
          <h3 className="shared-log__day-title">{dayLabel(block.day)}</h3>
          <span className="shared-log__day-count">{dayCount(block.groups, tt)}</span>
        </header>
        {block.groups.map((g) => {
          const key = groupKey(g)
          const open = openGroups.has(key)
          const loaded = items[key]
          return (
            <div className="shared-log__group" key={key}>
              <button
                type="button"
                className="shared-log__group-head"
                aria-expanded={open}
                onClick={() => toggle(openGroups, key, setOpenGroups)}
              >
                <span
                  className="shared-log__bar"
                  style={personaColor ? { background: personaColor } : undefined}
                  aria-hidden="true"
                />
                <span className="shared-log__summary">{groupSummary(g, tt)}</span>
                <span className="shared-log__chevron" aria-hidden="true">
                  {open ? '▾' : '▸'}
                </span>
              </button>
              {open && loaded === 'loading' && (
                <p className="history-screen__notice">{t('navShell.shared.loading')}</p>
              )}
              {open && loaded === 'error' && (
                <p role="alert">{t('navShell.shared.itemsError')}</p>
              )}
              {open && Array.isArray(loaded) && renderItems(key, loaded, g.background)}
            </div>
          )
        })}
      </section>
    ))

  const renderItems = (groupId: string, all: SharedLogItem[], backgroundCount: number) => {
    const { visible, background } = splitItems(all)
    const showBackground = openBackground.has(groupId)
    const row = (item: SharedLogItem, index: number) => {
      const id = `${groupId}|${index}|${item.time}`
      const detailsOpen = openDetails.has(id)
      return (
        <li className="shared-log__item" key={id}>
          <div className="shared-log__item-line">
            <span className="shared-log__tag" data-tag={item.tag}>
              {tagLabel(item, tt)}
            </span>
            <span className="shared-log__sentence">{itemSentence(item, tt)}</span>
            <button
              type="button"
              className="shared-log__details-link"
              aria-expanded={detailsOpen}
              onClick={() => toggle(openDetails, id, setOpenDetails)}
            >
              {detailsOpen ? t('navShell.shared.hideDetails') : t('navShell.shared.details')}
            </button>
          </div>
          {detailsOpen && (
            <dl className="shared-log__grid">
              <dt>{t('navShell.shared.detailWhen')}</dt>
              <dd>{itemTime(item.time)}</dd>
              <dt>{t('navShell.shared.detailWhat')}</dt>
              <dd>{tagLabel(item, tt)}</dd>
              {item.summary !== null && (
                <>
                  <dt>{t('navShell.shared.detailRecorded')}</dt>
                  <dd>{item.summary}</dd>
                </>
              )}
              {(item.tag === 'sent' || item.shared.length > 0) && (
                <>
                  <dt>{t('navShell.shared.detailShared')}</dt>
                  <dd>
                    {item.shared.length === 0
                      ? t('navShell.shared.nothing')
                      : item.shared.map((l) => labelText(l, tt)).join(', ')}
                  </dd>
                </>
              )}
              {(item.tag === 'sent' || item.tag === 'kept_back') && (
                <>
                  <dt>{t('navShell.shared.detailKeptBack')}</dt>
                  <dd>
                    {item.kept_back.length === 0
                      ? t('navShell.shared.nothing')
                      : item.kept_back.map((l) => labelText(l, tt)).join(', ')}
                  </dd>
                  <dt>{t('navShell.shared.detailSentTo')}</dt>
                  <dd>
                    {item.provider !== null && item.tag === 'sent'
                      ? providerWithMode(item.provider, tt)
                      : t('navShell.shared.detailNobody')}
                  </dd>
                </>
              )}
              <dd className="shared-log__grid-note">{t('navShell.shared.detailNote')}</dd>
            </dl>
          )}
        </li>
      )
    }
    return (
      <>
        <ul className="shared-log__items">{visible.map(row)}</ul>
        {backgroundCount > 0 && background.length > 0 && (
          <div className="shared-log__background">
            <button
              type="button"
              className="shared-log__details-link"
              aria-expanded={showBackground}
              onClick={() => toggle(openBackground, groupId, setOpenBackground)}
            >
              {showBackground
                ? t('navShell.shared.hideBackground')
                : t('navShell.shared.showBackground', { count: background.length })}
            </button>
            {showBackground && (
              <ul className="shared-log__items">{background.map(row)}</ul>
            )}
          </div>
        )}
      </>
    )
  }

  const nothingYet = !summary.has_any
  const noMatches = summary.has_any && summary.groups.length === 0 && summary.months.length === 0

  return (
    <div className="shared-log">
      <h2 className="shared-log__title">{t('navShell.shared.title', { persona: personaName })}</h2>
      <p className="shared-log__purpose">{t('navShell.shared.purpose')}</p>

      {summary.cleared_on !== null && (
        <p className="shared-log__cleared" role="note">
          {t('navShell.shared.clearedNotice', { date: clearedDate(summary.cleared_on) })}
        </p>
      )}

      {nothingYet ? (
        <div className="shared-log__empty">
          <p className="shared-log__empty-title">
            {t('navShell.shared.emptyTitle', { persona: personaName })}
          </p>
          <p className="shared-log__empty-body">{t('navShell.shared.emptyBody')}</p>
        </div>
      ) : (
        <>
          <div className="shared-log__filters">
            <label className="shared-log__filter">
              <span>{t('navShell.shared.focusFilter')}</span>
              <select
                value={focusFilter ?? ''}
                onChange={(e) => setFocusFilter(e.target.value === '' ? null : e.target.value)}
              >
                <option value="">{t('navShell.shared.allFocuses')}</option>
                {summary.focuses.map((f) => (
                  <option key={f.id} value={f.id}>
                    {f.quick_chat
                      ? t('navShell.shared.quickChat')
                      : (f.name ?? t('navShell.shared.aFocus'))}
                  </option>
                ))}
              </select>
            </label>
            <label className="shared-log__filter">
              <span>{t('navShell.shared.providerFilter')}</span>
              <select
                value={providerFilter ?? ''}
                onChange={(e) => setProviderFilter(e.target.value === '' ? null : e.target.value)}
              >
                <option value="">{t('navShell.shared.anyProvider')}</option>
                {summary.providers.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name ?? t('navShell.shared.outsideService')}
                  </option>
                ))}
              </select>
            </label>
            <label className="shared-log__filter">
              <span>{t('navShell.shared.dateFilter')}</span>
              <select
                value={windowSel}
                onChange={(e) => setWindowSel(e.target.value as SharedLogWindow)}
              >
                <option value="last30">{t('navShell.shared.windowLast30')}</option>
                <option value="last7">{t('navShell.shared.windowLast7')}</option>
                <option value="everything">{t('navShell.shared.windowEverything')}</option>
              </select>
            </label>
            {filtersChanged && (
              <button
                type="button"
                className="shared-log__reset"
                onClick={() => {
                  setWindowSel('last30')
                  setFocusFilter(null)
                  setProviderFilter(null)
                }}
              >
                {t('navShell.shared.resetFilters')}
              </button>
            )}
          </div>

          {noMatches ? (
            <div className="shared-log__empty">
              <p className="shared-log__empty-title">{t('navShell.shared.noMatchesTitle')}</p>
              <p className="shared-log__empty-body">{t('navShell.shared.noMatchesBody')}</p>
            </div>
          ) : (
            <>
              {dayBlocks(summary.groups)}
              {summary.months.length > 0 && (
                <section className="shared-log__earlier">
                  <h3 className="shared-log__day-title">{t('navShell.shared.earlier')}</h3>
                  {summary.months.map((m) => {
                    const open = openMonths.has(m.month)
                    return (
                      <div className="shared-log__month" key={m.month}>
                        <button
                          type="button"
                          className="shared-log__group-head"
                          aria-expanded={open}
                          onClick={() => toggle(openMonths, m.month, setOpenMonths)}
                        >
                          <span className="shared-log__summary">{monthSummary(m, tt)}</span>
                          <span className="shared-log__chevron" aria-hidden="true">
                            {open ? '▾' : '▸'}
                          </span>
                        </button>
                        {open && dayBlocks(m.groups)}
                      </div>
                    )
                  })}
                </section>
              )}
            </>
          )}
          <p className="shared-log__footer">{t('navShell.shared.footer')}</p>
        </>
      )}
    </div>
  )
}
