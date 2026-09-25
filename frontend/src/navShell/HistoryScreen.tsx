// The consolidated Library screen -- items.id=568 (decisions.id=834),
// reworking items.id=404's row-stack + action-pane History screen into a
// flat, always-visible button frame: PersonaPillRow (Library's own
// local-filter variant, per decisions.id=833) at top, then Documents /
// Facts / Chat History / User Info / Group Info as sibling buttons,
// Documents pre-selected on entry. The file/component/prop names
// (HistoryScreen, HistoryScreenProps, HistoryOpenTarget) are deliberately
// NOT renamed to match -- only the user-facing label changes (WorkspaceShell
// now shows this rail as "Library", reusing the navShell.library string
// freed up once the old separate Library rail was deleted). Renaming the
// internal identifiers too would be a much larger mechanical diff than this
// item's scope calls for; the design doc itself treats that as a separate,
// non-blocking documentation-cleanup flag, not a build requirement.
//
// Absorbs what the old row-stack absorbed (Chat History, My Facts) plus
// what used to be a separate Library rail entirely: Documents is now this
// screen's own persona-scoped Library view (LibraryPane, mounted directly --
// no more "preview + Open full Library escape hatch", since there's no
// second screen to escape to any more).
//
// One PersonaPillRow now governs every view (Documents/Facts/Chat History/
// User Info/Group Info alike) -- previously LibraryPane owned its own copy
// of this selector just for Documents. User Info's content (the session
// display name) is NOT persona-scoped -- it's account/session-level, same
// as the old User row's label always was. Group Info's underlying
// listPersonaGroupIds fetch IS keyed off this screen's own persona
// selection now (previously keyed off the shared, app-wide activePersonaId)
// -- Jason, 2026-09-24: consistent with every other view here using the
// local filter, not the app-wide one.

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { commands, type ChatInfo, type PersonaInfo } from '../bindings'
import { LibraryPane } from '../library/LibraryPane'
import { PersonaPillRow } from './persona/PersonaPillRow'
import { usePersonaLocalFilter } from './persona/usePersonaLocalFilter'
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
  /** The shared, app-wide active persona (NavState.activePersonaId) --
   *  read only here: drives PersonaPillRow's ring mark and this screen's
   *  own local filter's first-open default. This screen's own selection
   *  never writes back to it (decisions.id=833). */
  activePersonaId: string | null
  /** Chat History's "resume this chat" row-action. */
  onOpenPersonaChat: (personaId: string, chat: ChatInfo) => void
  /** Chat's own "Chat history" toggle jumping in from outside -- consumed
   *  once (see onOpenTargetConsumed). */
  openTarget: HistoryOpenTarget | null
  onOpenTargetConsumed: () => void
}

type LibraryView = 'documents' | 'facts' | 'chatHistory' | 'userInfo' | 'groupInfo'

export function HistoryScreen({
  userId,
  personas,
  activePersonaId,
  onOpenPersonaChat,
  openTarget,
  onOpenTargetConsumed,
}: HistoryScreenProps) {
  const { t } = useTranslation()
  const { filterId: libraryFilter, select: selectLibraryFilter, setFilter: setLibraryFilter } =
    usePersonaLocalFilter(activePersonaId, false)
  const [view, setView] = useState<LibraryView>('documents')
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
  // as "no groups" (hide the button) -- this is a visibility check, not a
  // page a user is trying to use, so there's nothing useful an error
  // banner here would let them do differently.
  useEffect(() => {
    setGroupIds([])
    if (libraryFilter === null) return
    commands.listPersonaGroupIds(libraryFilter).then((result) => {
      if (result.status === 'ok') setGroupIds(result.data)
    })
  }, [libraryFilter])

  // items.id=558 precedent (LibraryPane's own persona-switch reset, now
  // relocated here alongside the selector itself): switching Persona always
  // lands back on the default Documents view, so a Facts/Chat History/Group
  // Info selection from one Persona doesn't silently carry over to the next.
  useEffect(() => {
    setView('documents')
  }, [libraryFilter])

  // Chat's "Chat history" toggle jumping in -- forces this screen's own
  // persona selection to the target (via setFilter, not select: see
  // usePersonaLocalFilter.ts's header comment on why the toggle-to-fallback
  // select() would be wrong here) and pre-selects Chat History as the
  // showing view, matching the design doc's transition description verbatim
  // ("'Chat History' already showing in the action-pane").
  useEffect(() => {
    if (!openTarget) return
    setLibraryFilter(openTarget.personaId)
    setView('chatHistory')
    onOpenTargetConsumed()
  }, [openTarget, onOpenTargetConsumed, setLibraryFilter])

  const showGroupInfo = groupIds.length > 0

  return (
    <div className="history-screen">
      <PersonaPillRow
        personas={personas}
        activePersonaId={activePersonaId}
        filterId={libraryFilter}
        onSelect={selectLibraryFilter}
        allowAll={false}
      />

      <div className="history-screen__view-buttons">
        <button
          type="button"
          className="history-screen__view-button"
          data-selected={view === 'documents' ? '' : undefined}
          onClick={() => setView('documents')}
        >
          {t('navShell.history.documentsAction')}
        </button>
        <button
          type="button"
          className="history-screen__view-button"
          data-selected={view === 'facts' ? '' : undefined}
          onClick={() => setView('facts')}
        >
          {t('navShell.history.factsAction')}
        </button>
        <button
          type="button"
          className="history-screen__view-button"
          data-selected={view === 'chatHistory' ? '' : undefined}
          onClick={() => setView('chatHistory')}
        >
          {t('navShell.history.chatHistoryAction')}
        </button>
        <button
          type="button"
          className="history-screen__view-button"
          data-selected={view === 'userInfo' ? '' : undefined}
          onClick={() => setView('userInfo')}
        >
          {t('navShell.history.userInfoAction')}
        </button>
        {showGroupInfo && (
          <button
            type="button"
            className="history-screen__view-button"
            data-selected={view === 'groupInfo' ? '' : undefined}
            onClick={() => setView('groupInfo')}
          >
            {t('navShell.history.groupInfoAction')}
          </button>
        )}
      </div>

      <div className="history-screen__content-pane">
        {view === 'documents' &&
          (libraryFilter === null ? (
            <p className="history-screen__notice">{t('navShell.history.noPersonaSelected')}</p>
          ) : (
            <LibraryPane userId={userId} personaId={libraryFilter} />
          ))}
        {view === 'facts' &&
          (libraryFilter === null ? (
            <p className="history-screen__notice">{t('navShell.history.noPersonaSelected')}</p>
          ) : (
            <p>{t('navShell.content.myFactsPlaceholder')}</p>
          ))}
        {view === 'chatHistory' &&
          (libraryFilter === null ? (
            <p className="history-screen__notice">{t('navShell.history.noPersonaSelected')}</p>
          ) : (
            <ChatHistoryAction
              userId={userId}
              personaId={libraryFilter}
              onResumeChat={(chat) => onOpenPersonaChat(libraryFilter, chat)}
            />
          ))}
        {view === 'userInfo' && (
          <p>{sessionDisplayName ?? t('navShell.history.userRowLabel')}</p>
        )}
        {view === 'groupInfo' && showGroupInfo && (
          <p>
            {groupIds.length === 1
              ? t('navShell.history.groupRowLabel')
              : t('navShell.history.groupRowLabelCount', { count: groupIds.length })}
          </p>
        )}
      </div>
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
