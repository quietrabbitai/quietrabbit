// Real Active Board screen -- items.id=384 slice 2, replacing the
// placeholder <p> NavShell.tsx used to render for content.type ===
// 'activeBoard'. Two containers per IA_SPEC Section 2a/2b: a bordered
// high-priority section, and the full topic list -- no third container
// (Daily Brief/Quick Launch Dock were both retired, see
// commands/active_board.rs's own header comment).
//
// Backend (commands.getActiveBoard/getTopicList/updateTopicState, "Group
// 5 -- Active Board") has been complete and callable with just
// userId/personaId since items.id=268's KeyRegistry migration
// (2026-08-15) -- NavShell.tsx and navShellConfig.ts previously carried
// stale comments claiming this required a key_hex bridge that didn't
// exist; both corrected alongside this file landing.
//
// decisions.id=741: Active Board defaults to showing ALL personas, with
// an optional filter -- independent of whatever Persona (if any) is
// active elsewhere in the shell. The top-strip 'activeBoard' button has
// never been persona-scoped to begin with (unlike 'cloudChat', gated behind
// selecting a Persona first), so there's no single-persona precursor to
// build before this: fanning out across every Persona is the natural
// shape from the start. commands.getActiveBoard itself is
// single-persona-scoped -- no backend aggregate exists -- so this calls
// it once per Persona (from the same listPersonas() result the top
// strip already fetches) and merges client-side. Fine at R1's persona
// counts (~4); revisit only if that assumption stops holding.
//
// Board's own 3-discrete-size-state sizing (decisions.id=737) is a
// WorkspaceShell-level concern (items.id=384 slice 3), not this
// component's -- it renders its content, the caller decides how much
// room it gets.
//
// Read-only this pass: commands.updateTopicState exists and is wired
// end-to-end on the backend, but no card-level state-change control is
// built here -- neither IA_SPEC's two-container description nor the
// reference mockup (board-full/bcard, static cards, no controls) call
// for one. Not invented ahead of an actual design for it.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type PersonaInfo, type TopicInfo } from '../bindings'
import './ActiveBoardPane.css'

interface BoardTopic {
  topic: TopicInfo
  persona: PersonaInfo
}

export interface ActiveBoardPaneProps {
  userId: string
  /** decisions.id=737's 'compact' Board size state (see WorkspaceShell.tsx):
   *  a single-column row list instead of the card grid, matching the
   *  reference mockup's board-full.compact treatment -- the card grid
   *  doesn't read well at the reduced width 'compact' gives it. Card
   *  content is unchanged, only the layout. */
  dense?: boolean
}

// items.id=391 (eleventh pass): this component no longer renders its own
// "Active Board" heading -- WorkspaceShell.tsx now renders a
// .section-header bar above this component whenever Board is expanded
// (matching QR's and Cloud Chat's own expanded headers, NavShell.css), so an
// internal heading here would just duplicate it under a different style.
export function ActiveBoardPane({ userId, dense = false }: ActiveBoardPaneProps) {
  const { t } = useTranslation()
  const [personas, setPersonas] = useState<PersonaInfo[]>([])
  const [topics, setTopics] = useState<BoardTopic[]>([])
  const [highPriorityIds, setHighPriorityIds] = useState<Set<string>>(new Set())
  const [loadError, setLoadError] = useState<string | null>(null)
  const [personaFilter, setPersonaFilter] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    setLoadError(null)
    setTopics([])
    setHighPriorityIds(new Set())

    commands.listPersonas(userId).then((personaResult) => {
      if (cancelled) return
      if (personaResult.status !== 'ok') {
        setLoadError(personaResult.error)
        return
      }
      setPersonas(personaResult.data)

      Promise.all(
        personaResult.data.map((persona) =>
          commands
            .getActiveBoard(userId, persona.id)
            .then((boardResult) => ({ persona, boardResult })),
        ),
      ).then((results) => {
        if (cancelled) return
        const allTopics: BoardTopic[] = []
        const highPriority = new Set<string>()
        const errors: string[] = []
        for (const { persona, boardResult } of results) {
          if (boardResult.status !== 'ok') {
            errors.push(boardResult.error)
            continue
          }
          for (const topic of boardResult.data.high_priority) {
            highPriority.add(topic.id)
          }
          for (const topic of boardResult.data.topics) {
            allTopics.push({ topic, persona })
          }
        }
        setTopics(allTopics)
        setHighPriorityIds(highPriority)
        if (errors.length > 0) setLoadError(errors.join('; '))
      })
    })

    return () => {
      cancelled = true
    }
  }, [userId])

  const visibleTopics = personaFilter
    ? topics.filter((bt) => bt.persona.id === personaFilter)
    : topics
  const highPriorityTopics = visibleTopics.filter((bt) => highPriorityIds.has(bt.topic.id))
  const restTopics = visibleTopics.filter((bt) => !highPriorityIds.has(bt.topic.id))

  return (
    <div className="active-board-pane">
      {personas.length > 1 && (
        <fieldset className="active-board-pane__persona-filter">
          <legend>{t('navShell.activeBoardPane.filterLabel')}</legend>
          <button
            type="button"
            className="active-board-pane__filter-pill"
            data-selected={personaFilter === null ? '' : undefined}
            onClick={() => setPersonaFilter(null)}
          >
            {t('navShell.activeBoardPane.filterAll')}
          </button>
          {personas.map((persona) => (
            <button
              key={persona.id}
              type="button"
              className="active-board-pane__filter-pill"
              data-selected={personaFilter === persona.id ? '' : undefined}
              onClick={() => setPersonaFilter(persona.id)}
            >
              <span
                className="active-board-pane__persona-dot"
                style={persona.color ? { background: persona.color } : undefined}
                aria-hidden="true"
              />
              {persona.display_name}
            </button>
          ))}
        </fieldset>
      )}

      {loadError && (
        <p role="alert">
          {t('navShell.activeBoardPane.loadError', { message: loadError })}
        </p>
      )}

      {visibleTopics.length === 0 && !loadError && (
        <p>{t('navShell.activeBoardPane.empty')}</p>
      )}

      {highPriorityTopics.length > 0 && (
        <section className="active-board-pane__high-priority">
          <h3 className="active-board-pane__section-label">
            {t('navShell.activeBoardPane.highPriorityLabel')}
          </h3>
          <BoardGrid topics={highPriorityTopics} dense={dense} />
        </section>
      )}

      {restTopics.length > 0 && <BoardGrid topics={restTopics} dense={dense} />}
    </div>
  )
}

interface BoardGridProps {
  topics: BoardTopic[]
  dense: boolean
}

function BoardGrid({ topics, dense }: BoardGridProps) {
  const { t } = useTranslation()
  const gridClassName = dense
    ? 'active-board-pane__grid active-board-pane__grid--dense'
    : 'active-board-pane__grid'
  return (
    <ul className={gridClassName}>
      {topics.map((bt) => (
        <li
          key={bt.topic.id}
          className="active-board-pane__card"
          style={bt.persona.color ? { borderLeftColor: bt.persona.color } : undefined}
        >
          <span className="active-board-pane__card-title">{bt.topic.display_name}</span>
          <span className="active-board-pane__card-sub">
            {t('navShell.activeBoardPane.cardSub', {
              persona: bt.persona.display_name,
              state: bt.topic.lifecycle_state,
            })}
          </span>
        </li>
      ))}
    </ul>
  )
}
