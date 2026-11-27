// items.id=959 unit 3 (decisions.id=968, 969; design record items.id=176): the
// Library screen's Facts button, the per-Persona Summary view. One Persona at a time,
// set by the screen's Persona pill row (no "All"); HistoryScreen mounts it keyed by
// Persona, so nothing of one Persona survives a switch.
//
// WHAT IT DOES: browse (General, then sections with their facts as one flowing line),
// search this Persona, open a fact (where it came from, earlier values), stop using a
// fact or a section, and the Removed list with Use again. A catalogued General fact's
// panel also has the Sharing block (items.id=960: FactSharing.tsx, factSharing.ts,
// factSharingController.ts). Add, change and the Historian are items.id=961; the
// design record's "Ask about this" therefore is not here, and stop using sits where
// it would have: in a fact's panel and on a section's header.
//
// SHARING READS FAIL ON THEIR OWN: policies that cannot be read give a failed Sharing
// block with a retry. The tree and Stop using stay usable.
//
// FAILS CLOSED: if the tree, the Removed list or a search cannot be read, the whole
// view is one line and a retry. No partial tree is ever shown.
//
// CHAT-BRAND REVIEW: all copy under navShell.facts.* is placeholder wording.

import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from 'react'
import { useTranslation } from 'react-i18next'
import {
  commands,
  type EarlierValue,
  type FactView,
  type FactsTree,
  type RemovedList,
  type SectionView,
} from '../bindings'
import {
  descendantCount,
  earlierLinkText,
  factCountText,
  fieldLabel,
  nestSections,
  provenanceText,
  sensitivityTag,
  shortDate,
  stopSectionBody,
  writeErrorText,
  type T,
} from './myFactsFormat'
import { FactSharing } from './FactSharing'
import { sharingBlock } from './factSharing'
import {
  createSharingController,
  INITIAL_SHARING,
  type SharingController,
  type SharingState,
} from './factSharingController'
import { StopUsingDialog } from './StopUsingDialog'
import './MyFactsView.css'

export interface MyFactsViewProps {
  personaId: string
  personaName: string
}

interface Loaded {
  tree: FactsTree
  removed: RemovedList
}

type Pending = { kind: 'fact'; fact: FactView } | { kind: 'section'; section: SectionView }

type History = EarlierValue[] | 'loading' | 'error'

