// The unified History screen -- items.id=404. Row-stack by ownership
// level (User / Group / Persona rows -- Focus explicitly deferred, held
// open pending items.id=400/401), bottom action-pane reusing the same
// resolve-to-a-descriptor idea navShellConfig.ts's old currentContent used
// (NavShell.tsx), "applied per-row" per the design doc: HistoryActionContent
// below is that same pattern, sized to this screen's own local state
// instead of NavShell's removed global one.
//
// Absorbs two things that used to be separate: Chat History (previously
// ChatHistoryList.tsx's own self-fetching dropdown, now this screen's
// Persona-row "Chat History" action) and My Facts (previously an unbuilt
// placeholder behind its own top-strip button, now the exact same
// placeholder string reused as a "Facts" row-action at every level -- not
// a new build, just placed structurally per the design doc).
//
// Group row (items.id=404 follow-up, Jason 2026-09-03): built for real,
// not a static placeholder, but conditionally hidden -- shown only when
// `commands.listPersonaGroupIds(activePersonaId)` (persistence::
// group_key_store::list_group_keys, wrapped for IPC this item) returns
// non-empty for whichever Persona is currently active elsewhere in the
// app. No groups-metadata table exists anywhere to source a real display
// name from (list_group_keys/list_persona_group_ids only returns opaque
// group_ids) -- the row renders a generic "Group" / "{{count}} Groups"
// label, never a fabricated name. items.id=407 tracks the real
// Group/household-sharing GUI (creation, invitation, permission
// management, a real display-name mechanism) this row is deliberately NOT
// attempting to be.

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type ChatInfo, type OutputInfo, type PersonaInfo } from '../bindings'
import { DocumentRow } from '../library/DocumentRow'
import '../chat/ChatHistoryList.css'
import './HistoryScreen.css'

/** Hand-declared, not generated: ChatActivityUpdatedPayload
 *  (commands/messages.rs) is emitted via AppHandle::emit(), not
 *  returned/accepted by any #[tauri::command] -- tauri-specta's
 *  collect_commands! only walks types reachable from the registered
 *  command surface, so specta::Type on the Rust struct alone doesn't get
 *  it into bindings.ts. This codebase has no typed-event registration
 *  (no collect_events!/mount_events anywhere) to fix that with -- same
 *  convention as ChatPane.tsx's RunStatusPayload/MessageContentReadyPayload.
 *  Keep this in sync by hand with ChatActivityUpdatedPayload's field list
 *  if that struct changes. */
interface ChatActivityUpdatedPayload {
  persona_id: string
}

export interface HistoryOpenTarget {
  personaId: string
  action: 'chatHistory'
}

export interface HistoryScreenProps {
  userId: string
  personas: PersonaInfo[]
  /** Whichever Persona is active elsewhere (Chat/Library) -- used only to
   *  decide the Group row's visibility (Jason, 2026-09-03: scoped to "the
   *  active Persona", not evaluated per-row across every Persona). */
  activePersonaId: string | null
  /** History's Persona-row "Chat" action, and Chat History's own
   *  "resume this chat" row-action -- chat is null for the former (lands
   *  on whatever Chat's own default view is), set for the latter. */
  onOpenPersonaChat: (personaId: string, chat: ChatInfo | null) => void
  /** The Library preview's "Open full Library ->" escape hatch. */
  onOpenFullLibrary: (personaId: string) => void
  /** Chat's own "Chat history" toggle jumping in from outside -- consumed
   *  once (see onOpenTargetConsumed). */
  openTarget: HistoryOpenTarget | null
  onOpenTargetConsumed: () => void
}

type RowId = 'user' | 'group' | `persona:${string}`

type HistoryActionContent =
  | { type: 'facts' }
  | { type: 'chatHistory'; personaId: string }
  | { type: 'libraryPreview'; personaId: string }
  | null

