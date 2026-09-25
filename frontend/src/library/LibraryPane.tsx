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

import { type ChangeEvent, useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type OutputInfo } from '../bindings'
import { DocumentRow } from './DocumentRow'
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

// Mirrors ingest.rs's own TEXT_MIRROR_EXTENSIONS -- Import new version reads
// the picked file as text in-browser (no @tauri-apps file-dialog plugin
// wired yet for a real filesystem path), so it's limited to the same
// trivially-UTF-8-decodable formats the backend already mirrors verbatim.
const TEXT_IMPORT_EXTENSIONS = ['txt', 'md', 'markdown', 'html', 'htm']

function hasTextImportExtension(filename: string): boolean {
  const ext = filename.split('.').pop()?.toLowerCase()
  return ext !== undefined && TEXT_IMPORT_EXTENSIONS.includes(ext)
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
  const importInputRef = useRef<HTMLInputElement>(null)

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

  // Import new version (decisions.id=827): the button itself is the only
  // gate -- opening the native file picker *is* the confirmation, no extra
  // dialog. Picking a file stores it as a fresh ingested output tagged with
  // the selected document's own focus_slug/project_entity_id/sensitivity,
  // then supersedes the selected document via updateActiveDocument, exactly
  // as decisions.id=827's rationale describes ("tags result with
  // parent_output_id/superseded_by against the currently-selected document").
  const handleImportClick = () => {
    importInputRef.current?.click()
  }

  const handleImportFileChange = useCallback(
    (event: ChangeEvent<HTMLInputElement>) => {
      const file = event.target.files?.[0] ?? null
      event.target.value = ''
      if (file === null || selectedOutputId === null) return

      setAction('import')
      setImportStatus('pending')
      setImportError(null)

      const selected = outputs.find((o) => o.id === selectedOutputId)
      const focusSlug = selected?.focus_slug ?? null
      if (!selected || focusSlug === null) {
        setImportStatus('error')
        setImportError(t('navShell.libraryPane.importMissingFocusError'))
        return
      }
      if (!hasTextImportExtension(file.name)) {
        setImportStatus('error')
        setImportError(t('navShell.libraryPane.importUnsupportedFileError'))
        return
      }

      file
        .text()
        .then((content) =>
          commands.storeIngestedDocument(
            userId,
            personaId,
            focusSlug,
            selected.project_entity_id,
            selected.sensitivity,
            content,
            null,
          ),
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
        .catch((err: unknown) => {
          setImportStatus('error')
          setImportError(err instanceof Error ? err.message : String(err))
        })
    },
    [selectedOutputId, userId, personaId, outputs, sourceView, t],
  )

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
            </li>
          ))}
        </ul>
      )}

      {action === 'view' && (
        <div className="library-pane__action-pane">
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
      <input
        ref={importInputRef}
        type="file"
        accept=".txt,.md,.markdown,.html,.htm,text/plain,text/html,text/markdown"
        className="library-pane__import-input"
        onChange={handleImportFileChange}
      />
    </div>
  )
}
