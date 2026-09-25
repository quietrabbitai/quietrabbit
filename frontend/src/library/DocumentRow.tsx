// A single Output/document row -- LibraryPane.tsx's own row-stack
// (items.id=404, interactive: selecting a row reveals its [View]/[Copy]
// actions). Previously also shared with HistoryScreen.tsx's Persona-row
// Library preview (glance-only, per the Working/LIBRARY_ROW_INTEGRATION_
// MOCKUP_20260902.html mockup -- "Open full Library" was the escape hatch
// for anything beyond glancing, not these rows themselves). items.id=568
// (decisions.id=834) removed that preview and its escape hatch --
// HistoryScreen.tsx now mounts LibraryPane directly instead, so
// LibraryPane.tsx is this component's only caller.

import type { TFunction } from 'i18next'
import { useTranslation } from 'react-i18next'
import type { OutputInfo } from '../bindings'
import './DocumentRow.css'

// decisions.id=422: the five document_relationship values on the wire --
// plain string on OutputInfo, this local union just catches typos in the
// literal comparisons below and in LibraryPane.tsx's lineage-fetch hook.
export type DocumentRelationship = 'prime' | 'update' | 'fork' | 'reference' | 'continue_draft'

// Shared with LibraryPane.tsx's lineage link labels so a jump target's
// display name matches exactly what a row shows for itself.
export function getDocumentDisplayName(output: OutputInfo, t: TFunction): string {
  return (
    output.original_filename ??
    t('navShell.libraryPane.itemLabel', {
      type: output.output_type,
      date: output.created_at,
    })
  )
}

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
  const name = getDocumentDisplayName(output, t)
  const date = new Date(output.created_at).toLocaleString()
  const relationship = output.document_relationship as DocumentRelationship

  // items.id=573: relationship badge -- never shown for prime (default,
  // uninteresting) or continue_draft (same row before/after, nothing
  // relational to display per DOCUMENTROW_LINEAGE_DESIGN_20260924.md's
  // Transitions section).
  const relationshipBadge =
    relationship === 'update'
      ? { label: t('navShell.libraryPane.relationshipBadgeUpdate'), tooltip: t('navShell.libraryPane.relationshipBadgeUpdateTooltip') }
      : relationship === 'fork'
        ? { label: t('navShell.libraryPane.relationshipBadgeFork'), tooltip: t('navShell.libraryPane.relationshipBadgeForkTooltip') }
        : relationship === 'reference'
          ? { label: t('navShell.libraryPane.relationshipBadgeReference'), tooltip: t('navShell.libraryPane.relationshipBadgeReferenceTooltip') }
          : null

  // Independent of relationship type (design doc: "a distinct piece of
  // state" from document_relationship) -- but continue_draft still shows
  // nothing, matching "NO lineage UI at all" for that type.
  const showSupersededBadge = output.superseded_by !== null && relationship !== 'continue_draft'
  const exportedTooltip = output.exported_at
    ? t('navShell.libraryPane.exportedTooltip', {
        when: new Date(output.exported_at).toLocaleString(),
      })
    : null

  const dateGroup = (
    <span className="document-row__date-group">
      {relationshipBadge && (
        <span className="document-row__lineage-badge" title={relationshipBadge.tooltip}>
          {relationshipBadge.label}
        </span>
      )}
      {showSupersededBadge && (
        <span
          className="document-row__lineage-badge"
          title={t('navShell.libraryPane.supersededBadgeTooltip')}
        >
          {t('navShell.libraryPane.supersededBadge')}
        </span>
      )}
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
