// Real Library output-viewer -- items.id=404 reworked this from a bare
// list -> full-pane-detail view (a click replaced the whole list with
// LibraryOutputDetail, Back button to return) into the row-stack/
// action-pane pattern History (HistoryScreen.tsx) uses: rows stay listed,
// selecting one reveals its own [View]/[Copy] actions inline, and a
// bottom action-pane (not a full-pane swap) shows whichever action was
// clicked -- "adapted for documents" per the design doc's own framing of
// Library's internal rework (items.id=397(a)). The mockup
// (Working/LIBRARY_ROW_INTEGRATION_MOCKUP_20260902.html) only shows this
// pattern applied to identity-hierarchy rows, not documents -- this
// mapping (row = one document, actions = View/Copy, matching the two real
// actions this screen already had) is this item's own extrapolation of
// "same pattern" onto Library's existing two actions, not something the
// mockup itself specifies.
//
// items.id=568 (decisions.id=834): this component no longer owns its own
// persona selection. The consolidated Library screen (HistoryScreen.tsx)
// now mounts one PersonaPillRow (Library's own local-filter variant, per
// decisions.id=833) governing every one of its views, not just Documents
// -- so that selector moved up to the parent. This component just takes
// the already-resolved `personaId` as a prop; the parent only mounts it
// once a persona is actually selected, so there's no null case to handle
// here any more. (Previously: items.id=543 gave this component its own
// local persona filter, decoupled from activePersonaId, specifically so
// browsing Library couldn't silently switch/lose the user's open Chat
// conversation -- that reasoning is unchanged, it's just enforced one
// level up now.)

import { useCallback, useEffect, useState } from 'react'
import { open } from '@tauri-apps/plugin-dialog'
import { useTranslation } from 'react-i18next'
import { commands, type OutputInfo } from '../bindings'
import { DocumentRow, getDocumentDisplayName } from './DocumentRow'
import './LibraryPane.css'

export interface LibraryPaneProps {
  userId: string
  /** The persona this Documents view is scoped to -- resolved by the
   *  parent Library screen's own PersonaPillRow selection. The parent
   *  never mounts this component without one selected. */
  personaId: string
}

type LibraryAction = 'view' | 'copy' | 'import' | null

// items.id=558: the two Library views -- qr_generated is the existing
// default, external_ingested is the new Imported view. Mirrors
// OutputInfo.source's own two values (items.id=383) exactly, so this type
// is passed straight through to listOutputs' `source` param.
type LibrarySourceView = 'qr_generated' | 'external_ingested'

// items.id=573 -- DocumentRow lineage display + cross-row navigation,
// per DOCUMENTROW_LINEAGE_DESIGN_20260924.md's Transitions section.
interface DocumentLineage {
  backward: OutputInfo | null
  forward: OutputInfo[]
}

// Backward-link semantics differ by relationship type and are NOT
// symmetric in the backend: get_predecessor only answers the `update`
// case (reverse lookup on superseded_by). `fork`'s backward link is a
// direct field read -- parent_output_id is already on the row, fetched
// via a plain getOutput call rather than get_predecessor. `prime`/
// `reference` never have a backward link. `continue_draft` gets no
// lineage UI at all (same row before/after -- nothing relational to
// display), so it skips fetching entirely rather than just hiding the
// result.
function useDocumentLineage(
  output: OutputInfo | null,
  userId: string,
  personaId: string,
): DocumentLineage {
  const [lineage, setLineage] = useState<DocumentLineage>({ backward: null, forward: [] })

  useEffect(() => {
    if (output === null || output.document_relationship === 'continue_draft') {
      setLineage({ backward: null, forward: [] })
      return
    }

    let cancelled = false

    const backward: Promise<OutputInfo | null> =
      output.document_relationship === 'fork' && output.parent_output_id !== null
        ? commands
            .getOutput(output.parent_output_id, userId, personaId)
            .then((r) => (r.status === 'ok' ? r.data : null))
        : output.document_relationship === 'update'
          ? commands
              .getPredecessor(output.id, userId, personaId)
              .then((r) => (r.status === 'ok' ? r.data : null))
          : Promise.resolve(null)

    const forward = commands
      .listForwardLinks(output.id, userId, personaId)
      .then((r) => (r.status === 'ok' ? r.data : []))

    Promise.all([backward, forward]).then(([b, f]) => {
      if (!cancelled) setLineage({ backward: b, forward: f })
    })

    return () => {
      cancelled = true
    }
  }, [output, userId, personaId])

  return lineage
}

