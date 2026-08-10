// Privacy Guardian consent modal -- PRIVACY_GUARDIAN_GATE_SPEC.md (LOCKED,
// Chat-BRAND session June 22 2026, item 19c, D6-362). No modal component
// existed anywhere in this codebase before this file -- built from the spec
// directly, following this codebase's plain-global-CSS / useState / t()
// conventions (Tier3Selector.tsx is the closest sibling for those idioms).
//
// Mounted by Tier3AccessPane.tsx once request_tier3_gate3_review reports
// pending_consent=true. `open` covers both the pre-payload scanning state
// (gate3() is still running server-side, bounded by gate3.rs's own 10s
// PF_TIMEOUT_SECS) and the post-payload tiered review; `payload` arrives via
// Tier3AccessPane's own consent_request listener once gate3() emits it.
//
// ConsentRequestPayload / ConsentSpanItem / ReviewTier / ElementDecision are
// hand-declared here, not generated: consent_request is emitted via
// AppHandle::emit (conductor/privacy/gate3.rs), not returned from a
// #[tauri::command], so tauri-specta's collect_commands! never sees it even
// though the Rust types derive specta::Type -- same treatment ChatPane.tsx
// gives RunStatusPayload, for the same reason (see that file's header
// comment). Keep these in sync by hand with conductor/privacy/types.rs.
//
// Scope trims from the spec, called out rather than silently dropped:
//   - The live "[N rows remaining]" scroll indicator is not implemented --
//     would need a scroll/IntersectionObserver wiring with no clear
//     precedent elsewhere in this codebase; the bounded scrollable list
//     with a bottom fade is implemented, just not the live count.
//   - The >10s "taking longer than expected" Cancel button is a *soft*
//     cancel: request_tier3_gate3_review is a single bounded async command
//     (gate3()'s own PF_TIMEOUT_SECS already caps it at ~10s), not a
//     cancelable in-flight operation with its own IPC cancel path the way
//     the spec's "tapping Cancel stops the run" phrasing implies. Tapping
//     Cancel here dismisses the modal locally (onCancel) and the eventual
//     command response is ignored by the caller -- nothing further is sent,
//     which satisfies the user-visible contract, but no gate_timeout event
//     is forced early; if the backend call itself times out server-side,
//     gate3.rs already logs gate_timeout independently.
//
// role="dialog"/role="button" (not the native <dialog>/<button> tags
// oxlint's jsx-a11y plugin suggests) are deliberate here, not oversights:
// native <dialog> closes on Escape by default unless fought with an extra
// keydown handler, which is exactly backwards for this modal's "no
// dismiss-by-keyboard, the only exits are the action buttons" requirement
// (PRIVACY_GUARDIAN_GATE_SPEC.md). role="button" on PgCell is required
// because the editing state nests real interactive children (input, cancel
// button) -- see PgCell's own comment for why it can't be a <button>.

import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import './PrivacyGuardianModal.css'

export type ReviewTier = 'easy' | 'medium' | 'high'

export interface ConsentSpanItem {
  span_id: string
  category: string
  user_label: string
  original_text: string
  suggestion: string | null
  start_byte: number
  end_byte: number
  score: number
}

export interface ConsentRequestPayload {
  focus_run_id: string
  focus_name: string
  review_tier: ReviewTier
  spans: ConsentSpanItem[]
}

export type ElementDecisionKind = 'generalize' | 'keep_private' | 'release_original'

export interface ElementDecision {
  span_id: string
  decision: ElementDecisionKind
  suggestion_text: string | null
  user_modified_text: string | null
}

export interface PrivacyGuardianModalProps {
  /** True from the moment gate3 review was requested until it resolves or
   *  is cancelled. False unmounts the modal entirely. */
  open: boolean
  /** Arrives once gate3() emits consent_request. Null while scanning. */
  payload: ConsentRequestPayload | null
  /** Fires once the user has resolved every row and confirmed Send (or
   *  Keep everything private). Caller submits the decisions and closes. */
  onResolve: (decisions: ElementDecision[]) => void
  /** Fires when the user cancels out of the >10s scanning state. See this
   *  file's header comment on why this is a soft (local-only) cancel. */
  onCancel: () => void
}

