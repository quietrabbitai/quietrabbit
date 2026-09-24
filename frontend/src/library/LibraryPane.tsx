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
// items.id=543 (PERSONA_SELECTOR_DESIGN_ITEM543_20260921.md Section 2.2):
// reworked again -- Library now has its own local persona filter
// (libraryFilter, usePersonaLocalFilter), decoupled from the shared
// activePersonaId this component used to scope its fetches with directly.
// This was a deliberate mid-session reversal in the design doc: an earlier
// draft had Library's pills write straight to activePersonaId, matching
// Chat, which meant browsing Library while hunting for a document could
// silently switch (and appear to lose) the user's open Chat conversation.
// libraryFilter defaults from activePersonaId once on mount and is
// independent of it afterward -- see usePersonaLocalFilter.ts's own header
// comment for the exact mechanism, mirrored from ActiveBoardPane.tsx's
// pre-existing personaFilter.

import { type ChangeEvent, useCallback, useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type OutputInfo, type PersonaInfo } from '../bindings'
import { DocumentRow } from './DocumentRow'
import { PersonaPillRow } from '../navShell/persona/PersonaPillRow'
import { usePersonaLocalFilter } from '../navShell/persona/usePersonaLocalFilter'
import './LibraryPane.css'

export interface LibraryPaneProps {
  userId: string
  personas: PersonaInfo[]
  /** The shared, app-wide active persona (NavState.activePersonaId) --
   *  read only here: drives PersonaPillRow's ring mark and libraryFilter's
   *  first-open default. Library's own selection never writes this. */
  activePersonaId: string | null
}

type LibraryAction = 'view' | 'copy' | 'import' | null

// Mirrors ingest.rs's own TEXT_MIRROR_EXTENSIONS -- Import new version reads
// the picked file as text in-browser (no @tauri-apps file-dialog plugin
// wired yet for a real filesystem path), so it's limited to the same
// trivially-UTF-8-decodable formats the backend already mirrors verbatim.
const TEXT_IMPORT_EXTENSIONS = ['txt', 'md', 'markdown', 'html', 'htm']

function hasTextImportExtension(filename: string): boolean {
  const ext = filename.split('.').pop()?.toLowerCase()
  return ext !== undefined && TEXT_IMPORT_EXTENSIONS.includes(ext)
}

export function LibraryPane({ userId, personas, activePersonaId }: LibraryPaneProps) {
  const { t } = useTranslation()
  const { filterId: libraryFilter, select: selectLibraryFilter } = usePersonaLocalFilter(
    activePersonaId,
    false,
  )
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

  // Re-fetch on mount / identity change, mirrors ChatPane.tsx's own
  // identity-keyed re-fetch effect.
  useEffect(() => {
    setOutputs([])
    setListError(null)
    resetSelection()

    if (libraryFilter === null) {
      // Nothing was ever really listable for this identity -- honest
      // empty state, not a fetch failure.
      return
    }

    // items.id=404: passes all 6 params explicitly (source: null) -- the
    // pre-rework call site omitted the trailing `source` arg entirely.
    // null keeps today's default behavior unchanged (list_outputs treats
    // a missing source as 'qr_generated', excluding ingested/external
    // documents from this default view).
    commands.listOutputs(userId, libraryFilter, null, null, null, null).then((result) => {
      if (result.status === 'ok') {
        setOutputs(result.data)
      } else {
        setListError(result.error)
      }
    })
  }, [userId, libraryFilter, resetSelection])

  // View action -- re-fetches via getOutput rather than trusting the list
  // row's own cached content, matching ChatPane's reconcile-over-cache
  // discipline and giving Privacy Guardian a fresh per-access check point.
  useEffect(() => {
    if (action !== 'view' || selectedOutputId === null || libraryFilter === null) return
    setViewedOutput(null)
    setViewError(null)
    commands.getOutput(selectedOutputId, userId, libraryFilter).then((result) => {
      if (result.status === 'ok') {
        setViewedOutput(result.data)
      } else {
        setViewError(result.error)
      }
    })
  }, [action, selectedOutputId, userId, libraryFilter])

  const handleCopy = useCallback(() => {
    // libraryFilter is guaranteed non-null here: this whole component
    // returns its own notice-only render above before ever reaching UI
    // that could call this.
    if (selectedOutputId === null || libraryFilter === null) return
    setAction('copy')
    setCopyStatus('pending')
    setCopyError(null)
    commands.copyOutputToClipboard(selectedOutputId, userId, libraryFilter).then((result) => {
      if (result.status === 'ok') {
        setCopyStatus('success')
      } else {
        // Already the actionable, clipboard-specific message -- rendered
        // verbatim, not wrapped in a generic error template.
        setCopyStatus('error')
        setCopyError(result.error)
      }
    })
  }, [selectedOutputId, userId, libraryFilter])

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
      if (file === null || selectedOutputId === null || libraryFilter === null) return

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
            libraryFilter,
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
            .updateActiveDocument(storeResult.data.output_id, selectedOutputId, userId, libraryFilter)
            .then((linkResult) => {
              if (linkResult.status !== 'ok') {
                setImportStatus('error')
                setImportError(linkResult.error)
                return
              }
              setImportStatus('success')
              commands.listOutputs(userId, libraryFilter, null, null, null, null).then((result) => {
                if (result.status === 'ok') setOutputs(result.data)
              })
            })
        })
        .catch((err: unknown) => {
          setImportStatus('error')
          setImportError(err instanceof Error ? err.message : String(err))
        })
    },
    [selectedOutputId, userId, libraryFilter, outputs, t],
  )

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

  return (
    <div className="library-pane">
      <PersonaPillRow
        personas={personas}
        activePersonaId={activePersonaId}
        filterId={libraryFilter}
        onSelect={selectLibraryFilter}
        allowAll={false}
      />

      {libraryFilter === null ? (
        <p className="library-pane__notice">{t('navShell.libraryPane.noPersonaContext')}</p>
      ) : (
        <>
          <h2 className="library-pane__heading">{t('navShell.libraryPane.listHeading')}</h2>
          {listError && (
            <p role="alert">{t('navShell.libraryPane.listLoadError', { message: listError })}</p>
          )}
          {outputs.length === 0 && !listError && <p>{t('navShell.libraryPane.emptyList')}</p>}
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
        </>
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