// Shared by both navigation surfaces (row-actions inline area and the
// document header shown while View is open) -- renders nothing if there's
// nothing to show, matching the "earn its place" framing for a row with no
// real lineage. `openView` distinguishes Surface 1 (select-in-place) from
// Surface 2 (jump-into-View) per the design doc's Transitions section.
function LineageLinks({
  lineage,
  onNavigate,
  openView,
}: {
  lineage: DocumentLineage
  onNavigate: (target: OutputInfo, openView: boolean) => void
  openView: boolean
}) {
  const { t } = useTranslation()

  if (lineage.backward === null && lineage.forward.length === 0) return null

  return (
    <>
      {lineage.backward && (
        <button
          type="button"
          className="library-pane__lineage-link"
          onClick={() => onNavigate(lineage.backward as OutputInfo, openView)}
        >
          {t('navShell.libraryPane.lineageBackwardLink', {
            name: getDocumentDisplayName(lineage.backward, t),
          })}
        </button>
      )}
      {lineage.forward.map((target) => (
        <button
          key={target.id}
          type="button"
          className="library-pane__lineage-link"
          onClick={() => onNavigate(target, openView)}
        >
          {t('navShell.libraryPane.lineageForwardLink', {
            name: getDocumentDisplayName(target, t),
          })}
        </button>
      ))}
    </>
  )
}