interface RowState {
  decision: ElementDecisionKind | null
  editing: boolean
  editedText: string | null
}

const SCANNING_SLOW_AFTER_MS = 10_000
const CONFIRMATION_DISMISS_MS = 1_500
const TWO_STEP_RESET_MS = 4_000

function defaultDecisionForTier(tier: ReviewTier): ElementDecisionKind | null {
  return tier === 'high' ? null : 'generalize'
}

function cellLayoutForTier(tier: ReviewTier): {
  defaultKind: ElementDecisionKind
  overrideTop: ElementDecisionKind
  overrideBottom: ElementDecisionKind
} {
  if (tier === 'high') {
    return { defaultKind: 'keep_private', overrideTop: 'generalize', overrideBottom: 'release_original' }
  }
  return { defaultKind: 'generalize', overrideTop: 'keep_private', overrideBottom: 'release_original' }
}

function effectiveSuggestionText(span: ConsentSpanItem, row: RowState): string {
  if (row.editedText !== null) return row.editedText
  return span.suggestion ?? ''
}

function isRowValid(span: ConsentSpanItem, row: RowState): boolean {
  if (row.decision === null) return false
  if (row.decision === 'generalize') {
    return effectiveSuggestionText(span, row).trim().length > 0
  }
  return true
}

export function PrivacyGuardianModal({
  open,
  payload,
  onResolve,
  onCancel,
}: PrivacyGuardianModalProps) {
  const { t } = useTranslation()
  const [rows, setRows] = useState<Record<string, RowState>>({})
  const [scanningSlow, setScanningSlow] = useState(false)
  const [twoStepArm, setTwoStepArm] = useState<'keepAllPrivate' | 'selectAllGeneralize' | null>(null)
  const [confirmationMessage, setConfirmationMessage] = useState<string | null>(null)
  const twoStepTimerRef = useRef<number | null>(null)
  const confirmTimerRef = useRef<number | null>(null)

  // Reset per-open, and initialize row state once the payload (and its
  // tier) is known -- tier determines each row's starting selection.
  useEffect(() => {
    if (!open) {
      setRows({})
      setScanningSlow(false)
      setTwoStepArm(null)
      setConfirmationMessage(null)
      return
    }
    if (!payload) return
    const initial: Record<string, RowState> = {}
    for (const span of payload.spans) {
      initial[span.span_id] = {
        decision: defaultDecisionForTier(payload.review_tier),
        editing: false,
        editedText: null,
      }
    }
    setRows(initial)
  }, [open, payload])

  // Scanning-state slow timer -- only while open and no payload yet.
  useEffect(() => {
    if (!open || payload) {
      setScanningSlow(false)
      return
    }
    const id = window.setTimeout(() => setScanningSlow(true), SCANNING_SLOW_AFTER_MS)
    return () => window.clearTimeout(id)
  }, [open, payload])

  useEffect(() => {
    return () => {
      if (twoStepTimerRef.current !== null) window.clearTimeout(twoStepTimerRef.current)
      if (confirmTimerRef.current !== null) window.clearTimeout(confirmTimerRef.current)
    }
  }, [])

  if (!open) return null

  const armTwoStep = (which: 'keepAllPrivate' | 'selectAllGeneralize') => {
    if (twoStepArm === which) {
      if (twoStepTimerRef.current !== null) window.clearTimeout(twoStepTimerRef.current)
      setTwoStepArm(null)
      if (which === 'keepAllPrivate') {
        finishWithAllKeptPrivate()
      } else {
        selectAllGeneralize()
      }
      return
    }
    setTwoStepArm(which)
    if (twoStepTimerRef.current !== null) window.clearTimeout(twoStepTimerRef.current)
    twoStepTimerRef.current = window.setTimeout(() => setTwoStepArm(null), TWO_STEP_RESET_MS)
  }

  const selectAllGeneralize = () => {
    setRows((prev) => {
      const next: Record<string, RowState> = {}
      for (const [id, row] of Object.entries(prev)) {
        next[id] = { ...row, decision: 'generalize' }
      }
      return next
    })
  }

  const setRowDecision = (spanId: string, decision: ElementDecisionKind) => {
    setRows((prev) => ({
      ...prev,
      [spanId]: { ...prev[spanId], decision, editing: false },
    }))
  }

  const startEditing = (spanId: string) => {
    setRows((prev) => ({ ...prev, [spanId]: { ...prev[spanId], editing: true } }))
  }

  const cancelEditing = (spanId: string) => {
    setRows((prev) => ({ ...prev, [spanId]: { ...prev[spanId], editing: false, editedText: null } }))
  }

  const commitEditing = (spanId: string, text: string) => {
    setRows((prev) => ({ ...prev, [spanId]: { ...prev[spanId], editing: false, editedText: text } }))
  }

  const buildDecisions = (rowsSnapshot: Record<string, RowState>): ElementDecision[] => {
    if (!payload) return []
    return payload.spans.map((span) => {
      const row = rowsSnapshot[span.span_id]
      return {
        span_id: span.span_id,
        decision: row.decision ?? 'keep_private',
        suggestion_text: span.suggestion,
        user_modified_text: row.editedText,
      }
    })
  }

  const countsByDecision = (decisions: ElementDecision[]) => {
    let generalized = 0
    let keptPrivate = 0
    let released = 0
    for (const d of decisions) {
      if (d.decision === 'generalize') generalized += 1
      else if (d.decision === 'keep_private') keptPrivate += 1
      else released += 1
    }
    return { generalized, keptPrivate, released }
  }

  const finishAndResolve = (decisions: ElementDecision[], message: string) => {
    setConfirmationMessage(message)
    if (confirmTimerRef.current !== null) window.clearTimeout(confirmTimerRef.current)
    confirmTimerRef.current = window.setTimeout(() => {
      onResolve(decisions)
    }, CONFIRMATION_DISMISS_MS)
  }

  const finishWithAllKeptPrivate = () => {
    if (!payload) return
    const allPrivate: Record<string, RowState> = {}
    for (const span of payload.spans) {
      allPrivate[span.span_id] = { decision: 'keep_private', editing: false, editedText: null }
    }
    setRows(allPrivate)
    const decisions = buildDecisions(allPrivate)
    finishAndResolve(decisions, t('privacyGuardianModal.confirmAllKeptPrivate'))
  }

  const handleSend = () => {
    const decisions = buildDecisions(rows)
    const { generalized, keptPrivate, released } = countsByDecision(decisions)
    const message =
      keptPrivate === decisions.length
        ? t('privacyGuardianModal.confirmAllKeptPrivate')
        : t('privacyGuardianModal.confirmMixed', { generalized, keptPrivate, released })
    finishAndResolve(decisions, message)
  }

  if (confirmationMessage) {
    return (
      <div className="pg-modal-overlay">
        <div className="pg-modal" role="dialog" aria-modal="true" aria-live="polite">
          <PgHeader />
          <div className="pg-modal__body pg-modal__body--confirming">
            <p className="pg-modal__confirmation">{confirmationMessage}</p>
          </div>
        </div>
      </div>
    )
  }

  if (!payload) {
    return (
      <div className="pg-modal-overlay">
        <div className="pg-modal" role="dialog" aria-modal="true">
          <PgHeader />
          <div className="pg-modal__body pg-modal__body--scanning">
            <p className="pg-modal__scanning-line">{t('privacyGuardianModal.scanningLine')}</p>
            {scanningSlow && (
              <>
                <p className="pg-modal__scanning-slow-line">
                  {t('privacyGuardianModal.scanningSlowLine')}
                </p>
                <button type="button" className="pg-modal__cancel-button" onClick={onCancel}>
                  {t('privacyGuardianModal.cancelButton')}
                </button>
              </>
            )}
          </div>
        </div>
      </div>
    )
  }

  const tier = payload.review_tier
  const { defaultKind, overrideTop, overrideBottom } = cellLayoutForTier(tier)
  const reviewedCount = payload.spans.filter((s) => isRowValid(s, rows[s.span_id] ?? { decision: null, editing: false, editedText: null })).length
  const totalCount = payload.spans.length
  const allReviewed = reviewedCount === totalCount
  const sendDisabled = tier === 'high' ? !allReviewed : reviewedCount !== totalCount

  return (
    <div className="pg-modal-overlay">
      <div className="pg-modal" role="dialog" aria-modal="true">
        <PgHeader />
        <div className="pg-modal__subheader">
          {t('privacyGuardianModal.focusContext', { focusName: payload.focus_name })}
        </div>
        <div className="pg-modal__body">
          <p className="pg-modal__heading">{t(`privacyGuardianModal.${tier}.heading`)}</p>
          {tier !== 'easy' && (
            <p className="pg-modal__subline">{t(`privacyGuardianModal.${tier}.subline`)}</p>
          )}
          {tier === 'medium' && (
            <button
              type="button"
              className="pg-modal__select-all"
              onClick={() => armTwoStep('selectAllGeneralize')}
            >
              {twoStepArm === 'selectAllGeneralize'
                ? t('privacyGuardianModal.confirmSelectAll')
                : t('privacyGuardianModal.selectAllGeneralize')}
            </button>
          )}
          <ul className="pg-modal__row-list">
            {payload.spans.map((span) => {
              const row = rows[span.span_id] ?? { decision: null, editing: false, editedText: null }
              return (
                <PgRow
                  key={span.span_id}
                  span={span}
                  row={row}
                  tier={tier}
                  defaultKind={defaultKind}
                  overrideTop={overrideTop}
                  overrideBottom={overrideBottom}
                  onSelect={(kind) => setRowDecision(span.span_id, kind)}
                  onStartEditing={() => startEditing(span.span_id)}
                  onCancelEditing={() => cancelEditing(span.span_id)}
                  onCommitEditing={(text) => commitEditing(span.span_id, text)}
                  t={t}
                />
              )
            })}
          </ul>
        </div>
        <div className="pg-modal__cta-row">
          {tier !== 'easy' && (
            <span className="pg-modal__cta-count" aria-live="polite">
              {t(
                tier === 'high'
                  ? 'privacyGuardianModal.reviewedCount'
                  : 'privacyGuardianModal.selectedCount',
                { count: reviewedCount, total: totalCount },
              )}
            </span>
          )}
          {tier !== 'high' && (
            <button
              type="button"
              className="pg-modal__keep-all-private"
              onClick={() => armTwoStep('keepAllPrivate')}
            >
              {twoStepArm === 'keepAllPrivate'
                ? t('privacyGuardianModal.confirmKeepAllPrivate')
                : t('privacyGuardianModal.keepEverythingPrivate')}
            </button>
          )}
          <button
            type="button"
            className="pg-modal__send"
            disabled={sendDisabled}
            onClick={handleSend}
          >
            {t('privacyGuardianModal.sendButton')}
          </button>
        </div>
      </div>
    </div>
  )
}

