// Persona-scoped chat history -- items.id=384 slice 7 (decisions.id=739).
// A toggleable list of a Persona's past chats (commands.listChats),
// click-to-switch. Archived chats are excluded server-side (chat_store::
// list_chats' own WHERE clause) -- no "show archived" affordance exists
// in this build; decisions.id=739 only specifies archiving as a
// lifecycle transition, not a browsing UI for it.
//
// Does NOT include the pre-existing flat "tier3-access-{personaId}"
// conversation that predates this table -- that context_key has no
// `chats` row (messages_002.sql deliberately doesn't backfill one, see
// its own header comment), so it simply isn't something list_chats can
// return. It's still reachable as Tier3AccessPane's own default view
// (activeChat === null) -- this list only ever shows real chats.id rows.

import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type ChatInfo } from '../bindings'
import './ChatHistoryList.css'

export interface ChatHistoryListProps {
  userId: string
  personaId: string
  activeChatId: string | null
  onSelectChat: (chat: ChatInfo) => void
  /** items.id=384 slice 7: disabled while a Gate3 review is pending --
   *  switching the visible transcript mid-review would leave
   *  Tier3AccessPane's own pending-review state pointed at a message
   *  that's no longer the one on screen. Tier3AccessPane is the only
   *  caller and is the one that knows reviewOutcome, so it passes this
   *  through rather than this component needing to know about Gate3 at
   *  all. */
  disabled?: boolean
}

export function ChatHistoryList({
  userId,
  personaId,
  activeChatId,
  onSelectChat,
  disabled = false,
}: ChatHistoryListProps) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const [chats, setChats] = useState<ChatInfo[]>([])
  const [error, setError] = useState<string | null>(null)

  // Re-fetch every time the panel opens rather than once on mount --
  // cheap, and keeps the list honest after a create_chat/archive_chat
  // elsewhere without needing a shared cache or a push event for
  // something this low-frequency.
  useEffect(() => {
    if (!open) return
    setError(null)
    commands.listChats(userId, personaId).then((result) => {
      if (result.status === 'ok') {
        setChats(result.data)
      } else {
        setError(result.error)
      }
    })
  }, [open, userId, personaId])

  return (
    <div className="chat-history-list">
      <button
        type="button"
        className="chat-history-list__toggle"
        onClick={() => setOpen((o) => !o)}
        disabled={disabled}
        aria-expanded={open}
        title={t('navShell.chatHistoryList.toggleLabel')}
      >
        {t('navShell.chatHistoryList.toggleLabel')}
      </button>
      {open && (
        <div className="chat-history-list__panel" role="listbox">
          {error && (
            <p role="alert">{t('navShell.chatHistoryList.loadError', { message: error })}</p>
          )}
          {chats.length === 0 && !error && (
            <p className="chat-history-list__empty">{t('navShell.chatHistoryList.empty')}</p>
          )}
          <ul className="chat-history-list__items">
            {chats.map((chat) => (
              <li key={chat.id}>
                <button
                  type="button"
                  className="chat-history-list__item"
                  data-selected={chat.id === activeChatId ? '' : undefined}
                  onClick={() => {
                    onSelectChat(chat)
                    setOpen(false)
                  }}
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
        </div>
      )}
    </div>
  )
}