export function HistoryScreen({
  userId,
  personas,
  activePersonaId,
  onOpenPersonaChat,
  onOpenFullLibrary,
  openTarget,
  onOpenTargetConsumed,
}: HistoryScreenProps) {
  const { t } = useTranslation()
  const [expandedRow, setExpandedRow] = useState<RowId | null>(null)
  const [actionContent, setActionContent] = useState<HistoryActionContent>(null)
  const [sessionDisplayName, setSessionDisplayName] = useState<string | null>(null)
  const [groupIds, setGroupIds] = useState<string[]>([])

  useEffect(() => {
    commands.getSession().then((result) => {
      if (result.status === 'ok' && result.data) {
        setSessionDisplayName(result.data.display_name)
      }
    })
  }, [])

  // Fail-closed: an error checking group membership is treated the same
  // as "no groups" (hide the row) -- this is a visibility check, not a
  // page a user is trying to use, so there's nothing useful an error
  // banner here would let them do differently.
  useEffect(() => {
    setGroupIds([])
    if (activePersonaId === null) return
    commands.listPersonaGroupIds(activePersonaId).then((result) => {
      if (result.status === 'ok') setGroupIds(result.data)
    })
  }, [activePersonaId])

  // Chat's "Chat history" toggle jumping in -- expands the right Persona
  // row and pre-selects Chat History as the showing action-pane content,
  // matching the design doc's transition description verbatim ("'Chat
  // History' already showing in the action-pane").
  useEffect(() => {
    if (!openTarget) return
    setExpandedRow(`persona:${openTarget.personaId}`)
    setActionContent({ type: 'chatHistory', personaId: openTarget.personaId })
    onOpenTargetConsumed()
  }, [openTarget, onOpenTargetConsumed])

  const handleSelectRow = useCallback((rowId: RowId) => {
    setExpandedRow((prev) => {
      if (prev === rowId) {
        setActionContent(null)
        return null
      }
      setActionContent(null)
      return rowId
    })
  }, [])

  const showGroupRow = groupIds.length > 0

  return (
    <div className="history-screen">
      <ul className="history-screen__rows">
        <li className="history-screen__row">
          <button
            type="button"
            className="history-screen__row-header"
            data-selected={expandedRow === 'user' ? '' : undefined}
            onClick={() => handleSelectRow('user')}
          >
            <span className="history-screen__row-label">
              {sessionDisplayName ?? t('navShell.history.userRowLabel')}
            </span>
          </button>
          {expandedRow === 'user' && (
            <div className="history-screen__row-actions">
              <button type="button" onClick={() => setActionContent({ type: 'facts' })}>
                {t('navShell.history.factsAction')}
              </button>
            </div>
          )}
        </li>

        {showGroupRow && (
          <li className="history-screen__row">
            <button
              type="button"
              className="history-screen__row-header"
              data-selected={expandedRow === 'group' ? '' : undefined}
              onClick={() => handleSelectRow('group')}
            >
              <span className="history-screen__row-label">
                {groupIds.length === 1
                  ? t('navShell.history.groupRowLabel')
                  : t('navShell.history.groupRowLabelCount', { count: groupIds.length })}
              </span>
            </button>
            {expandedRow === 'group' && (
              <div className="history-screen__row-actions">
                <button type="button" onClick={() => setActionContent({ type: 'facts' })}>
                  {t('navShell.history.factsAction')}
                </button>
              </div>
            )}
          </li>
        )}

        {personas.map((persona) => {
          const rowId: RowId = `persona:${persona.id}`
          return (
            <li key={persona.id} className="history-screen__row">
              <button
                type="button"
                className="history-screen__row-header"
                data-selected={expandedRow === rowId ? '' : undefined}
                onClick={() => handleSelectRow(rowId)}
              >
                <span className="history-screen__row-label">{persona.display_name}</span>
              </button>
              {expandedRow === rowId && (
                <div className="history-screen__row-actions">
                  <button type="button" onClick={() => setActionContent({ type: 'facts' })}>
                    {t('navShell.history.factsAction')}
                  </button>
                  <button
                    type="button"
                    onClick={() =>
                      setActionContent({ type: 'chatHistory', personaId: persona.id })
                    }
                  >
                    {t('navShell.history.chatHistoryAction')}
                  </button>
                  <button
                    type="button"
                    onClick={() =>
                      setActionContent({ type: 'libraryPreview', personaId: persona.id })
                    }
                  >
                    {t('navShell.history.libraryAction')}
                  </button>
                  <button
                    type="button"
                    onClick={() => onOpenPersonaChat(persona.id, null)}
                  >
                    {t('navShell.history.chatAction')}
                  </button>
                </div>
              )}
            </li>
          )
        })}
      </ul>

      {actionContent && (
        <div className="history-screen__action-pane">
          {actionContent.type === 'facts' && <p>{t('navShell.content.myFactsPlaceholder')}</p>}
          {actionContent.type === 'chatHistory' && (
            <ChatHistoryAction
              userId={userId}
              personaId={actionContent.personaId}
              onResumeChat={(chat) => onOpenPersonaChat(actionContent.personaId, chat)}
            />
          )}
          {actionContent.type === 'libraryPreview' && (
            <LibraryPreviewAction
              userId={userId}
              personaId={actionContent.personaId}
              onOpenFullLibrary={() => onOpenFullLibrary(actionContent.personaId)}
            />
          )}
        </div>
      )}
    </div>
  )
}