function PgHeader() {
  const { t } = useTranslation()
  return (
    <div className="pg-modal__header">
      <span className="pg-modal__header-dot" aria-hidden="true" />
      <span className="pg-modal__header-title">{t('privacyGuardianModal.headerTitle')}</span>
    </div>
  )
}

interface PgRowProps {
  span: ConsentSpanItem
  row: RowState
  tier: ReviewTier
  defaultKind: ElementDecisionKind
  overrideTop: ElementDecisionKind
  overrideBottom: ElementDecisionKind
  onSelect: (kind: ElementDecisionKind) => void
  onStartEditing: () => void
  onCancelEditing: () => void
  onCommitEditing: (text: string) => void
  t: (key: string, opts?: Record<string, unknown>) => string
}

function PgRow({
  span,
  row,
  tier,
  defaultKind,
  overrideTop,
  overrideBottom,
  onSelect,
  onStartEditing,
  onCancelEditing,
  onCommitEditing,
  t,
}: PgRowProps) {
  return (
    <li className="pg-modal__row" data-tier={tier}>
      <div className="pg-modal__row-header">
        <span className="pg-modal__row-category">{span.user_label}</span>
        <span className="pg-modal__row-original">"{span.original_text}"</span>
      </div>
      <div className="pg-modal__row-decision">
        <PgCell
          kind={defaultKind}
          isDefault
          span={span}
          row={row}
          onSelect={onSelect}
          onStartEditing={onStartEditing}
          onCancelEditing={onCancelEditing}
          onCommitEditing={onCommitEditing}
          t={t}
        />
        <div className="pg-modal__row-overrides">
          <PgCell
            kind={overrideTop}
            isDefault={false}
            span={span}
            row={row}
            onSelect={onSelect}
            onStartEditing={onStartEditing}
            onCancelEditing={onCancelEditing}
            onCommitEditing={onCommitEditing}
            t={t}
          />
          <PgCell
            kind={overrideBottom}
            isDefault={false}
            span={span}
            row={row}
            onSelect={onSelect}
            onStartEditing={onStartEditing}
            onCancelEditing={onCancelEditing}
            onCommitEditing={onCommitEditing}
            t={t}
          />
        </div>
      </div>
    </li>
  )
}

