// A single Output/document row -- shared between LibraryPane.tsx's own
// row-stack (items.id=404, interactive: selecting a row reveals its
// [View]/[Copy] actions) and HistoryScreen.tsx's Persona-row Library
// preview (glance-only, per the Working/LIBRARY_ROW_INTEGRATION_MOCKUP_
// 20260902.html mockup -- "Open full Library" is the escape hatch for
// anything beyond glancing, not these rows themselves). One component so
// the two renderings can't visually drift apart, matching this codebase's
// existing discipline of importing a shared stylesheet rather than
// duplicating it (e.g. WorkspaceShell.tsx / Tier3CollapsedStrip.css).

import { useTranslation } from 'react-i18next'
import type { OutputInfo } from '../bindings'
import './DocumentRow.css'

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

  if (!interactive) {
    return (
      <div className="document-row">
        <span className="document-row__name">{name}</span>
        <span className="document-row__date">{date}</span>
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
      <span className="document-row__date">{date}</span>
    </button>
  )
}