function ChatHistoryAction({
  userId,
  personaId,
  onResumeChat,
}: {
  userId: string
  personaId: string
  onResumeChat: (chat: ChatInfo) => void
}) {
  const { t } = useTranslation()
  const [chats, setChats] = useState<ChatInfo[]>([])
  const [error, setError] = useState<string | null>(null)

  const fetchChats = useCallback(() => {
    commands.listChats(userId, personaId).then((result) => {
      if (result.status === 'ok') {
        setChats(result.data)
      } else {
        setError(result.error)
      }
    })
  }, [userId, personaId])

  useEffect(() => {
    setChats([])
    setError(null)
    fetchChats()
  }, [fetchChats])

  // items.id=546: the effect above only fetches on mount/persona change --
  // a chat updated elsewhere (a message sent while this panel is already
  // open) wouldn't otherwise show until this component remounted. Same
  // cancelled/unlisten cleanup idiom as ChatPane.tsx's own
  // run-status-update/message-content-ready effect (CLAUDE.md: Tauri event
  // listeners must be explicitly detached on SPA view unmount).
  useEffect(() => {
    let unlisten: UnlistenFn | undefined
    let cancelled = false

    listen<ChatActivityUpdatedPayload>('chat-activity-updated', (event) => {
      if (event.payload.persona_id === personaId) {
        fetchChats()
      }
    }).then((fn) => {
      if (cancelled) {
        fn()
      } else {
        unlisten = fn
      }
    })

    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [personaId, fetchChats])

  return (
    <div className="history-screen__chat-history">
      {error && (
        <p role="alert">{t('navShell.chatHistoryList.loadError', { message: error })}</p>
      )}
      {chats.length === 0 && !error && (
        <p className="history-screen__empty">{t('navShell.chatHistoryList.empty')}</p>
      )}
      {chats.length > 0 && (
        <ul className="chat-history-list__items">
          {chats.map((chat) => (
            <li key={chat.id}>
              <button
                type="button"
                className="chat-history-list__item"
                onClick={() => onResumeChat(chat)}
              >
                <span className="chat-history-list__item-title">
                  {chat.title ?? t('navShell.chatHistoryList.untitled')}
                </span>
                <span className="chat-history-list__item-time">
                  {new Date(chat.last_message_at).toLocaleString()}
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function LibraryPreviewAction({
  userId,
  personaId,
  onOpenFullLibrary,
}: {
  userId: string
  personaId: string
  onOpenFullLibrary: () => void
}) {
  const { t } = useTranslation()
  const [outputs, setOutputs] = useState<OutputInfo[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    setOutputs([])
    setError(null)
    commands.listOutputs(userId, personaId, null, null, null, null).then((result) => {
      if (result.status === 'ok') {
        setOutputs(result.data)
      } else {
        setError(result.error)
      }
    })
  }, [userId, personaId])

  return (
    <div className="history-screen__library-preview">
      {error && (
        <p role="alert">{t('navShell.libraryPane.listLoadError', { message: error })}</p>
      )}
      {outputs.length === 0 && !error && (
        <p className="history-screen__empty">{t('navShell.libraryPane.emptyList')}</p>
      )}
      {outputs.length > 0 && (
        <ul className="history-screen__library-preview-scroll">
          {outputs.map((output) => (
            <li key={output.id}>
              <DocumentRow output={output} interactive={false} />
            </li>
          ))}
        </ul>
      )}
      <div className="history-screen__library-preview-footer">
        <button
          type="button"
          className="history-screen__link-button"
          onClick={onOpenFullLibrary}
        >
          {t('navShell.history.openFullLibrary')}
        </button>
      </div>
    </div>
  )
}
