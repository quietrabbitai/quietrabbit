// Privacy Guardian consent modal -- PRIVACY_GUARDIAN_GATE_SPEC.md (originally
// LOCKED, Chat-BRAND session June 22 2026, item 19c, D6-362; its tier-routing
// rule and Easy/Medium/High naming were superseded in direction 2026-09-03,
// decisions.id=753/754, and built out here, items.id=406). No modal component
// existed anywhere in this codebase before the original build -- built from
// the spec directly, following this codebase's plain-global-CSS / useState /
// t() conventions (Tier3Selector.tsx is the closest sibling for those idioms).
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
// items.id=406 (decisions.id=754): THREE-SECTION REDESIGN. The single-tier-
// for-the-whole-modal model is replaced by three simultaneous sections
// (Low/Medium/High), each independently populated by each span's own
// review_tier (gate3.rs::assign_review_tier_for_span, no longer a single
// batch-wide tier). Blank sections are hidden entirely. Per-tier behavior
// (default selection, select-all availability, CTA options) is otherwise
// UNCHANGED from the original locked spec -- reused as-is, per-section,
// rather than redesigned. This build ships the MECHANISM (grouping,
// hidden-if-empty, a single bottom Send gated by the High section's own
// existing rule) reusing the *current* per-tier visual treatment unchanged,
// stacked -- modal height/spacing/collapse behavior for three simultaneous
// sections is explicitly a follow-up Chat-BRAND visual-design pass, not
// decided here (this document's own scope boundary, per the item that
// dispatched this build).
//
// Behavior adaptation required by having multiple sections at once (not
// specified by the single-tier original, since only one tier ever existed
// per modal before): "Keep everything private" for one section now only
// sets THAT section's rows to keep_private locally -- it no longer
// immediately submits the whole modal, since other sections (particularly
// an unreviewed High section) may still need attention. The user still
// presses the single bottom Send once every section they care about is set.
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
import { isAllKeptPrivate, type ElementDecision, type ElementDecisionKind } from './consentDecisions'
import { groupSpansByTier, REVIEW_TIER_ORDER, type ReviewTier } from './reviewSections'
import './PrivacyGuardianModal.css'

export type { ReviewTier } from './reviewSections'

export interface ConsentSpanItem {
  span_id: string
  category: string
  user_label: string
  original_text: string
  suggestion: string | null
  start_byte: number
  end_byte: number
  score: number
  /** items.id=406: this span's own review tier -- independently assigned,
   *  not shared across the whole payload. Drives which section it renders in. */
  review_tier: ReviewTier
  /** items.id=406: the stable fact identity gate3 resolved for this span
   *  (or null if none of the three deterministic layers resolved it).
   *  Echoed back unchanged in the matching ElementDecision. */
  fact_key: string | null
}

export interface ConsentRequestPayload {
  focus_run_id: string
  focus_name: string
  /** items.id=406: no longer used for per-row layout -- each span carries
   *  its own review_tier. Kept only for the documented empty-spans-forced-
   *  High edge case (no span exists to carry a tier in that case). */
  review_tier: ReviewTier
  spans: ConsentSpanItem[]
}

export type { ElementDecisionKind, ElementDecision } from './consentDecisions'

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
  /** items.id=406 (decisions.id=756): "remember this for [Persona]",
   *  off by default -- only meaningful (and only shown) when the span has
   *  a fact_key; ignored server-side otherwise. */
  saveForPersona: boolean
}