export function MyFactsView({ personaId, personaName }: MyFactsViewProps) {
  const { t } = useTranslation()
  const tt = t as unknown as T
  const [data, setData] = useState<Loaded | null>(null)
  const [failed, setFailed] = useState(false)
  const [query, setQuery] = useState('')
  const [found, setFound] = useState<{ key: string; tree: FactsTree } | null>(null)
  const [openFact, setOpenFact] = useState<string | null>(null)
  const [histories, setHistories] = useState<Record<string, History>>({})
  const [showRemoved, setShowRemoved] = useState(false)
  const [pending, setPending] = useState<Pending | null>(null)
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  // Bumped after a write (and by retry) so everything is read again.
  const [reloadKey, setReloadKey] = useState(0)
  const [sharing, setSharing] = useState<SharingState>(INITIAL_SHARING)
  const sharingController = useRef<SharingController | null>(null)
  useEffect(() => {
    const controller = createSharingController(
      {
        get: () => commands.getFactPolicies(personaId),
        set: (fieldName, tier, policy) =>
          commands.setFactPolicy(personaId, fieldName, tier, policy),
      },
      setSharing,
    )
    sharingController.current = controller
    return () => controller.dispose()
  }, [personaId])
  // Read again whenever the facts are (a stop or a use again). Declared after the
  // effect above, so the controller exists.
  useEffect(() => {
    void sharingController.current?.load()
  }, [personaId, reloadKey])
  const alive = useRef(true)
  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  useEffect(() => {
    let cancelled = false
    Promise.all([commands.getFactsTree(personaId), commands.listRemoved(personaId)])
      .then(([tree, removed]) => {
        if (cancelled) return
        if (tree.status === 'ok' && removed.status === 'ok') {
          setData({ tree: tree.data, removed: removed.data })
          // Using the last removed item again returns to the main view.
          if (removed.data.count === 0) setShowRemoved(false)
        } else {
          setFailed(true)
        }
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [personaId, reloadKey])

  const trimmed = query.trim()
  useEffect(() => {
    if (trimmed === '') return
    let cancelled = false
    commands
      .searchFacts(personaId, trimmed)
      .then((result) => {
        if (cancelled) return
        if (result.status === 'ok') setFound({ key: `${reloadKey}|${trimmed}`, tree: result.data })
        else setFailed(true)
      })
      .catch(() => {
        if (!cancelled) setFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [personaId, trimmed, reloadKey])

  const loadHistory = useCallback(
    (factId: string) => {
      setHistories((prev) => ({ ...prev, [factId]: 'loading' }))
      const done = (value: History) => {
        if (alive.current) setHistories((prev) => ({ ...prev, [factId]: value }))
      }
      commands
        .getFactHistory(personaId, factId)
        .then((result) => done(result.status === 'ok' ? result.data : 'error'))
        .catch(() => done('error'))
    },
    [personaId],
  )

  function reload() {
    setOpenFact(null)
    setHistories({})
    setReloadKey((k) => k + 1)
  }

  function retry() {
    setFailed(false)
    setData(null)
    setFound(null)
    setQuery('')
    setNotice(null)
    reload()
  }

  /** Run one stop or use again, then read everything again, whatever it answered. */
  function write(run: () => Promise<{ status: 'ok' } | { status: 'error'; error: string }>) {
    setBusy(true)
    setNotice(null)
    const done = (error: string | null) => {
      if (!alive.current) return
      setBusy(false)
      setPending(null)
      setNotice(error === null ? null : writeErrorText(tt, error))
      reload()
    }
    run()
      .then((result) => done(result.status === 'ok' ? null : result.error))
      .catch(() => done('failed'))
  }

  function confirmStop(target: Pending) {
    write(() =>
      target.kind === 'fact'
        ? commands.stopUsingFact(personaId, target.fact.id)
        : commands.stopUsingSection(personaId, target.section.id),
    )
  }

  /** Up and Down move between facts in reading order; Escape closes the open panel. */
  function onTreeKey(e: KeyboardEvent<HTMLDivElement>) {
    if (e.key === 'Escape') {
      setOpenFact(null)
      return
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return
    const buttons = Array.from(e.currentTarget.querySelectorAll<HTMLElement>('[data-fact]'))
    const at = buttons.indexOf(document.activeElement as HTMLElement)
    if (at === -1) return
    e.preventDefault()
    buttons[at + (e.key === 'ArrowDown' ? 1 : -1)]?.focus()
  }

  if (failed) {
    return (
      <div className="my-facts">
        <p role="alert" className="my-facts__unreadable">
          {t('navShell.facts.unreadable')}
        </p>
        <button type="button" className="my-facts__button" onClick={retry}>
          {t('navShell.facts.retry')}
        </button>
      </div>
    )
  }
  if (data === null) {
    return <p className="my-facts__muted">{t('navShell.facts.loading')}</p>
  }

  const { tree, removed } = data
  const nothingKept = tree.fact_count === 0 && tree.sections.length === 0
  const searching = trimmed !== ''
  // A result is only shown for the query AND the read it answered, so a stopped fact
  // never flashes back from an older search.
  const shown = searching ? (found?.key === `${reloadKey}|${trimmed}` ? found.tree : null) : tree

  function factPanel(fact: FactView, inGeneral: boolean) {
    const history = histories[fact.id]
    const source = provenanceText(tt, fact.provenance)
    const since = t('navShell.facts.since', { date: shortDate(fact.since) })
    return (
      <div className="my-facts__panel">
        <p className="my-facts__panel-value">
          <span className="my-facts__label">{fieldLabel(tt, fact.label)}</span> {fact.value}
        </p>
        <p className="my-facts__muted">{source === null ? since : `${source} · ${since}`}</p>
        <div className="my-facts__actions">
          {fact.earlier_count > 0 && history === undefined && (
            <button type="button" className="my-facts__link" onClick={() => loadHistory(fact.id)}>
              {earlierLinkText(tt, fact.earlier_count)}
            </button>
          )}
          <button
            type="button"
            className="my-facts__link"
            onClick={() => setPending({ kind: 'fact', fact })}
          >
            {t('navShell.facts.stop.fact')}
          </button>
        </div>
        <FactSharing
          block={sharingBlock(fact, inGeneral, sharing.load)}
          fieldText={fieldLabel(tt, fact.label)}
          busy={sharing.busy}
          errorCode={
            sharing.writeError?.field === fact.label ? sharing.writeError.code : null
          }
          onChoose={(tier, word) =>
            void sharingController.current?.choose(fact.label, tier, word)
          }
          onRetry={() => void sharingController.current?.load()}
        />
        {history === 'loading' && <p className="my-facts__muted">{t('navShell.facts.loading')}</p>}
        {history === 'error' && <p role="alert">{t('navShell.facts.earlierError')}</p>}
        {Array.isArray(history) && (
          <ul className="my-facts__earlier">
            {history.map((v, i) => (
              <li key={`${v.until}|${i}`}>
                {v.value}
                <span className="my-facts__muted">
                  {' '}
                  {t('navShell.facts.earlierRange', {
                    from: shortDate(v.since),
                    until: shortDate(v.until),
                  })}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    )
  }

  function factLine(facts: FactView[], inGeneral: boolean) {
    const open = facts.find((f) => f.id === openFact)
    return (
      <>
        <ul className="my-facts__line">
          {facts.map((fact) => {
            const tag = sensitivityTag(tt, fact.sensitivity)
            return (
              <li key={fact.id} className="my-facts__pair">
                <button
                  type="button"
                  className="my-facts__fact"
                  data-fact=""
                  aria-expanded={openFact === fact.id}
                  onClick={() => {
                    sharingController.current?.clearError()
                    setOpenFact(openFact === fact.id ? null : fact.id)
                  }}
                >
                  <span className="my-facts__label">{fieldLabel(tt, fact.label)}</span>{' '}
                  {fact.value}
                  {tag !== null && <span className="my-facts__tag">{tag}</span>}
                </button>
              </li>
            )
          })}
        </ul>
        {open !== undefined && factPanel(open, inGeneral)}
      </>
    )
  }

  function sectionBlock(section: SectionView) {
    // The confirm counts from the WHOLE tree, never from a filtered search result.
    const whole = tree.sections.find((s) => s.id === section.id) ?? section
    return (
      <>
        <div className="my-facts__section-head">
          <h3 className="my-facts__section-name">
            {section.name} <span className="my-facts__type">{section.entity_type}</span>
          </h3>
          <button
            type="button"
            className="my-facts__link"
            onClick={() => setPending({ kind: 'section', section: whole })}
          >
            {t('navShell.facts.stop.section', { name: section.name })}
          </button>
        </div>
        {section.facts.length > 0 ? (
          factLine(section.facts, false)
        ) : (
          !searching && <p className="my-facts__muted">{t('navShell.facts.sectionEmpty')}</p>
        )}
      </>
    )
  }

  function treeBody(visible: FactsTree) {
    return (
      <div className="my-facts__tree" role="group" onKeyDown={onTreeKey}>
        {visible.general.length > 0 && (
          <section className="my-facts__section">
            <div className="my-facts__section-head">
              <h3 className="my-facts__section-name">
                {t('navShell.facts.general')}{' '}
                <span className="my-facts__type">{t('navShell.facts.generalSub')}</span>
              </h3>
            </div>
            {factLine(visible.general, true)}
          </section>
        )}
        {nestSections(visible.sections).map((node) => (
          <section key={node.section.id} className="my-facts__section">
            {sectionBlock(node.section)}
            {node.children.map((child) => (
              <div key={child.id} className="my-facts__child">
                {sectionBlock(child)}
              </div>
            ))}
          </section>
        ))}
      </div>
    )
  }

  function removedBody() {
    return (
      <div className="my-facts__removed">
        <div className="my-facts__section-head">
          <h3 className="my-facts__section-name">{t('navShell.facts.removed.title')}</h3>
          <button type="button" className="my-facts__link" onClick={() => setShowRemoved(false)}>
            {t('navShell.facts.removed.back')}
          </button>
        </div>
        <p className="my-facts__muted">
          {removed.count === 0
            ? t('navShell.facts.removed.empty')
            : t('navShell.facts.removed.note')}
        </p>
        <ul className="my-facts__removed-list">
          {removed.sections.map((s) => (
            <li key={s.id} className="my-facts__removed-row">
              <div>
                <p className="my-facts__removed-main">
                  {s.name} <span className="my-facts__type">{s.entity_type}</span>
                </p>
                <p className="my-facts__muted">
                  {factCountText(tt, s.fact_count)} ·{' '}
                  {t('navShell.facts.removed.date', { date: shortDate(s.stopped_at) })}
                </p>
              </div>
              <button
                type="button"
                className="my-facts__button"
                disabled={busy}
                onClick={() => write(() => commands.useAgainSection(personaId, s.id))}
              >
                {t('navShell.facts.removed.useAgain')}
              </button>
            </li>
          ))}
          {removed.facts.map((f) => (
            <li key={f.id} className="my-facts__removed-row">
              <div>
                <p className="my-facts__removed-main">
                  <span className="my-facts__label">
                    {f.section_name ?? t('navShell.facts.general')} · {fieldLabel(tt, f.label)}
                  </span>{' '}
                  <s>{f.value}</s>
                </p>
                <p className="my-facts__muted">
                  {t('navShell.facts.removed.date', { date: shortDate(f.stopped_at) })}
                </p>
              </div>
              <button
                type="button"
                className="my-facts__button"
                disabled={busy}
                onClick={() => write(() => commands.useAgainFact(personaId, f.id))}
              >
                {t('navShell.facts.removed.useAgain')}
              </button>
            </li>
          ))}
        </ul>
      </div>
    )
  }

  function dialogLines(target: Pending): string[] {
    const limits = t('navShell.facts.stop.limits')
    if (target.kind === 'fact') {
      const lines = [t('navShell.facts.stop.factBody'), limits]
      if (target.fact.sync_may_restore) lines.push(t('navShell.facts.stop.syncMayRestore'))
      return lines
    }
    const { id, facts, stop_fact_count: count } = target.section
    const hasInside = descendantCount(tree.sections, id) > 0 || count > facts.length
    return [stopSectionBody(tt, count, hasInside), limits]
  }

  return (
    <div className="my-facts">
      <header>
        <h2 className="my-facts__title">{t('navShell.facts.title', { persona: personaName })}</h2>
        <p className="my-facts__muted">
          {t('navShell.facts.purpose')} {!nothingKept && factCountText(tt, tree.fact_count)}
        </p>
      </header>

      {!showRemoved && (!nothingKept || removed.count > 0) && (
        <div className="my-facts__toolbar">
          {!nothingKept && (
            <input
              type="search"
              className="my-facts__search"
              value={query}
              placeholder={t('navShell.facts.searchLabel', { persona: personaName })}
              aria-label={t('navShell.facts.searchLabel', { persona: personaName })}
              onChange={(e) => {
                setQuery(e.target.value)
                setOpenFact(null)
              }}
            />
          )}
          {removed.count > 0 && (
            <button
              type="button"
              className="my-facts__button"
              onClick={() => {
                setNotice(null)
                setOpenFact(null)
                setShowRemoved(true)
              }}
            >
              {t('navShell.facts.removed.link', { count: removed.count })}
            </button>
          )}
        </div>
      )}

      {notice !== null && (
        <p role="alert" className="my-facts__notice">
          {notice}
        </p>
      )}

      {showRemoved ? (
        removedBody()
      ) : nothingKept ? (
        <p className="my-facts__empty">
          {t('navShell.facts.empty', { persona: personaName })}
        </p>
      ) : shown === null ? (
        <p className="my-facts__muted">{t('navShell.facts.loading')}</p>
      ) : searching && shown.general.length === 0 && shown.sections.length === 0 ? (
        <p className="my-facts__empty">
          {t('navShell.facts.noMatches', { persona: personaName })}
        </p>
      ) : (
        treeBody(shown)
      )}

      {pending !== null && (
        <StopUsingDialog
          title={
            pending.kind === 'fact'
              ? t('navShell.facts.stop.factTitle')
              : t('navShell.facts.stop.sectionTitle', { name: pending.section.name })
          }
          lines={dialogLines(pending)}
          busy={busy}
          onConfirm={() => confirmStop(pending)}
          onCancel={() => setPending(null)}
        />
      )}
    </div>
  )
}