export function LibraryPane({ userId, personaId }: LibraryPaneProps) {
  const { t } = useTranslation()
  const [sourceView, setSourceView] = useState<LibrarySourceView>('qr_generated')
  const [otherSourceHasDocs, setOtherSourceHasDocs] = useState(false)
  const [outputs, setOutputs] = useState<OutputInfo[]>([])
  const [listError, setListError] = useState<string | null>(null)
  const [selectedOutputId, setSelectedOutputId] = useState<string | null>(null)
  const [action, setAction] = useState<LibraryAction>(null)
  const [viewedOutput, setViewedOutput] = useState<OutputInfo | null>(null)
  const [viewError, setViewError] = useState<string | null>(null)
  const [copyStatus, setCopyStatus] = useState<'pending' | 'success' | 'error' | null>(null)
  const [copyError, setCopyError] = useState<string | null>(null)
  const [importStatus, setImportStatus] = useState<'pending' | 'success' | 'error' | null>(null)
  const [importError, setImportError] = useState<string | null>(null)

  // items.id=573: set when a lineage link jumps across the qr_generated/
  // external_ingested toggle -- the sourceView-change effect below resets
  // selection and re-fetches `outputs` from scratch, so the actual
  // selection can't happen until the target is confirmed present in that
  // fresh list (see the effect further down).
  const [pendingLineageTarget, setPendingLineageTarget] = useState<{
    outputId: string
    openView: boolean
  } | null>(null)

  const resetSelection = useCallback(() => {
    setSelectedOutputId(null)
    setAction(null)
    setViewedOutput(null)
    setViewError(null)
    setCopyStatus(null)
    setCopyError(null)
    setImportStatus(null)
    setImportError(null)
  }, [])

  // items.id=558: switching Persona always lands back on the default
  // qr_generated view, per the design's own "resetting to the default
  // qr_generated view on persona switch" -- an Imported-view selection
  // from one Persona shouldn't silently carry over to the next.
  useEffect(() => {
    setSourceView('qr_generated')
  }, [userId, personaId])

  // Re-fetch on mount / identity / source-view change, mirrors ChatPane.tsx's
  // own identity-keyed re-fetch effect.
  useEffect(() => {
    setOutputs([])
    setListError(null)
    resetSelection()

    // items.id=404 / items.id=558: passes all 6 params explicitly. `source`
    // is sourceView itself now rather than a hardcoded null -- list_outputs
    // treats a missing source as 'qr_generated' anyway, but making the
    // param explicit keeps this call site honest about which of the two
    // views it's asking for.
    commands.listOutputs(userId, personaId, null, null, null, sourceView).then((result) => {
      if (result.status === 'ok') {
        setOutputs(result.data)
      } else {
        setListError(result.error)
      }
    })
  }, [userId, personaId, sourceView, resetSelection])

  // items.id=558: probes the *other* source so the "View imported
  // documents ->" / "View library ->" entry point can stay hidden unless
  // it actually leads somewhere.
  useEffect(() => {
    setOtherSourceHasDocs(false)

    const otherSource: LibrarySourceView =
      sourceView === 'qr_generated' ? 'external_ingested' : 'qr_generated'
    commands.listOutputs(userId, personaId, null, null, null, otherSource).then((result) => {
      if (result.status === 'ok') {
        setOtherSourceHasDocs(result.data.length > 0)
      }
    })
  }, [userId, personaId, sourceView])

  // View action -- re-fetches via getOutput rather than trusting the list
  // row's own cached content, matching ChatPane's reconcile-over-cache
  // discipline and giving Privacy Guardian a fresh per-access check point.
  useEffect(() => {
    if (action !== 'view' || selectedOutputId === null) return
    setViewedOutput(null)
    setViewError(null)
    commands.getOutput(selectedOutputId, userId, personaId).then((result) => {
      if (result.status === 'ok') {
        setViewedOutput(result.data)
      } else {
        setViewError(result.error)
      }
    })
  }, [action, selectedOutputId, userId, personaId])

  // items.id=573, Surface 1 -- inline actions area: computed for whichever
  // row is selected, regardless of which action (if any) is currently open.
  const selectedOutput = outputs.find((o) => o.id === selectedOutputId) ?? null
  const selectedLineage = useDocumentLineage(selectedOutput, userId, personaId)

  // items.id=573, Surface 2 -- document header: computed for the row
  // actually being viewed, gated on View being open so Copy/Import don't
  // trigger a wasted lineage fetch.
  const viewedLineage = useDocumentLineage(
    action === 'view' ? viewedOutput : null,
    userId,
    personaId,
  )

  // Shared by handleSelectRow-equivalent lineage jumps (Surface 1: openView
  // false, mimics handleSelectRow exactly; Surface 2: openView true, jumps
  // straight into View) and by the cross-sourceView completion effect below.
  const applyLineageSelection = useCallback((outputId: string, openView: boolean) => {
    setSelectedOutputId(outputId)
    setAction(openView ? 'view' : null)
    setViewedOutput(null)
    setViewError(null)
    setCopyStatus(null)
    setCopyError(null)
    setImportStatus(null)
    setImportError(null)
  }, [])

  // Completes a cross-sourceView lineage jump once the target is confirmed
  // present in the freshly-loaded `outputs` list (see navigateToLineageTarget
  // below for why this can't happen in the same tick as setSourceView).
  useEffect(() => {
    if (!pendingLineageTarget) return
    const match = outputs.find((o) => o.id === pendingLineageTarget.outputId)
    if (!match) return
    applyLineageSelection(match.id, pendingLineageTarget.openView)
    setPendingLineageTarget(null)
  }, [outputs, pendingLineageTarget, applyLineageSelection])

  const handleCopy = useCallback(() => {
    if (selectedOutputId === null) return
    setAction('copy')
    setCopyStatus('pending')
    setCopyError(null)
    commands.copyOutputToClipboard(selectedOutputId, userId, personaId).then((result) => {
      if (result.status === 'ok') {
        setCopyStatus('success')
      } else {
        // Already the actionable, clipboard-specific message -- rendered
        // verbatim, not wrapped in a generic error template.
        setCopyStatus('error')
        setCopyError(result.error)
      }
    })
  }, [selectedOutputId, userId, personaId])

  // Import new version (decisions.id=827/items.id=565): the native picker
  // itself is the only gate -- no extra confirmation dialog, and no
  // sensitivity/focus picker either. decisions.id=827 confirmed the button
  // only renders when a document is selected (enforced again below in JSX),
  // so focus_slug/sensitivity always have a source to inherit from -- there's
  // no unselected case for a picker to disambiguate. Every picked file is
  // passed to store_ingested_document as file_path (never content, unlike
  // the old hidden-input/File.text() flow this replaces) -- the backend
  // already mirrors text extensions and real-extracts PDF/.docx
  // (items.id=386) uniformly off the file's own extension regardless of
  // which parameter supplied it, so no extension allowlist is needed
  // client-side any more. This also fixes original_filename for text files:
  // the content path always hardcoded "pasted-content.txt".
  const handleImportClick = useCallback(() => {
    if (selectedOutputId === null) return

    open({ multiple: false, directory: false })
      .then((path) => {
        if (path === null) return // dialog cancelled

        const selected = outputs.find((o) => o.id === selectedOutputId)
        const focusSlug = selected?.focus_slug ?? null

        setAction('import')
        setImportStatus('pending')
        setImportError(null)

        if (!selected || focusSlug === null) {
          setImportStatus('error')
          setImportError(t('navShell.libraryPane.importMissingFocusError'))
          return
        }

        return commands
          .storeIngestedDocument(
            userId,
            personaId,
            focusSlug,
            selected.project_entity_id,
            selected.sensitivity,
            null,
            path,
          )
          .then((storeResult) => {
            if (storeResult.status !== 'ok') {
              setImportStatus('error')
              setImportError(storeResult.error)
              return
            }
            return commands
              .updateActiveDocument(storeResult.data.output_id, selectedOutputId, userId, personaId)
              .then((linkResult) => {
                if (linkResult.status !== 'ok') {
                  setImportStatus('error')
                  setImportError(linkResult.error)
                  return
                }
                setImportStatus('success')
                commands.listOutputs(userId, personaId, null, null, null, sourceView).then((result) => {
                  if (result.status === 'ok') setOutputs(result.data)
                })
              })
          })
      })
      .catch((err: unknown) => {
        setAction('import')
        setImportStatus('error')
        setImportError(err instanceof Error ? err.message : String(err))
      })
  }, [selectedOutputId, userId, personaId, outputs, sourceView, t])

  // items.id=558: the discoverable entry point toggles between the two
  // views -- no separate "close" affordance needed, since re-clicking it
  // from the Imported view just flips straight back.
  const handleToggleSourceView = () => {
    setSourceView((prev) => (prev === 'qr_generated' ? 'external_ingested' : 'qr_generated'))
  }

  const handleSelectRow = (outputId: string) => {
    if (outputId === selectedOutputId) {
      resetSelection()
      return
    }
    setSelectedOutputId(outputId)
    setAction(null)
    setViewedOutput(null)
    setViewError(null)
    setCopyStatus(null)
    setCopyError(null)
    setImportStatus(null)
    setImportError(null)
  }

  // items.id=573: entry point for both lineage surfaces. A link may cross
  // the qr_generated/external_ingested toggle (decisions.id=826, Update is
  // bidirectional across source types) -- when it does, setSourceView fires
  // first and the pending-target effect above finishes the selection once
  // the new list has loaded, since the sourceView-change effect resets
  // selection and refetches `outputs` from scratch in the same tick.
  const navigateToLineageTarget = useCallback(
    (target: OutputInfo, openView: boolean) => {
      const targetSourceView = target.source as LibrarySourceView
      if (targetSourceView !== sourceView) {
        setPendingLineageTarget({ outputId: target.id, openView })
        setSourceView(targetSourceView)
        return
      }
      applyLineageSelection(target.id, openView)
    },
    [sourceView, applyLineageSelection],
  )

  const otherSourceLinkKey =
    sourceView === 'qr_generated'
      ? 'navShell.libraryPane.viewImportedLink'
      : 'navShell.libraryPane.viewLibraryLink'

  return (
    <div className="library-pane">
      <div className="library-pane__heading-row">
        <h2 className="library-pane__heading">
          {t(
            sourceView === 'qr_generated'
              ? 'navShell.libraryPane.listHeading'
              : 'navShell.libraryPane.importedListHeading',
          )}
        </h2>
        {/* items.id=558: shown next to the heading once there's a list
         *  to look at; when the current view is empty this same link
         *  collapses into the empty-state message below instead. */}
        {otherSourceHasDocs && outputs.length > 0 && (
          <button
            type="button"
            className="library-pane__link-button"
            onClick={handleToggleSourceView}
          >
            {t(otherSourceLinkKey)}
          </button>
        )}
      </div>
      {listError && (
        <p role="alert">{t('navShell.libraryPane.listLoadError', { message: listError })}</p>
      )}
      {outputs.length === 0 && !listError && (
        <p>
          {t(
            sourceView === 'qr_generated'
              ? 'navShell.libraryPane.emptyList'
              : 'navShell.libraryPane.emptyListImported',
          )}
          {otherSourceHasDocs && (
            <>
              {' '}
              <button
                type="button"
                className="library-pane__link-button"
                onClick={handleToggleSourceView}
              >
                {t(otherSourceLinkKey)}
              </button>
            </>
          )}
        </p>
      )}
      {outputs.length > 0 && (
        <ul className="library-pane__list">
          {outputs.map((output) => (
            <li key={output.id} className="library-pane__list-item">
              <DocumentRow
                output={output}
                selected={output.id === selectedOutputId}
                onSelect={() => handleSelectRow(output.id)}
              />
              {output.id === selectedOutputId && (
                <div className="library-pane__row-actions">
                  <button type="button" onClick={() => setAction('view')}>
                    {t('navShell.libraryPane.viewButton')}
                  </button>
                  <button type="button" onClick={handleCopy}>
                    {t('navShell.libraryPane.copyButton')}
                  </button>
                  <span className="library-pane__row-actions-divider" aria-hidden="true" />
                  <button type="button" onClick={handleImportClick}>
                    {t('navShell.libraryPane.importButton')}
                  </button>
                </div>
              )}
              {output.id === selectedOutputId &&
                (selectedLineage.backward !== null || selectedLineage.forward.length > 0) && (
                  <div className="library-pane__row-lineage">
                    <LineageLinks
                      lineage={selectedLineage}
                      onNavigate={navigateToLineageTarget}
                      openView={false}
                    />
                  </div>
                )}
            </li>
          ))}
        </ul>
      )}

      {action === 'view' && (
        <div className="library-pane__action-pane">
          {(viewedLineage.backward !== null || viewedLineage.forward.length > 0) && (
            <div className="library-pane__document-header">
              <LineageLinks lineage={viewedLineage} onNavigate={navigateToLineageTarget} openView={true} />
            </div>
          )}
          {viewError && (
            <p role="alert">
              {t('navShell.libraryPane.detailLoadError', { message: viewError })}
            </p>
          )}
          {viewedOutput && <pre className="library-pane__content">{viewedOutput.content}</pre>}
        </div>
      )}
      {action === 'copy' && (
        <div className="library-pane__action-pane">
          {copyStatus === 'success' && <p>{t('navShell.libraryPane.copySuccess')}</p>}
          {copyStatus === 'error' && (
            <p role="alert" className="library-pane__copy-error">
              {copyError}
            </p>
          )}
        </div>
      )}
      {action === 'import' && (
        <div className="library-pane__action-pane">
          {importStatus === 'pending' && <p>{t('navShell.libraryPane.importPending')}</p>}
          {importStatus === 'success' && <p>{t('navShell.libraryPane.importSuccess')}</p>}
          {importStatus === 'error' && (
            <p role="alert" className="library-pane__copy-error">
              {importError}
            </p>
          )}
        </div>
      )}
    </div>
  )
}