interface PgCellProps {
  kind: ElementDecisionKind
  isDefault: boolean
  span: ConsentSpanItem
  row: RowState
  onSelect: (kind: ElementDecisionKind) => void
  onStartEditing: () => void
  onCancelEditing: () => void
  onCommitEditing: (text: string) => void
  t: (key: string, opts?: Record<string, unknown>) => string
}

function PgCell({
  kind,
  isDefault,
  span,
  row,
  onSelect,
  onStartEditing,
  onCancelEditing,
  onCommitEditing,
  t,
}: PgCellProps) {
  const selected = row.decision === kind
  const label =
    kind === 'generalize'
      ? t('privacyGuardianModal.cellGeneralize')
      : kind === 'keep_private'
        ? t('privacyGuardianModal.cellKeepPrivate')
        : t('privacyGuardianModal.cellReleaseOriginal')

  const suggestionText = kind === 'generalize' ? effectiveSuggestionText(span, row) : ''
  const showPlaceholder = kind === 'generalize' && suggestionText.trim().length === 0

  // A plain div, not a <button>, because the editing state nests real
  // interactive children (a text input and a cancel button) -- a <button>
  // containing another <button> is invalid HTML and browsers will misbehave
  // (the outer button's own click handling breaks). role="button" +
  // tabIndex + a matching onKeyDown keep it keyboard-operable without that
  // nesting problem. Selecting is idempotent (`if (!selected)`), so clicks
  // that bubble up from the input/edit-cancel/edit-affordance children
  // while already selected are harmless no-ops -- no stopPropagation
  // wrapper needed for those.
  const selectCell = () => {
    if (!selected) onSelect(kind)
  }

  return (
    <div
      className={`pg-modal__cell pg-modal__cell--${kind}${isDefault ? ' pg-modal__cell--default' : ' pg-modal__cell--override'}`}
      data-selected={selected ? '' : undefined}
      role="button"
      tabIndex={0}
      onClick={selectCell}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          selectCell()
        }
      }}
    >
      <span className="pg-modal__cell-label">{label}</span>
      {kind === 'generalize' &&
        (row.editing ? (
          <span className="pg-modal__cell-edit-field">
            <input
              type="text"
              defaultValue={suggestionText}
              placeholder={t('privacyGuardianModal.suggestionPlaceholder')}
              onBlur={(e) => onCommitEditing(e.currentTarget.value)}
              onKeyDown={(e) => {
                e.stopPropagation()
                if (e.key === 'Enter') onCommitEditing(e.currentTarget.value)
              }}
            />
            <button
              type="button"
              className="pg-modal__cell-edit-cancel"
              onClick={(e) => {
                e.stopPropagation()
                onCancelEditing()
              }}
              aria-label={t('privacyGuardianModal.cancelEdit')}
            >
              ×
            </button>
          </span>
        ) : (
          <span className="pg-modal__cell-suggestion">
            {showPlaceholder ? t('privacyGuardianModal.suggestionPlaceholder') : suggestionText}
          </span>
        ))}
      {kind === 'generalize' && selected && !row.editing && (
        <button
          type="button"
          className="pg-modal__cell-edit-affordance"
          onClick={(e) => {
            e.stopPropagation()
            onStartEditing()
          }}
        >
          {t('privacyGuardianModal.editLabel')}
        </button>
      )}
    </div>
  )
}
