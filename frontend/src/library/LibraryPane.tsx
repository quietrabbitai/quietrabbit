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

import { useCallback, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { commands, type OutputInfo } from '../bindings'
import { DocumentRow } from './DocumentRow'
import './LibraryPane.css'

export interface LibraryPaneProps {
  userId: string
  /** items.id=404: Library is now a dock rail, scoped by the same
   *  activePersonaId NavState field Chat/History use -- the old
   *  content.personaFilter per-crumb mechanism (there is no more crumb
   *  chain to carry a filter on) is superseded. null means no Persona is
   *  active yet -- Outputs cannot be scoped without one. */
  personaId: string | null
}

type LibraryAction = 'view' | 'copy' | null

export function LibraryPane({ userId, personaId }: LibraryPaneProps) {
  const { t } = useTranslation()
  const [outputs, setOutputs] = useState<OutputInfo[]>([])
  const [listError, setListError] = useState<string | null>(null)
  const [selectedOutputId, setSelectedOutputId] = useState<string | null>(null)
  const [action, setAction] = useState<LibraryAction>(null)
  const [viewedOutput, setViewedOutput] = useState<OutputInfo | null>(null)
  const [viewError, setViewError] = useState<string | null>(null)
  const [copyStatus, setCopyStatus] = useState<'pending' | 'success' | 'error' | null>(null)
  const [copyError, setCopyError] = useState<string | null>(null)

  const resetSelection = useCallback(() => {
    setSelectedOutputId(null)
    setAction(null)
    setViewedOutput(null)
    setViewError(null)
    setCopyStatus(null)
    setCopyError(null)
  }, [])

  // Re-fetch on mount / identity change, mirrors ChatPane.tsx's own
  // identity-keyed re-fetch effect.
  useEffect(() => {
    setOutputs([])
    setListError(null)
    resetSelection()

    if (personaId === null) {
      // Nothing was ever really listable for this identity -- honest
      // empty state, not a fetch failure.
      return
    }

    // items.id=404: passes all 6 params explicitly (source: null) -- the
    // pre-rework call site omitted the trailing `source` arg entirely.
    // null keeps today's default behavior unchanged (list_outputs treats
    // a missing source as 'qr_generated', excluding ingested/external
    // documents from this default view).
    commands.listOutputs(userId, personaId, null, null, null, null).then((result) => {
      if (result.status === 'ok') {
        setOutputs(result.data)
      } else {
        setListError(result.error)
      }
    })
  }, [userId, personaId, resetSelection])

  // View action -- re-fetches via getOutput rather than trusting the list
  // row's own cached content, matching ChatPane's reconcile-over-cache
  // discipline and giving Privacy Guardian a fresh per-access check point.
  useEffect(() => {
    if (action !== 'view' || selectedOutputId === null || personaId === null) return
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
    // personaId is guaranteed non-null here: this whole component returns
    // its own notice-only render above before ever reaching UI that could
    // call this.
    if (selectedOutputId === null || personaId === null) return
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
  }

  if (personaId === null) {
    return (
      <div className="library-pane">
        <p className="library-pane__notice">{t('navShell.libraryPane.noPersonaContext')}</p>
      </div>
    )
  }

  return (
    <div className="library-pane">
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
    </div>
  )
}
