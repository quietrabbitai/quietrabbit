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
  /** items.id=384 slice 7: disabled while a Gate3 review is pending --
   *  switching screens mid-review would leave Tier3AccessPane's own
   *  pending-review state pointed at a message that's no longer visible.
   *  Tier3AccessPane is the only caller and is the one that knows
   *  reviewOutcome, so it passes this through rather than this component
   *  needing to know about Gate3 at all. */
  disabled?: boolean
}

export function ChatHistoryList({ onOpenHistory, disabled = false }: ChatHistoryListProps) {
  const { t } = useTranslation()
  return (
    <button
      type="button"
      className="chat-history-list__toggle"
      onClick={onOpenHistory}
      disabled={disabled}
      title={t('navShell.chatHistoryList.toggleLabel')}
    >
      {t('navShell.chatHistoryList.toggleLabel')}
    </button>
  )
}
