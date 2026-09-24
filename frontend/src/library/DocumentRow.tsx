// A single Output/document row -- shared between LibraryPane.tsx's own
// row-stack (items.id=404, interactive: selecting a row reveals its
// [View]/[Copy] actions) and HistoryScreen.tsx's Persona-row Library
// preview (glance-only, per the Working/LIBRARY_ROW_INTEGRATION_MOCKUP_
// 20260902.html mockup -- "Open full Library" is the escape hatch for
// anything beyond glancing, not these rows themselves). One component so
// the two renderings can't visually drift apart, matching this codebase's
// existing discipline of importing a shared stylesheet rather than
// duplicating it (e.g. WorkspaceShell.tsx / CloudChatCollapsedStrip.css).

import { useTranslation } from 'react-i18next'
import type { OutputInfo } from '../bindings'
import './DocumentRow.css'

// Option C (decisions.id=827): small glyph beside the date, full
// "Exported: [date][time]" text lives in the title/aria-label tooltip only.
function ExportedGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="10" height="10" aria-hidden="true">
      <path
        d="M8 2.4v7.3M8 2.4 5.3 5.1M8 2.4l2.7 2.7"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M3 10.6v1.3c0 .7.6 1.3 1.3 1.3h7.4c.7 0 1.3-.6 1.3-1.3v-1.3"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </svg>
  )
}

export interface DocumentRowProps {
  output: OutputInfo
  /** false for History's glance-only preview rows -- no click behavior,
   *  renders as a plain div instead of a button. */
  interactive?: boolean
  selected?: boolean
  onSelect?: () => void
}

export function DocumentRow({
  output,
  interactive = true,
  selected = false,
  onSelect,
}: DocumentRowProps) {
  const { t } = useTranslation()
  // original_filename is null for QR-generated (non-ingested) outputs --
  // falls back to the same type+date label LibraryPane's list already
  // used before this rework.
  const name =
    output.original_filename ??
    t('navShell.libraryPane.itemLabel', {
      type: output.output_type,
      date: output.created_at,
    })
  const date = new Date(output.created_at).toLocaleString()
  const exportedTooltip = output.exported_at
    ? t('navShell.libraryPane.exportedTooltip', {
        when: new Date(output.exported_at).toLocaleString(),
      })
    : null

  const dateGroup = (
    <span className="document-row__date-group">
      <span className="document-row__date">{date}</span>
      {exportedTooltip && (
        <span
          className="document-row__exported-mark"
          title={exportedTooltip}
          aria-hidden="true"
        >
          <ExportedGlyph />
        </span>
      )}
    </span>
  )

  if (!interactive) {
    return (
      <div className="document-row">
        <span className="document-row__name">{name}</span>
        {dateGroup}
      </div>
    )
  }

  return (
    <button
      type="button"
      className="document-row document-row--interactive"
      data-selected={selected ? '' : undefined}
      onClick={onSelect}
    >
      <span className="document-row__name">{name}</span>
      {dateGroup}
    </button>
  )
}