const EMPTY_ROW: RowState = { decision: null, editing: false, editedText: null, saveForPersona: false }

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
  const [twoStepArm, setTwoStepArm] = useState<string | null>(null)
  const [confirmationMessage, setConfirmationMessage] = useState<string | null>(null)
  const twoStepTimerRef = useRef<number | null>(null)
  const confirmTimerRef = useRef<number | null>(null)

  // Reset per-open, and initialize row state once the payload is known --
  // each span's OWN review_tier determines that row's starting selection
  // (items.id=406: no longer one shared tier for every row).
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
        decision: defaultDecisionForTier(span.review_tier),
        editing: false,
        editedText: null,
        saveForPersona: false,
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

  const armTwoStep = (which: string, onConfirm: () => void) => {
    if (twoStepArm === which) {
      if (twoStepTimerRef.current !== null) window.clearTimeout(twoStepTimerRef.current)
      setTwoStepArm(null)
      onConfirm()
      return
    }
    setTwoStepArm(which)
    if (twoStepTimerRef.current !== null) window.clearTimeout(twoStepTimerRef.current)
    twoStepTimerRef.current = window.setTimeout(() => setTwoStepArm(null), TWO_STEP_RESET_MS)
  }

  const selectAllGeneralizeInSection = (spanIds: string[]) => {
    setRows((prev) => {
      const next = { ...prev }
      for (const id of spanIds) {
        next[id] = { ...next[id], decision: 'generalize' }
      }
      return next
    })
  }

  // items.id=406: sets only THIS section's rows to keep_private -- does NOT
  // submit the whole modal (see file header: a behavior adaptation required
  // once more than one section can be present at once).
  const keepSectionPrivate = (spanIds: string[]) => {
    setRows((prev) => {
      const next = { ...prev }
      for (const id of spanIds) {
        next[id] = { ...next[id], decision: 'keep_private', editing: false }
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

  const toggleSaveForPersona = (spanId: string) => {
    setRows((prev) => ({
      ...prev,
      [spanId]: { ...prev[spanId], saveForPersona: !prev[spanId].saveForPersona },
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
        category: span.category,
        fact_key: span.fact_key,
        save_for_persona: span.fact_key !== null && row.saveForPersona,
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

  const handleSend = () => {
    const decisions = buildDecisions(rows)
    const { generalized, keptPrivate, released } = countsByDecision(decisions)
    const message = isAllKeptPrivate(decisions)
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

  const grouped = groupSpansByTier(payload.spans)
  const highSpans = grouped.high
  const highReviewedCount = highSpans.filter((s) =>
    isRowValid(s, rows[s.span_id] ?? EMPTY_ROW),
  ).length
  const highAllReviewed = highReviewedCount === highSpans.length
  // items.id=406: Send is gated by the High section's own existing rule
  // (every row must be individually selected) when a High section is
  // present. With no High section, Send stays active immediately -- Low/
  // Medium rows arrive pre-selected, matching their original single-tier
  // behavior.
  const sendDisabled = highSpans.length > 0 && !highAllReviewed

  return (
    <div className="pg-modal-overlay">
      <div className="pg-modal" role="dialog" aria-modal="true">
        <PgHeader />
        <div className="pg-modal__subheader">
          {t('privacyGuardianModal.focusContext', { focusName: payload.focus_name })}
        </div>
        <div className="pg-modal__body">
          {REVIEW_TIER_ORDER.map((tier) => {
            const spans = grouped[tier]
            if (spans.length === 0) return null
            return (
              <PgSection
                key={tier}
                tier={tier}
                spans={spans}
                rows={rows}
                twoStepArm={twoStepArm}
                onArmTwoStep={armTwoStep}
                onSelectAllGeneralize={() => selectAllGeneralizeInSection(spans.map((s) => s.span_id))}
                onKeepSectionPrivate={() => keepSectionPrivate(spans.map((s) => s.span_id))}
                onSelect={setRowDecision}
                onStartEditing={startEditing}
                onCancelEditing={cancelEditing}
                onCommitEditing={commitEditing}
                onToggleSaveForPersona={toggleSaveForPersona}
                t={t}
              />
            )
          })}
        </div>
        <div className="pg-modal__cta-row">
          {highSpans.length > 0 && (
            <span className="pg-modal__cta-count" aria-live="polite">
              {t('privacyGuardianModal.reviewedCount', {
                count: highReviewedCount,
                total: highSpans.length,
              })}
            </span>
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

interface PgSectionProps {
  tier: ReviewTier
  spans: ConsentSpanItem[]
  rows: Record<string, RowState>
  twoStepArm: string | null
  onArmTwoStep: (which: string, onConfirm: () => void) => void
  onSelectAllGeneralize: () => void
  onKeepSectionPrivate: () => void
  onSelect: (spanId: string, kind: ElementDecisionKind) => void
  onStartEditing: (spanId: string) => void
  onCancelEditing: (spanId: string) => void
  onCommitEditing: (spanId: string, text: string) => void
  onToggleSaveForPersona: (spanId: string) => void
  t: (key: string, opts?: Record<string, unknown>) => string
}

function PgSection({
  tier,
  spans,
  rows,
  twoStepArm,
  onArmTwoStep,
  onSelectAllGeneralize,
  onKeepSectionPrivate,
  onSelect,
  onStartEditing,
  onCancelEditing,
  onCommitEditing,
  onToggleSaveForPersona,
  t,
}: PgSectionProps) {
  const { defaultKind, overrideTop, overrideBottom } = cellLayoutForTier(tier)
  const keepPrivateArmKey = `keepAllPrivate-${tier}`
  const selectAllArmKey = `selectAllGeneralize-${tier}`

  return (
    <section className="pg-modal__section" data-tier={tier}>
      <p className="pg-modal__section-label">{t(`privacyGuardianModal.sectionLabel.${tier}`)}</p>
      <p className="pg-modal__heading">{t(`privacyGuardianModal.${tier}.heading`)}</p>
      {tier !== 'low' && (
        <p className="pg-modal__subline">{t(`privacyGuardianModal.${tier}.subline`)}</p>
      )}
      {tier === 'medium' && (
        <button
          type="button"
          className="pg-modal__select-all"
          onClick={() => onArmTwoStep(selectAllArmKey, onSelectAllGeneralize)}
        >
          {twoStepArm === selectAllArmKey
            ? t('privacyGuardianModal.confirmSelectAll')
            : t('privacyGuardianModal.selectAllGeneralize')}
        </button>
      )}
      <ul className="pg-modal__row-list">
        {spans.map((span) => {
          const row = rows[span.span_id] ?? EMPTY_ROW
          return (
            <PgRow
              key={span.span_id}
              span={span}
              row={row}
              tier={tier}
              defaultKind={defaultKind}
              overrideTop={overrideTop}
              overrideBottom={overrideBottom}
              onSelect={(kind) => onSelect(span.span_id, kind)}
              onStartEditing={() => onStartEditing(span.span_id)}
              onCancelEditing={() => onCancelEditing(span.span_id)}
              onCommitEditing={(text) => onCommitEditing(span.span_id, text)}
              onToggleSaveForPersona={() => onToggleSaveForPersona(span.span_id)}
              t={t}
            />
          )
        })}
      </ul>
      {tier !== 'high' && (
        <button
          type="button"
          className="pg-modal__keep-all-private"
          onClick={() => onArmTwoStep(keepPrivateArmKey, onKeepSectionPrivate)}
        >
          {twoStepArm === keepPrivateArmKey
            ? t('privacyGuardianModal.confirmKeepAllPrivate')
            : t('privacyGuardianModal.keepEverythingPrivate')}
        </button>
      )}
    </section>
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
  onToggleSaveForPersona: () => void
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
  onToggleSaveForPersona,
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
      {/* items.id=406 (decisions.id=756): "remember this for [Persona]" --
          only shown when gate3 resolved a stable fact_key for this span;
          a plain checkbox for now, visual placement is a Chat-BRAND pass. */}
      {span.fact_key !== null && row.decision !== null && (
        <label className="pg-modal__remember-for-persona">
          <input type="checkbox" checked={row.saveForPersona} onChange={onToggleSaveForPersona} />
          {t('privacyGuardianModal.rememberForPersona')}
        </label>
      )}
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
