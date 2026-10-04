// Chat's "Chat history" toggle -- items.id=384 slice 7 (decisions.id=739),
// reworked for items.id=404: this used to be a self-fetching dropdown
// (listChats + a <ul> panel, click-to-switch inline). The History screen
// (HistoryScreen.tsx) now owns that list and its resume-a-chat action, as
// one of a Persona row's action-pane contents -- this component is just
// the toggle button, calling onOpenHistory to make the History rail
// dominant with the current Persona pre-selected (see the design doc's
// "Chat's existing 'Chat history' toggle -> repurpose to make History
// dominant instead of its current dropdown" transition).

import { useTranslation } from 'react-i18next'
import './ChatHistoryList.css'

export interface ChatHistoryListProps {
  onOpenHistory: () => void
}

export function ChatHistoryList({ onOpenHistory }: ChatHistoryListProps) {
  const { t } = useTranslation()
  return (
    <button
      type="button"
      className="chat-history-list__toggle"
      onClick={onOpenHistory}
      title={t('navShell.chatHistoryList.toggleLabel')}
    >
      {t('navShell.chatHistoryList.toggleLabel')}
    </button>
  )
}
