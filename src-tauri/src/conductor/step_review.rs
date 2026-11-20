// src-tauri/src/conductor/step_review.rs
//
// items.id=884 (decisions.id=943, decisions.id=937 point 4, decisions.id=866,
// decisions.id=947): the OUTBOUND REVIEW of the rendered prompt of a step that
// goes to a hosted provider. The Privacy Filter, the redaction and the user
// review that API Chat applies to a typed turn (decisions.id=888) are applied to
// the text a Focus step actually sends.
//
// WHEN IT FIRES. Once per attempt, in execute_once, between the render and the
// context-window check, before the inference slot and before the spend gate. It
// fires if and only if the selected model is hosted, read from
// `treatment.hosted`, the single predicate that also picks the network branch and
// the render (step_treatment, decisions.id=947). A local pick under an external
// ceiling is NOT reviewed. A hosted pick under a local-only ceiling, or an
// inconsistent selection, already stopped with an error before this hook. A bound
// API Chat run skips it, because send_api_chat_message reviewed the turn before
// the run began (`ReviewedPrompt::for_bound_run`, and a test pins that no
// unbound step takes that door).
//
// WHAT THE NETWORK SEES. `hosted_step_call` takes a `ReviewedPrompt`, a newtype
// whose fields are private to this module, so the only text a hosted call can
// carry is text this module released: the rendered prompt after render_template
// trimmed it, with the user's decisions and the standing decisions applied
// SERVER SIDE. The front end never supplies the text that is sent. A source scan
// pins that nothing else constructs a ReviewedPrompt and that executor.rs has one
// call site of hosted_step_call. Any system text that is ever put on a
// HostedRequest goes through this review too.
//
// FAIL CLOSED. Before gate3 is trusted the filter must be available
// (`privacy_filter::is_available`), there must be an app handle (without one
// gate3 falls to its legacy path, which approves below severity 3 without
// scanning), the disclosure-log write must succeed, the filter must not time out
// or fail, the review record must be kept, and the spans must be applicable to the
// text. Each failure parks the step (recoverable, AwaitUser) with a code from
// spend/codes.rs and the reason as data. A debug build without
// PRIVACY_FILTER_LIB_DIR therefore parks every external step, by design.
//
// LONG PROMPTS (items.id=923). A prompt longer than privacy::MAX_REVIEW_BYTES
// (128 KiB) is refused before the filter is consulted (Park::TooLong, code
// step_review.too_long) and nothing is sent. A shorter one is classified by gate3
// in chunks (privacy_filter::classify_chunked) whose spans come back in
// whole-prompt offsets, so the hash, the stored descriptors, the held span text
// and the rebuilt outbound text are the same as for a one-call review. The first
// chunk that times out or fails parks the step as a single call would.
//
// PARK AND RESUME (decisions.id=875: attended or not, one rule). When the filter
// flagged spans the user must decide on, a step_reviews row is written PENDING
// (hash, offsets, categories: no text), gate3 has already emitted consent_request,
// and the step returns AwaitConsent: the run goes to awaiting_user. The user
// answers through submit_element_consent_decision (unchanged) and resumes. The
// step re-renders, re-hashes and finds the pending row:
//   * same hash, at least one recorded answer: the answers are applied (an
//     unanswered span is REMOVED, never sent), the row is finalised, the step goes;
//   * same hash, no answer at all (a restart, a closed modal): the review is asked
//     again, the old row is superseded;
//   * a changed hash: the old row is superseded and a new review runs.
// There is no clock TTL. cancel_run supersedes pending rows. The retry loop and a
// resume reuse a FINAL row with the same run, action and hash: the outbound text
// is rebuilt from the stored descriptors and the recorded answers and must hash to
// the stored outbound hash, or a new review runs.
//
// WHAT IS HELD IN MEMORY ONLY. The text of the flagged spans, so that
// get_pending_step_review can show a review whose consent_request event the front
// end missed. It is process memory, never written to disk, bounded, and dropped
// when the review ends. After a restart it is gone, and the pending review is then
// reported as needing a resume, which asks again.
//
// LOGGING: counts and hash prefixes, never prompt text (a source scan pins it).
// Events: step_review_pf_unavailable, step_review_blocked, step_review_clean,
// step_review_pending, step_review_applied, step_review_refused.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use sha2::{Digest, Sha256};
use specta::Type;
use sqlx::SqliteConnection;

use crate::api_chat::redact::{apply, Decision, SpanDecision};
use crate::api_chat::BoundDestination;
use crate::conductor::executor::StepContext;
use crate::conductor::failure::{FailureAction, FailureResult, FailureSeverity};
use crate::conductor::privacy::errors::DisclosureLogWriteError;
use crate::conductor::privacy::logger::DisclosureLogger;
use crate::conductor::privacy::privacy_filter;
use crate::conductor::privacy::types::{AutoResolvedSpan, ConsentSpanItem, Gate3Result};
use crate::conductor::privacy::PrivacyGateway;
use crate::conductor::step_treatment::StepTreatment;
use crate::conductor::tokens::ExternalAccess;
use crate::persistence::output_store::{self, RecordedSpanDecision};
use crate::persistence::step_review_store::{
    self as store, AskedSpan, Counts, Finalize, NewReview, ReviewOutcome, SettledSpan,
    StepReviewRow, StoredSpans,
};
use crate::spend::codes::Code;

// ---------------------------------------------------------------------------
// The gate parameters (copied from API Chat's review_new_turn, decisions.id=943
// point 2). Named so a test can pin them.
// ---------------------------------------------------------------------------

/// Placeholder severity: there is no PersonalTrack for a rendered prompt, so only
/// content the Privacy Filter actually flags reaches the modal.
pub(crate) const REVIEW_SEVERITY: u8 = 1;
/// The severity is a placeholder, not an assessment.
pub(crate) const REVIEW_SEVERITY_AUTHORITATIVE: bool = false;
/// External target, so gate3's own ceiling check also holds.
pub(crate) const REVIEW_TARGET_TIER: u8 = 3;
/// Strict constant, as the copy review and API Chat: no providers.risk_rating or
/// model-override lookup (items.id=729 is separate and open).
pub(crate) const REVIEW_DESTINATION_RISK: u8 = 3;

/// Bound on spans held in memory for get_pending_step_review.
const MAX_HELD_REVIEWS: usize = 32;

// ---------------------------------------------------------------------------
// ReviewedPrompt: the only text a hosted call can carry
// ---------------------------------------------------------------------------

/// The text a hosted step may send. Its fields are private: nothing outside this
/// module can build one, except `for_bound_run` (a bound API Chat run, reviewed
/// before it began) and the test constructor.
#[derive(Clone, PartialEq, Eq)]
pub struct ReviewedPrompt {
    text: String,
    sha: String,
}

impl ReviewedPrompt {
    fn new(text: String) -> Self {
        let sha = sha256_hex(&text);
        Self { text, sha }
    }

    /// The text to send.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// SHA-256 of the text to send, as stored in step_reviews.outbound_sha.
    pub fn sha(&self) -> &str {
        &self.sha
    }

    /// A bound API Chat run: send_api_chat_message reviewed the typed turn (and
    /// replayed history was reviewed when it was sent) before the run began, so
    /// the step does not review again (decisions.id=943 point 1, ruling Q3). The
    /// `BoundDestination` is the proof the caller is a bound run.
    pub fn for_bound_run(_bound: &BoundDestination, text: &str) -> Self {
        Self::new(text.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn for_test(text: &str) -> Self {
        Self::new(text.to_owned())
    }
}

impl std::fmt::Debug for ReviewedPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ReviewedPrompt {{ bytes: {}, sha256: {} }}",
            self.text.len(),
            short(&self.sha)
        )
    }
}

pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

// ---------------------------------------------------------------------------
// The scanner: gate3 behind a trait so the verdict and persistence logic can be
// tested with an injectable scanner and availability flag (decisions.id=943
// point 9)
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ReviewScanner: Send + Sync {
    /// The Privacy Filter is loaded and can classify.
    fn filter_available(&self) -> bool;
    /// There is an app handle gate3 can emit the consent request through.
    fn has_app_handle(&self) -> bool;
    /// One gate3 call over the rendered prompt. gate3 emits consent_request itself
    /// when the user must decide.
    async fn scan(
        &self,
        run_id: &str,
        action_id: &str,
        content: &str,
    ) -> Result<Gate3Result, DisclosureLogWriteError>;
}

/// The production scanner: one gate3 call with the review parameters.
pub struct Gate3Scanner<'a, L: DisclosureLogger> {
    pub gateway: &'a PrivacyGateway<L>,
    pub app_handle: Option<&'a tauri::AppHandle<tauri::Wry>>,
    pub focus_name: &'a str,
    /// The derived access of this attempt (`treatment.access`), not the ceiling
    /// lifecycle computed (decisions.id=947).
    pub ceiling: ExternalAccess,
    /// The tier this attempt really runs at (`treatment.execution_tier`).
    pub execution_tier: u8,
    pub user_id: &'a str,
    pub persona_id: &'a str,
    pub key_hex: &'a str,
}

#[async_trait]
impl<L: DisclosureLogger> ReviewScanner for Gate3Scanner<'_, L> {
    fn filter_available(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = forced::get() {
            return forced.available;
        }
        privacy_filter::is_available()
    }

    fn has_app_handle(&self) -> bool {
        #[cfg(test)]
        if let Some(forced) = forced::get() {
            return forced.has_handle;
        }
        self.app_handle.is_some()
    }

    async fn scan(
        &self,
        run_id: &str,
        action_id: &str,
        content: &str,
    ) -> Result<Gate3Result, DisclosureLogWriteError> {
        #[cfg(test)]
        if let Some(forced) = forced::get() {
            return (forced.result)(content);
        }
        self.gateway
            .gate3(
                action_id,
                run_id,
                self.focus_name,
                action_id,
                content,
                REVIEW_SEVERITY,
                REVIEW_TARGET_TIER,
                self.ceiling,
                self.execution_tier,
                self.app_handle,
                Some(REVIEW_DESTINATION_RISK),
                REVIEW_SEVERITY_AUTHORITATIVE,
                self.user_id,
                self.persona_id,
                self.key_hex,
            )
            .await
    }
}

/// Test-only override of the scanner the executor builds, so a test that drives
/// execute_once can choose what the filter says. Thread-local, so it follows a
/// current-thread test runtime and never leaks between tests.
#[cfg(test)]
pub(crate) mod forced {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    pub type ForcedResult = Rc<dyn Fn(&str) -> Result<Gate3Result, DisclosureLogWriteError>>;

    #[derive(Clone)]
    pub struct Forced {
        pub available: bool,
        pub has_handle: bool,
        pub result: ForcedResult,
    }

    thread_local! {
        static FORCED: RefCell<Option<Forced>> = const { RefCell::new(None) };
    }

    pub fn get() -> Option<Forced> {
        FORCED.with(|f| f.borrow().clone())
    }

    /// Install an override until the guard drops.
    pub struct Guard;

    pub fn set(forced: Forced) -> Guard {
        FORCED.with(|f| *f.borrow_mut() = Some(forced));
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FORCED.with(|f| *f.borrow_mut() = None);
        }
    }

    /// Make a test context reviewable: the run row the review record references
    /// exists (outputs.db at the test's data root), and the scanner is `forced`
    /// until the guard drops. A test that drives execute_once for a hosted step
    /// needs both, because the review runs before the spend gate.
    pub async fn arm(ctx: &StepContext, forced: Forced) -> Guard {
        let mut conn =
            output_store::open_outputs_db_retrying(&ctx.user_id, &ctx.persona_id, &ctx.key_hex)
                .await
                .expect("outputs.db for the review record");
        sqlx::query(
            "INSERT OR IGNORE INTO focus_runs (id, focus_id, started_at) VALUES (?, ?, 't')",
        )
        .bind(&ctx.focus_run_id)
        .bind(&ctx.focus_id)
        .execute(&mut conn)
        .await
        .expect("focus_runs row");
        set(forced)
    }

    /// A scanner that finds nothing in anything.
    pub fn clean() -> Forced {
        Forced {
            available: true,
            has_handle: true,
            result: Rc::new(|_| {
                Ok(Gate3Result {
                    approved: true,
                    ..Gate3Result::default()
                })
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// In-memory hold of the flagged spans' text (get_pending_step_review)
// ---------------------------------------------------------------------------

struct HeldSpans {
    prompt_sha: String,
    spans: Vec<ConsentSpanItem>,
    seq: u64,
}

static HELD: Mutex<Option<(u64, HashMap<String, HeldSpans>)>> = Mutex::new(None);

fn with_held<R>(f: impl FnOnce(&mut u64, &mut HashMap<String, HeldSpans>) -> R) -> R {
    let mut guard = HELD.lock().unwrap_or_else(|p| p.into_inner());
    let (seq, map) = guard.get_or_insert_with(|| (0, HashMap::new()));
    f(seq, map)
}

fn hold_spans(review_id: &str, prompt_sha: &str, spans: Vec<ConsentSpanItem>) {
    with_held(|seq, map| insert_bounded(map, seq, MAX_HELD_REVIEWS, review_id, prompt_sha, spans));
}

/// Insert into a held map, dropping the oldest entries past `cap`.
fn insert_bounded(
    map: &mut HashMap<String, HeldSpans>,
    seq: &mut u64,
    cap: usize,
    review_id: &str,
    prompt_sha: &str,
    spans: Vec<ConsentSpanItem>,
) {
    *seq += 1;
    while map.len() >= cap {
        let oldest = map
            .iter()
            .min_by_key(|(_, h)| h.seq)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                map.remove(&k);
            }
            None => break,
        }
    }
    map.insert(
        review_id.to_owned(),
        HeldSpans {
            prompt_sha: prompt_sha.to_owned(),
            spans,
            seq: *seq,
        },
    );
}

fn drop_held(review_id: &str) {
    with_held(|_, map| {
        map.remove(review_id);
    });
}

fn held_spans(review_id: &str) -> Option<(String, Vec<ConsentSpanItem>)> {
    with_held(|_, map| {
        map.get(review_id)
            .map(|h| (h.prompt_sha.clone(), h.spans.clone()))
    })
}

// ---------------------------------------------------------------------------
// Decisions and parks
// ---------------------------------------------------------------------------

/// Why a review blocked. A short code, never text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// No app handle: gate3 could not scan or ask.
    NoAppHandle,
    /// The Privacy Filter timed out.
    Timeout,
    /// The Privacy Filter failed, or gate3 blocked the content.
    FilterBlocked,
    /// gate3 returned a result that is none of clean, pending or blocked.
    Indeterminate,
    /// The disclosure-log write failed.
    AuditLog,
    /// The review record could not be read or written.
    RecordUnavailable,
    /// The spans cannot be applied safely to the text.
    Unappliable,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::NoAppHandle => "no_app_handle",
            BlockReason::Timeout => "timeout",
            BlockReason::FilterBlocked => "filter_blocked",
            BlockReason::Indeterminate => "indeterminate",
            BlockReason::AuditLog => "audit_log_unavailable",
            BlockReason::RecordUnavailable => "record_unavailable",
            BlockReason::Unappliable => "unappliable",
        }
    }
}

/// Why the step parks instead of sending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Park {
    /// The user must answer. gate3 has emitted consent_request.
    Pending {
        review_id: String,
    },
    /// The Privacy Filter is not available.
    FilterUnavailable,
    /// The prompt is longer than privacy::MAX_REVIEW_BYTES (items.id=923): refused
    /// before any scan.
    TooLong,
    Blocked(BlockReason),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    /// Send exactly this text.
    Send(ReviewedPrompt),
    Park(Park),
}

/// What the executor hands the review.
#[derive(Debug, Clone, Copy)]
pub struct ReviewInput<'a> {
    pub run_id: &'a str,
    pub action_id: &'a str,
    /// The rendered prompt, after render_template trimmed it.
    pub prompt: &'a str,
    /// The provider of the selected model, for the audit row.
    pub provider_id: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Pure: applying decisions to text, and the outcome vocabulary
// ---------------------------------------------------------------------------

fn settled_to_decision(s: &SettledSpan) -> SpanDecision {
    SpanDecision {
        start_byte: s.start_byte,
        end_byte: s.end_byte,
        decision: Decision::parse(&s.decision),
        suggestion_text: s.suggestion_text.clone(),
        user_modified_text: s.user_modified_text.clone(),
    }
}

fn auto_to_settled(a: &AutoResolvedSpan) -> SettledSpan {
    SettledSpan {
        start_byte: a.start_byte as usize,
        end_byte: a.end_byte as usize,
        decision: match Decision::parse(&a.decision) {
            Decision::Generalize => "generalize",
            Decision::ReleaseOriginal => "release_original",
            Decision::KeepPrivate => "keep_private",
        }
        .to_owned(),
        suggestion_text: a.suggestion_text.clone(),
        user_modified_text: a.user_modified_text.clone(),
    }
}

fn answer_for<'a>(
    answers: &'a [RecordedSpanDecision],
    span_id: &str,
) -> Option<&'a RecordedSpanDecision> {
    answers.iter().find(|a| a.span_id == span_id)
}

/// The text that leaves: the settled spans as they were settled, every asked span
/// as the user answered it, and an unanswered asked span REMOVED. None when the
/// spans cannot be applied safely, and the caller must not send. The original is
/// never the fallback.
pub fn outbound_for(
    prompt: &str,
    spans: &StoredSpans,
    answers: &[RecordedSpanDecision],
) -> Option<String> {
    let mut all: Vec<SpanDecision> = spans.settled.iter().map(settled_to_decision).collect();
    for s in &spans.asked {
        all.push(match answer_for(answers, &s.span_id) {
            Some(a) => SpanDecision {
                start_byte: s.start_byte,
                end_byte: s.end_byte,
                decision: Decision::parse(&a.decision),
                suggestion_text: a.suggestion_text.clone(),
                user_modified_text: a.user_modified_text.clone(),
            },
            None => SpanDecision::unresolved(s.start_byte, s.end_byte),
        });
    }
    apply(prompt, all)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Released,
    /// `edited`: the user wrote a replacement that is not the suggestion.
    Generalized {
        edited: bool,
    },
    Removed,
}

/// What a decision does to a span, mirroring redact::apply's replacement rule.
fn classify(decision: Decision, suggestion: Option<&str>, user_modified: Option<&str>) -> Kind {
    match decision {
        Decision::ReleaseOriginal => Kind::Released,
        Decision::KeepPrivate => Kind::Removed,
        Decision::Generalize => {
            let replacement = user_modified.or(suggestion).unwrap_or("").trim();
            if replacement.is_empty() {
                return Kind::Removed;
            }
            let edited = user_modified
                .map(str::trim)
                .is_some_and(|m| !m.is_empty() && Some(m) != suggestion.map(str::trim));
            Kind::Generalized { edited }
        }
    }
}

/// The outcome and the counts of a review whose asked spans have been answered
/// (or not). `edited` wins over `redacted`, and the counts keep the detail.
pub fn derive_outcome(
    spans: &StoredSpans,
    answers: &[RecordedSpanDecision],
) -> (ReviewOutcome, Counts) {
    let mut counts = Counts {
        total: (spans.asked.len() + spans.settled.len()) as u32,
        auto_resolved: spans.settled.len() as u32,
        ..Counts::default()
    };
    let mut bump = |k: Kind| match k {
        Kind::Released => counts.released += 1,
        Kind::Generalized { .. } => counts.generalized += 1,
        Kind::Removed => counts.removed += 1,
    };

    for s in &spans.settled {
        bump(classify(
            Decision::parse(&s.decision),
            s.suggestion_text.as_deref(),
            s.user_modified_text.as_deref(),
        ));
    }
    let mut edited = false;
    let mut asked_removed = false;
    for s in &spans.asked {
        let kind = match answer_for(answers, &s.span_id) {
            Some(a) => classify(
                Decision::parse(&a.decision),
                a.suggestion_text.as_deref(),
                a.user_modified_text.as_deref(),
            ),
            None => Kind::Removed,
        };
        edited |= matches!(kind, Kind::Generalized { edited: true });
        asked_removed |= kind == Kind::Removed;
        bump(kind);
    }

    let outcome = if spans.asked.is_empty() {
        if spans.settled.is_empty() {
            ReviewOutcome::Clean
        } else {
            ReviewOutcome::StandingApplied
        }
    } else if edited {
        ReviewOutcome::Edited
    } else if asked_removed {
        ReviewOutcome::Redacted
    } else {
        ReviewOutcome::ApprovedAsShown
    };
    (outcome, counts)
}

fn tier_str(t: &crate::conductor::privacy::types::ReviewTier) -> &'static str {
    use crate::conductor::privacy::types::ReviewTier;
    match t {
        ReviewTier::Low => "low",
        ReviewTier::Medium => "medium",
        ReviewTier::High => "high",
    }
}

fn asked_from(spans: &[ConsentSpanItem]) -> Vec<AskedSpan> {
    spans
        .iter()
        .map(|s| AskedSpan {
            span_id: s.span_id.clone(),
            start_byte: s.start_byte,
            end_byte: s.end_byte,
            category: s.category.clone(),
            review_tier: tier_str(&s.review_tier).to_owned(),
        })
        .collect()
}

/// A flagged span must lie inside the prompt on character boundaries and name
/// the text it was flagged on. A span that does not is not applicable.
fn spans_fit(prompt: &str, spans: &[ConsentSpanItem]) -> bool {
    spans.iter().all(|s| {
        prompt
            .get(s.start_byte..s.end_byte)
            .is_some_and(|t| t == s.original_text)
    })
}

// ---------------------------------------------------------------------------
// The review
// ---------------------------------------------------------------------------

fn log_blocked(input: &ReviewInput<'_>, sha: &str, reason: BlockReason) {
    log::warn!(
        "step_review_blocked: run={} action={} prompt_sha={} reason={}",
        input.run_id,
        input.action_id,
        short(sha),
        reason.as_str()
    );
}

/// Record a blocked review and answer Park::Blocked. A failure to write the row
/// changes nothing for the step: it parks either way.
async fn block(
    conn: &mut SqliteConnection,
    input: &ReviewInput<'_>,
    sha: &str,
    reason: BlockReason,
) -> ReviewDecision {
    log_blocked(input, sha, reason);
    let spans = StoredSpans::default();
    let _ = store::insert_review(
        conn,
        &NewReview {
            focus_run_id: input.run_id,
            action_id: input.action_id,
            prompt_sha: sha,
            prompt_len: input.prompt.len(),
            spans: &spans,
            counts: Counts::default(),
            outcome: ReviewOutcome::Blocked,
            reason: Some(reason.as_str()),
            outbound_sha: None,
            outbound_len: None,
            provider_id: input.provider_id,
        },
    )
    .await;
    ReviewDecision::Park(Park::Blocked(reason))
}

/// Review the rendered prompt of an external step and decide what, if anything,
/// may be sent. See the module header for every branch.
pub async fn review_prompt<S: ReviewScanner>(
    conn: &mut SqliteConnection,
    scanner: &S,
    input: &ReviewInput<'_>,
) -> ReviewDecision {
    let sha = sha256_hex(input.prompt);

    // 1. An earlier review of this action in this run.
    let live = match store::latest_live_review(conn, input.run_id, input.action_id).await {
        Ok(row) => row,
        Err(_) => return block(conn, input, &sha, BlockReason::RecordUnavailable).await,
    };
    if let Some(row) = live {
        if row.prompt_sha == sha {
            match resume_existing(conn, input, &sha, &row).await {
                Existing::Decided(d) => return d,
                Existing::ReviewAgain => {}
            }
        }
    }

    // 2. A new review. Whatever was still pending for this action is replaced.
    match store::supersede_pending_for_action(conn, input.run_id, input.action_id).await {
        Ok(ids) => ids.iter().for_each(|id| drop_held(id)),
        Err(_) => return block(conn, input, &sha, BlockReason::RecordUnavailable).await,
    }

    // items.id=923: a prompt longer than the review accepts is refused before
    // any part of it is scanned. Nothing is sent; the row records the reason.
    let prompt_bytes = input.prompt.len();
    if prompt_bytes > crate::conductor::privacy::MAX_REVIEW_BYTES {
        log::warn!(
            "step_review_blocked: run={} action={} prompt_sha={} reason=prompt_too_long bytes={}",
            input.run_id,
            input.action_id,
            short(&sha),
            prompt_bytes
        );
        let spans = StoredSpans::default();
        let _ = store::insert_review(
            conn,
            &NewReview {
                focus_run_id: input.run_id,
                action_id: input.action_id,
                prompt_sha: &sha,
                prompt_len: input.prompt.len(),
                spans: &spans,
                counts: Counts::default(),
                outcome: ReviewOutcome::Blocked,
                reason: Some("prompt_too_long"),
                outbound_sha: None,
                outbound_len: None,
                provider_id: input.provider_id,
            },
        )
        .await;
        return ReviewDecision::Park(Park::TooLong);
    }

    if !scanner.filter_available() {
        log::warn!(
            "step_review_pf_unavailable: run={} action={} prompt_sha={}",
            input.run_id,
            input.action_id,
            short(&sha)
        );
        let spans = StoredSpans::default();
        let _ = store::insert_review(
            conn,
            &NewReview {
                focus_run_id: input.run_id,
                action_id: input.action_id,
                prompt_sha: &sha,
                prompt_len: input.prompt.len(),
                spans: &spans,
                counts: Counts::default(),
                outcome: ReviewOutcome::Blocked,
                reason: Some("filter_unavailable"),
                outbound_sha: None,
                outbound_len: None,
                provider_id: input.provider_id,
            },
        )
        .await;
        return ReviewDecision::Park(Park::FilterUnavailable);
    }
    if !scanner.has_app_handle() {
        return block(conn, input, &sha, BlockReason::NoAppHandle).await;
    }

    let result = match scanner
        .scan(input.run_id, input.action_id, input.prompt)
        .await
    {
        Ok(r) => r,
        Err(_) => return block(conn, input, &sha, BlockReason::AuditLog).await,
    };

    if result.timeout {
        return block(conn, input, &sha, BlockReason::Timeout).await;
    }
    if result.blocked {
        return block(conn, input, &sha, BlockReason::FilterBlocked).await;
    }
    let settled: Vec<SettledSpan> = result.auto_resolved.iter().map(auto_to_settled).collect();

    if result.pending_consent {
        // Nothing to ask about: gate3 can only reach here with no spans through its
        // forced-High branch (severity or an authoritative destination risk), which
        // these parameters never set. If it ever did, a review the user cannot answer
        // would park the step for good, so it is blocked instead.
        if result.review_spans.is_empty() {
            return block(conn, input, &sha, BlockReason::Indeterminate).await;
        }
        if !spans_fit(input.prompt, &result.review_spans) {
            return block(conn, input, &sha, BlockReason::Unappliable).await;
        }
        let stored = StoredSpans {
            asked: asked_from(&result.review_spans),
            settled,
        };
        // Whatever the user answers, the spans must apply: checked now, so a
        // review that could never be applied is not put in front of the user.
        if outbound_for(input.prompt, &stored, &[]).is_none() {
            return block(conn, input, &sha, BlockReason::Unappliable).await;
        }
        let (_, counts) = derive_outcome(&stored, &[]);
        let id = match store::insert_review(
            conn,
            &NewReview {
                focus_run_id: input.run_id,
                action_id: input.action_id,
                prompt_sha: &sha,
                prompt_len: input.prompt.len(),
                spans: &stored,
                counts,
                outcome: ReviewOutcome::Pending,
                reason: None,
                outbound_sha: None,
                outbound_len: None,
                provider_id: input.provider_id,
            },
        )
        .await
        {
            Ok(id) => id,
            // gate3 has already shown the request: the modal is an orphan, the
            // step parks, and a resume starts a new review.
            Err(_) => return block(conn, input, &sha, BlockReason::RecordUnavailable).await,
        };
        hold_spans(&id, &sha, result.review_spans);
        log::info!(
            "step_review_pending: run={} action={} review={} prompt_sha={} asked={} auto={}",
            input.run_id,
            input.action_id,
            id,
            short(&sha),
            stored.asked.len(),
            stored.settled.len()
        );
        return ReviewDecision::Park(Park::Pending { review_id: id });
    }

    if !result.approved {
        // None of clean, pending or blocked: not a green light.
        return block(conn, input, &sha, BlockReason::Indeterminate).await;
    }

    // Clean, or every flagged span settled by a standing decision.
    let stored = StoredSpans {
        asked: vec![],
        settled,
    };
    let Some(outbound) = outbound_for(input.prompt, &stored, &[]) else {
        return block(conn, input, &sha, BlockReason::Unappliable).await;
    };
    let (outcome, counts) = derive_outcome(&stored, &[]);
    let out_sha = sha256_hex(&outbound);
    if store::insert_review(
        conn,
        &NewReview {
            focus_run_id: input.run_id,
            action_id: input.action_id,
            prompt_sha: &sha,
            prompt_len: input.prompt.len(),
            spans: &stored,
            counts,
            outcome,
            reason: None,
            outbound_sha: Some(&out_sha),
            outbound_len: Some(outbound.len()),
            provider_id: input.provider_id,
        },
    )
    .await
    .is_err()
    {
        return block(conn, input, &sha, BlockReason::RecordUnavailable).await;
    }
    log::info!(
        "step_review_clean: run={} action={} prompt_sha={} outcome={} spans={} outbound_sha={}",
        input.run_id,
        input.action_id,
        short(&sha),
        outcome.as_str(),
        counts.total,
        short(&out_sha)
    );
    ReviewDecision::Send(ReviewedPrompt::new(outbound))
}

enum Existing {
    Decided(ReviewDecision),
    ReviewAgain,
}

/// A live row with the same prompt hash: reuse a final one, apply a pending one
/// the user has answered, or ask again.
async fn resume_existing(
    conn: &mut SqliteConnection,
    input: &ReviewInput<'_>,
    sha: &str,
    row: &StepReviewRow,
) -> Existing {
    let answers = if row.spans.asked.is_empty() {
        Vec::new()
    } else {
        match output_store::list_element_decisions_on_conn(conn, input.run_id).await {
            Ok(a) => a,
            Err(_) => {
                return Existing::Decided(
                    block(conn, input, sha, BlockReason::RecordUnavailable).await,
                )
            }
        }
    };

    if row.outcome.is_final() {
        // The retry loop, or a resume after the review was applied: no new
        // review, no second question. The text is rebuilt and must hash to what
        // was released, or the review runs again.
        let rebuilt = outbound_for(input.prompt, &row.spans, &answers);
        return match rebuilt {
            Some(text) => {
                let out_sha = sha256_hex(&text);
                if row.outbound_sha.as_deref() == Some(out_sha.as_str())
                    && row.outbound_len == Some(text.len())
                {
                    log::info!(
                        "step_review_applied: run={} action={} review={} reused=true outcome={} \
                         outbound_sha={}",
                        input.run_id,
                        input.action_id,
                        row.review_id,
                        row.outcome.as_str(),
                        short(&out_sha)
                    );
                    Existing::Decided(ReviewDecision::Send(ReviewedPrompt::new(text)))
                } else {
                    Existing::ReviewAgain
                }
            }
            None => Existing::ReviewAgain,
        };
    }

    // Pending with the same text.
    let answered = row
        .spans
        .asked
        .iter()
        .any(|s| answer_for(&answers, &s.span_id).is_some());
    if !answered {
        // Nothing was answered (a restart, a dismissed modal): ask again. The
        // caller supersedes this row and runs a new review.
        return Existing::ReviewAgain;
    }

    let Some(outbound) = outbound_for(input.prompt, &row.spans, &answers) else {
        let _ = store::block_pending_review(conn, &row.review_id, "unappliable").await;
        drop_held(&row.review_id);
        log_blocked(input, sha, BlockReason::Unappliable);
        return Existing::Decided(ReviewDecision::Park(Park::Blocked(
            BlockReason::Unappliable,
        )));
    };
    let (outcome, counts) = derive_outcome(&row.spans, &answers);
    let out_sha = sha256_hex(&outbound);
    match store::finalize_review(
        conn,
        &row.review_id,
        &Finalize {
            outcome,
            counts,
            outbound_sha: &out_sha,
            outbound_len: outbound.len(),
            provider_id: input.provider_id,
        },
    )
    .await
    {
        Ok(true) => {
            drop_held(&row.review_id);
            log::info!(
                "step_review_applied: run={} action={} review={} reused=false outcome={} \
                 asked={} released={} generalized={} removed={} outbound_sha={}",
                input.run_id,
                input.action_id,
                row.review_id,
                outcome.as_str(),
                row.spans.asked.len(),
                counts.released,
                counts.generalized,
                counts.removed,
                short(&out_sha)
            );
            Existing::Decided(ReviewDecision::Send(ReviewedPrompt::new(outbound)))
        }
        // No longer pending (answered, refused or superseded elsewhere) or the
        // write failed: nothing is sent on the strength of it.
        Ok(false) | Err(_) => {
            Existing::Decided(block(conn, input, sha, BlockReason::RecordUnavailable).await)
        }
    }
}

// ---------------------------------------------------------------------------
// The executor's door
// ---------------------------------------------------------------------------

/// The one call execute_once makes for a hosted step. A bound API Chat run takes
/// its already reviewed prompt. Any other hosted step is reviewed; a park comes
/// back as the FailureResult the step returns.
pub async fn review_hosted_step<L: DisclosureLogger>(
    ctx: &StepContext,
    treatment: &StepTreatment,
    provider_id: Option<&str>,
    prompt: &str,
    gateway: &PrivacyGateway<L>,
    app_handle: Option<&tauri::AppHandle<tauri::Wry>>,
) -> Result<ReviewedPrompt, FailureResult> {
    if let Some(bound) = &ctx.bound {
        return Ok(ReviewedPrompt::for_bound_run(bound, prompt));
    }
    let scanner = Gate3Scanner {
        gateway,
        app_handle,
        focus_name: &ctx.focus_name,
        ceiling: treatment.access,
        execution_tier: treatment.execution_tier,
        user_id: &ctx.user_id,
        persona_id: &ctx.persona_id,
        key_hex: &ctx.key_hex,
    };
    let input = ReviewInput {
        run_id: &ctx.focus_run_id,
        action_id: &ctx.step.action_id,
        prompt,
        provider_id,
    };
    let mut conn =
        match output_store::open_outputs_db_retrying(&ctx.user_id, &ctx.persona_id, &ctx.key_hex)
            .await
        {
            Ok(c) => c,
            Err(_) => {
                log_blocked(&input, &sha256_hex(prompt), BlockReason::RecordUnavailable);
                return Err(park_result(
                    &Park::Blocked(BlockReason::RecordUnavailable),
                    &ctx.step.action_id,
                    &ctx.focus_id,
                ));
            }
        };
    match review_prompt(&mut conn, &scanner, &input).await {
        ReviewDecision::Send(p) => Ok(p),
        ReviewDecision::Park(park) => Err(park_result(&park, &ctx.step.action_id, &ctx.focus_id)),
    }
}

/// The step's failure result for a park. The code travels as data and the
/// plain_language is the stable code string, never prose (the copy is the
/// frontend's, keyed by the code), as for a spend block.
pub fn park_result(park: &Park, action_id: &str, focus_id: &str) -> FailureResult {
    let (code, action, severity, extra): (
        Code,
        FailureAction,
        FailureSeverity,
        Vec<(&str, String)>,
    ) = match park {
        Park::Pending { review_id } => (
            Code::StepReviewRequired,
            FailureAction::AwaitConsent,
            FailureSeverity::Pause,
            vec![("review_id", review_id.clone())],
        ),
        Park::FilterUnavailable => (
            Code::StepReviewPrivacyFilterUnavailable,
            FailureAction::AwaitUser,
            FailureSeverity::Require,
            vec![("reason", "filter_unavailable".to_owned())],
        ),
        Park::TooLong => (
            Code::StepReviewTooLong,
            FailureAction::AwaitUser,
            FailureSeverity::Require,
            vec![("reason", "prompt_too_long".to_owned())],
        ),
        Park::Blocked(reason) => (
            Code::StepReviewBlocked,
            FailureAction::AwaitUser,
            FailureSeverity::Require,
            vec![("reason", reason.as_str().to_owned())],
        ),
    };
    let mut meta: HashMap<String, serde_json::Value> = HashMap::new();
    meta.insert("code".to_owned(), serde_json::json!(code.as_str()));
    meta.insert("action_id".to_owned(), serde_json::json!(action_id));
    for (k, v) in extra {
        meta.insert(k.to_owned(), serde_json::json!(v));
    }
    FailureResult {
        action,
        failure_mode: Some("F4".to_owned()),
        plain_language: code.as_str().to_owned(),
        is_recoverable: true,
        severity,
        action_id: Some(action_id.to_owned()),
        focus_id: Some(focus_id.to_owned()),
        metadata: Some(meta),
    }
}

// ---------------------------------------------------------------------------
// The IPC side: what is waiting, and the refusal
// ---------------------------------------------------------------------------

/// One flagged span as the front end gets it: ConsentSpanItem with u32 byte
/// offsets, because specta forbids exporting usize across IPC (the event payload,
/// which is not exported, keeps the original).
#[derive(Debug, Clone, Serialize, Type)]
pub struct PendingStepReviewSpan {
    pub span_id: String,
    pub category: String,
    pub user_label: String,
    pub original_text: String,
    pub suggestion: Option<String>,
    pub start_byte: u32,
    pub end_byte: u32,
    pub score: f32,
    pub review_tier: crate::conductor::privacy::types::ReviewTier,
    pub fact_key: Option<String>,
}

impl PendingStepReviewSpan {
    fn from_item(s: &ConsentSpanItem) -> Self {
        Self {
            span_id: s.span_id.clone(),
            category: s.category.clone(),
            user_label: s.user_label.clone(),
            original_text: s.original_text.clone(),
            suggestion: s.suggestion.clone(),
            start_byte: u32::try_from(s.start_byte).unwrap_or(u32::MAX),
            end_byte: u32::try_from(s.end_byte).unwrap_or(u32::MAX),
            score: s.score,
            review_tier: s.review_tier.clone(),
            fact_key: s.fact_key.clone(),
        }
    }
}

/// A review waiting for the user's answers, as the front end gets it.
#[derive(Debug, Clone, Serialize, Type)]
pub struct PendingStepReview {
    pub review_id: String,
    pub run_id: String,
    pub action_id: String,
    /// "ready": `spans` can be shown. "needs_resume": the text of the flagged
    /// spans is not held (the app restarted), so `spans` is empty and a resume asks
    /// again.
    pub state: String,
    pub spans: Vec<PendingStepReviewSpan>,
}

pub const STATE_READY: &str = "ready";
pub const STATE_NEEDS_RESUME: &str = "needs_resume";

/// A pending review as the front end gets it: with the text of its flagged spans
/// when that is held in memory and still matches the stored descriptors (same
/// prompt hash, same ids and offsets), otherwise `needs_resume`.
fn view_of(row: StepReviewRow) -> PendingStepReview {
    let held = held_spans(&row.review_id).filter(|(sha, spans)| {
        *sha == row.prompt_sha
            && spans.len() == row.spans.asked.len()
            && spans.iter().zip(&row.spans.asked).all(|(h, a)| {
                h.span_id == a.span_id && h.start_byte == a.start_byte && h.end_byte == a.end_byte
            })
    });
    let (state, spans) = match held {
        Some((_, spans)) => (
            STATE_READY,
            spans.iter().map(PendingStepReviewSpan::from_item).collect(),
        ),
        None => (STATE_NEEDS_RESUME, Vec::new()),
    };
    PendingStepReview {
        review_id: row.review_id,
        run_id: row.focus_run_id,
        action_id: row.action_id,
        state: state.to_owned(),
        spans,
    }
}

/// The newest pending review of a run.
pub async fn pending_view(
    conn: &mut SqliteConnection,
    run_id: &str,
) -> Result<Option<PendingStepReview>, store::StepReviewStoreError> {
    Ok(store::pending_review_for_run(conn, run_id)
        .await?
        .map(view_of))
}

/// Every pending review of the persona whose run is still open, oldest first: what
/// a front end that has just started needs to find a review it never saw an event
/// for (list_pending_step_reviews).
pub async fn pending_views(
    conn: &mut SqliteConnection,
) -> Result<Vec<PendingStepReview>, store::StepReviewStoreError> {
    Ok(store::list_open_pending_reviews(conn)
        .await?
        .into_iter()
        .map(view_of)
        .collect())
}

/// The user refused to send: the newest pending review of the run becomes
/// `refused`. Returns whether there was one. The caller cancels the run.
pub async fn decline_pending(
    conn: &mut SqliteConnection,
    run_id: &str,
) -> Result<bool, store::StepReviewStoreError> {
    let Some(row) = store::pending_review_for_run(conn, run_id).await? else {
        return Ok(false);
    };
    let changed = store::refuse_pending_review(conn, &row.review_id).await?;
    if changed {
        drop_held(&row.review_id);
        log::info!(
            "step_review_refused: run={} action={} review={} prompt_sha={}",
            row.focus_run_id,
            row.action_id,
            row.review_id,
            short(&row.prompt_sha)
        );
    }
    Ok(changed)
}

/// cancel_run: pending reviews of the run are superseded and their held text
/// dropped (the cache is only a convenience). Returns how many were superseded.
pub async fn supersede_for_cancel(
    conn: &mut SqliteConnection,
    run_id: &str,
) -> Result<usize, store::StepReviewStoreError> {
    let ids = store::supersede_pending_for_run(conn, run_id).await?;
    ids.iter().for_each(|id| drop_held(id));
    Ok(ids.len())
}

// ---------------------------------------------------------------------------
// Test support: an injectable scanner (decisions.id=943 point 9)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::conductor::privacy::types::ReviewTier;

    type ScanFn = dyn Fn(&str) -> Result<Gate3Result, DisclosureLogWriteError> + Send + Sync;

    /// A scanner that answers what the test says and counts its calls.
    pub struct FakeScanner {
        pub available: bool,
        pub has_handle: bool,
        result: Box<ScanFn>,
        calls: AtomicUsize,
    }

    impl FakeScanner {
        fn with(
            result: impl Fn(&str) -> Result<Gate3Result, DisclosureLogWriteError>
                + Send
                + Sync
                + 'static,
        ) -> Self {
            Self {
                available: true,
                has_handle: true,
                result: Box::new(result),
                calls: AtomicUsize::new(0),
            }
        }

        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// The filter finds nothing.
        pub fn clean() -> Self {
            Self::with(|_| {
                Ok(Gate3Result {
                    approved: true,
                    ..Gate3Result::default()
                })
            })
        }

        /// The user must decide on `spans`.
        pub fn pending(spans: Vec<ConsentSpanItem>) -> Self {
            Self::pending_with_auto(spans, vec![])
        }

        pub fn pending_with_auto(spans: Vec<ConsentSpanItem>, auto: Vec<AutoResolvedSpan>) -> Self {
            Self::with(move |_| {
                Ok(Gate3Result {
                    pending_consent: true,
                    review_spans: spans.clone(),
                    auto_resolved: auto.clone(),
                    ..Gate3Result::default()
                })
            })
        }

        /// Every flagged span was settled by a standing decision.
        pub fn standing(auto: Vec<AutoResolvedSpan>) -> Self {
            Self::with(move |_| {
                Ok(Gate3Result {
                    approved: true,
                    auto_resolved: auto.clone(),
                    ..Gate3Result::default()
                })
            })
        }

        pub fn blocked() -> Self {
            Self::with(|_| {
                Ok(Gate3Result {
                    blocked: true,
                    ..Gate3Result::default()
                })
            })
        }

        pub fn timed_out() -> Self {
            Self::with(|_| {
                Ok(Gate3Result {
                    blocked: true,
                    timeout: true,
                    ..Gate3Result::default()
                })
            })
        }

        /// gate3's own result is none of approved, pending or blocked.
        pub fn indeterminate() -> Self {
            Self::with(|_| Ok(Gate3Result::default()))
        }

        /// The disclosure-log write failed.
        pub fn log_failure() -> Self {
            Self::with(|_| {
                Err(DisclosureLogWriteError::new(std::io::Error::other(
                    "log down",
                )))
            })
        }
    }

    #[async_trait]
    impl ReviewScanner for FakeScanner {
        fn filter_available(&self) -> bool {
            self.available
        }
        fn has_app_handle(&self) -> bool {
            self.has_handle
        }
        async fn scan(
            &self,
            _run_id: &str,
            _action_id: &str,
            content: &str,
        ) -> Result<Gate3Result, DisclosureLogWriteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result)(content)
        }
    }

    /// A flagged span over the first occurrence of `text` in `prompt`.
    pub fn asked_span(span_id: &str, prompt: &str, text: &str) -> ConsentSpanItem {
        let start = prompt.find(text).expect("the span text is in the prompt");
        ConsentSpanItem {
            span_id: span_id.to_owned(),
            category: "private_person".to_owned(),
            user_label: "Person name".to_owned(),
            original_text: text.to_owned(),
            suggestion: Some("[person]".to_owned()),
            start_byte: start,
            end_byte: start + text.len(),
            score: 0.95,
            review_tier: ReviewTier::High,
            fact_key: None,
        }
    }

    /// A span a standing decision settled, over the first occurrence of `text`.
    pub fn settled_span(
        prompt: &str,
        text: &str,
        decision: &str,
        replacement: Option<&str>,
    ) -> AutoResolvedSpan {
        let start = prompt.find(text).expect("the span text is in the prompt");
        AutoResolvedSpan {
            start_byte: start as u32,
            end_byte: (start + text.len()) as u32,
            decision: decision.to_owned(),
            suggestion_text: replacement.map(str::to_owned),
            user_modified_text: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{asked_span, settled_span, FakeScanner};
    use super::*;
    use crate::persistence::session_store::test_db::mem_outputs_conn;

    const RUN: &str = "run-1";
    const ACTION: &str = "draft";
    const PROMPT: &str = "Write to Ann Smith at 12 Oak Road about the invoice";

    async fn conn() -> SqliteConnection {
        let mut c = mem_outputs_conn().await;
        sqlx::query("INSERT INTO focus_runs (id, focus_id, started_at) VALUES (?, 'f', 't')")
            .bind(RUN)
            .execute(&mut c)
            .await
            .unwrap();
        c
    }

    fn input(prompt: &str) -> ReviewInput<'_> {
        ReviewInput {
            run_id: RUN,
            action_id: ACTION,
            prompt,
            provider_id: Some("prov"),
        }
    }

    fn answer(span_id: &str, decision: &str, modified: Option<&str>) -> RecordedSpanDecision {
        RecordedSpanDecision {
            span_id: span_id.to_owned(),
            decision: decision.to_owned(),
            suggestion_text: Some("[person]".to_owned()),
            user_modified_text: modified.map(str::to_owned),
        }
    }

    /// Record answers the way submit_element_consent_decision does (one row per span).
    async fn record(c: &mut SqliteConnection, answers: &[RecordedSpanDecision]) {
        for a in answers {
            sqlx::query(
                "INSERT INTO consent_decisions
                 (id, focus_run_id, decision_type, decision, span_id, suggestion_text,
                  user_modified_text, created_at)
                 VALUES (?, ?, 'element_consent', ?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(RUN)
            .bind(&a.decision)
            .bind(&a.span_id)
            .bind(&a.suggestion_text)
            .bind(&a.user_modified_text)
            .bind(crate::providers::utils::now())
            .execute(&mut *c)
            .await
            .unwrap();
        }
    }

    fn sent(d: ReviewDecision) -> String {
        match d {
            ReviewDecision::Send(p) => p.as_str().to_owned(),
            other => panic!("expected the prompt to be sent, got {other:?}"),
        }
    }

    fn parked(d: ReviewDecision) -> Park {
        match d {
            ReviewDecision::Park(p) => p,
            other => panic!("expected a park, got {other:?}"),
        }
    }

    async fn rows(c: &mut SqliteConnection) -> Vec<StepReviewRow> {
        store::list_reviews(c, RUN, ACTION).await.unwrap()
    }

    // -- the verdicts -------------------------------------------------------------

    #[tokio::test]
    async fn a_clean_prompt_is_sent_as_it_is_and_recorded_clean() {
        let mut c = conn().await;
        let scanner = FakeScanner::clean();
        let out = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(out, PROMPT);
        let r = rows(&mut c).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].outcome, ReviewOutcome::Clean);
        assert_eq!(r[0].prompt_sha, sha256_hex(PROMPT));
        assert_eq!(
            r[0].outbound_sha.as_deref(),
            Some(sha256_hex(PROMPT).as_str())
        );
        assert_eq!(r[0].outbound_len, Some(PROMPT.len()));
        assert_eq!(r[0].counts, Counts::default());
        assert_eq!(r[0].provider_id.as_deref(), Some("prov"));
    }

    #[tokio::test]
    async fn standing_decisions_that_settle_every_span_proceed_with_no_park() {
        let mut c = conn().await;
        let scanner = FakeScanner::standing(vec![
            settled_span(PROMPT, "Ann Smith", "generalize", Some("[person]")),
            settled_span(PROMPT, "12 Oak Road", "keep_private", None),
        ]);
        let out = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(out, "Write to [person] at [removed] about the invoice");
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::StandingApplied);
        assert_eq!(r[0].counts.total, 2);
        assert_eq!(r[0].counts.auto_resolved, 2);
        assert_eq!(r[0].counts.generalized, 1);
        assert_eq!(r[0].counts.removed, 1);
        assert_eq!(
            r[0].outbound_sha.as_deref(),
            Some(sha256_hex(&out).as_str())
        );
        // The row holds the replacement, never the original words.
        let json = serde_json::to_string(&r[0].spans).unwrap();
        assert!(!json.contains("Ann") && !json.contains("Oak"), "{json}");
    }

    #[tokio::test]
    async fn flagged_spans_park_the_step_with_a_pending_row_and_held_text() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-park", PROMPT, "Ann Smith")]);
        let park = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        let Park::Pending { review_id } = park else {
            panic!("expected pending")
        };
        let r = rows(&mut c).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].review_id, review_id);
        assert_eq!(r[0].outcome, ReviewOutcome::Pending);
        assert_eq!(r[0].spans.asked.len(), 1);
        assert_eq!(r[0].spans.asked[0].span_id, "s-park");
        assert!(r[0].outbound_sha.is_none(), "nothing released yet");
        let json = serde_json::to_string(&r[0].spans).unwrap();
        assert!(!json.contains("Ann"), "the row holds no text: {json}");
        let held = held_spans(&review_id).expect("held in memory");
        assert_eq!(held.0, sha256_hex(PROMPT));
        assert_eq!(held.1[0].original_text, "Ann Smith");
    }

    // -- fail closed --------------------------------------------------------------

    #[tokio::test]
    async fn an_unavailable_filter_parks_the_step_and_calls_nothing() {
        let mut c = conn().await;
        let mut scanner = FakeScanner::clean();
        scanner.available = false;
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(p, Park::FilterUnavailable);
        assert_eq!(
            scanner.calls(),
            0,
            "gate3 is not trusted without the filter"
        );
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Blocked);
        assert_eq!(r[0].reason.as_deref(), Some("filter_unavailable"));
    }

    /// items.id=923: a prompt over the cap is refused before anything else
    /// looks at it, with its own code, and nothing is scanned or sent.
    #[tokio::test]
    async fn a_prompt_over_the_cap_is_refused_before_any_scan() {
        use crate::conductor::privacy::MAX_REVIEW_BYTES;
        let mut c = conn().await;
        let scanner = FakeScanner::clean();
        let long = "a".repeat(MAX_REVIEW_BYTES + 1);
        let p = parked(review_prompt(&mut c, &scanner, &input(&long)).await);
        assert_eq!(p, Park::TooLong);
        assert_eq!(scanner.calls(), 0, "no chunk is scanned");
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Blocked);
        assert_eq!(r[0].reason.as_deref(), Some("prompt_too_long"));
        assert_eq!(r[0].outbound_sha, None, "nothing was released");

        // At the cap it is reviewed as usual.
        let mut c = conn().await;
        let at_cap = "a".repeat(MAX_REVIEW_BYTES);
        let d = review_prompt(&mut c, &scanner, &input(&at_cap)).await;
        assert!(matches!(d, ReviewDecision::Send(_)), "{d:?}");
        assert_eq!(scanner.calls(), 1);
    }

    #[tokio::test]
    async fn no_app_handle_parks_the_step_and_calls_nothing() {
        let mut c = conn().await;
        let mut scanner = FakeScanner::clean();
        scanner.has_handle = false;
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(p, Park::Blocked(BlockReason::NoAppHandle));
        assert_eq!(scanner.calls(), 0, "without a handle gate3 would not scan");
    }

    #[tokio::test]
    async fn each_failed_scan_parks_with_its_own_reason() {
        let cases: Vec<(FakeScanner, BlockReason)> = vec![
            (FakeScanner::log_failure(), BlockReason::AuditLog),
            (FakeScanner::timed_out(), BlockReason::Timeout),
            (FakeScanner::blocked(), BlockReason::FilterBlocked),
            (FakeScanner::indeterminate(), BlockReason::Indeterminate),
        ];
        for (scanner, reason) in cases {
            let mut c = conn().await;
            let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
            assert_eq!(p, Park::Blocked(reason), "{reason:?}");
            let r = rows(&mut c).await;
            assert_eq!(r[0].outcome, ReviewOutcome::Blocked);
            assert_eq!(r[0].reason.as_deref(), Some(reason.as_str()));
        }
    }

    #[tokio::test]
    async fn spans_that_cannot_be_applied_park_before_the_user_is_asked() {
        // A span past the end of the text, one on the wrong text, one inside a
        // multi-byte character.
        let mut past = asked_span("s1", PROMPT, "Ann Smith");
        past.end_byte = PROMPT.len() + 10;
        let mut wrong = asked_span("s1", PROMPT, "Ann Smith");
        wrong.original_text = "Bob Jones".to_owned();
        let prompt = "Écrire à Ann";
        let mut mid = asked_span("s1", prompt, "Ann");
        mid.start_byte = 1;
        mid.end_byte = 4;
        mid.original_text = "x".to_owned(); // bytes 1..4 start inside 'É': get() is None
        for (p, span) in [(PROMPT, past), (PROMPT, wrong), (prompt, mid)] {
            let mut c = conn().await;
            let scanner = FakeScanner::pending(vec![span]);
            let parked = parked(review_prompt(&mut c, &scanner, &input(p)).await);
            assert_eq!(parked, Park::Blocked(BlockReason::Unappliable));
            assert!(
                store::pending_review_for_run(&mut c, RUN)
                    .await
                    .unwrap()
                    .is_none(),
                "nothing pending: the user was never asked"
            );
        }
    }

    #[tokio::test]
    async fn a_pending_review_with_nothing_to_ask_is_blocked_not_parked_for_good() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![]);
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(p, Park::Blocked(BlockReason::Indeterminate));
        assert!(
            store::pending_review_for_run(&mut c, RUN)
                .await
                .unwrap()
                .is_none(),
            "no pending row the user could never answer"
        );
    }

    #[tokio::test]
    async fn an_unreadable_record_parks_the_step_and_calls_nothing() {
        let mut c = conn().await;
        sqlx::query("DROP TABLE step_reviews")
            .execute(&mut c)
            .await
            .unwrap();
        let scanner = FakeScanner::clean();
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(p, Park::Blocked(BlockReason::RecordUnavailable));
        assert_eq!(scanner.calls(), 0);
    }

    // -- park and resume ----------------------------------------------------------

    async fn park_one(c: &mut SqliteConnection, scanner: &FakeScanner) -> String {
        match parked(review_prompt(c, scanner, &input(PROMPT)).await) {
            Park::Pending { review_id } => review_id,
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_resume_applies_the_recorded_answers_server_side() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending_with_auto(
            vec![
                asked_span("s-a", PROMPT, "Ann Smith"),
                asked_span("s-b", PROMPT, "12 Oak Road"),
            ],
            vec![settled_span(
                PROMPT,
                "invoice",
                "generalize",
                Some("[document]"),
            )],
        );
        let id = park_one(&mut c, &scanner).await;
        assert_eq!(scanner.calls(), 1);

        record(
            &mut c,
            &[
                answer("s-a", "generalize", None),
                answer("s-b", "release_original", None),
            ],
        )
        .await;
        let out = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(out, "Write to [person] at 12 Oak Road about the [document]");
        assert_eq!(
            scanner.calls(),
            1,
            "the answers are applied, nobody is asked again"
        );

        let r = rows(&mut c).await;
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].review_id, id);
        assert_eq!(r[0].outcome, ReviewOutcome::ApprovedAsShown);
        assert_eq!(
            r[0].outbound_sha.as_deref(),
            Some(sha256_hex(&out).as_str())
        );
        assert_eq!(r[0].counts.total, 3);
        assert_eq!(r[0].counts.auto_resolved, 1);
        assert_eq!(r[0].counts.released, 1);
        assert_eq!(r[0].counts.generalized, 2);
        assert!(
            held_spans(&id).is_none(),
            "the held text is dropped when applied"
        );
    }

    /// A restart loses the held text, never the answers: the step re-renders, finds
    /// the pending row by its hash and applies what was recorded.
    #[tokio::test]
    async fn a_review_parked_before_a_restart_is_answered_after_it() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        let id = park_one(&mut c, &scanner).await;
        record(&mut c, &[answer("s-a", "generalize", Some("a client"))]).await;
        drop_held(&id); // the process memory is gone
        let out = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(out, "Write to a client at 12 Oak Road about the invoice");
        assert_eq!(scanner.calls(), 1, "no second review");
        assert_eq!(rows(&mut c).await[0].outcome, ReviewOutcome::Edited);
    }

    #[tokio::test]
    async fn an_unanswered_span_is_removed_and_never_sent() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![
            asked_span("s-a", PROMPT, "Ann Smith"),
            asked_span("s-b", PROMPT, "12 Oak Road"),
        ]);
        park_one(&mut c, &scanner).await;
        record(&mut c, &[answer("s-a", "release_original", None)]).await;
        let out = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(out, "Write to Ann Smith at [removed] about the invoice");
        assert!(!out.contains("Oak"));
        assert_eq!(rows(&mut c).await[0].outcome, ReviewOutcome::Redacted);
    }

    #[tokio::test]
    async fn a_resume_with_no_answer_asks_again_and_supersedes_the_old_review() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        let first = park_one(&mut c, &scanner).await;
        let second = park_one(&mut c, &scanner).await;
        assert_ne!(first, second);
        assert_eq!(scanner.calls(), 2, "a restart or a closed modal asks again");
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Superseded);
        assert_eq!(r[1].outcome, ReviewOutcome::Pending);
        assert!(held_spans(&first).is_none());
        assert!(held_spans(&second).is_some());
    }

    #[tokio::test]
    async fn a_changed_prompt_is_a_new_review_never_the_old_answer() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        park_one(&mut c, &scanner).await;
        record(&mut c, &[answer("s-a", "release_original", None)]).await;

        // The text changed between the park and the resume.
        let changed = "Write to Ann Smith at 12 Oak Road about the refund";
        let scanner2 = FakeScanner::pending(vec![asked_span("s-new", changed, "Ann Smith")]);
        let p = parked(review_prompt(&mut c, &scanner2, &input(changed)).await);
        assert!(
            matches!(p, Park::Pending { .. }),
            "the old answer is not applied"
        );
        assert_eq!(scanner2.calls(), 1);
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Superseded);
        assert_eq!(r[1].prompt_sha, sha256_hex(changed));
    }

    #[tokio::test]
    async fn a_retry_reuses_an_applied_review_and_never_scans_again() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        park_one(&mut c, &scanner).await;
        record(&mut c, &[answer("s-a", "keep_private", None)]).await;
        let first = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        let again = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        let third = sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(first, again);
        assert_eq!(first, third);
        assert_eq!(scanner.calls(), 1, "neither skipped nor repeated");
        assert_eq!(rows(&mut c).await.len(), 1);
    }

    #[tokio::test]
    async fn a_clean_review_is_reused_for_a_retry_too() {
        let mut c = conn().await;
        let scanner = FakeScanner::clean();
        sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(scanner.calls(), 1);
        assert_eq!(rows(&mut c).await.len(), 1);
    }

    #[tokio::test]
    async fn a_reused_review_must_still_hash_to_what_was_released() {
        let mut c = conn().await;
        let scanner = FakeScanner::clean();
        sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        // The stored outbound hash no longer matches what the descriptors rebuild.
        sqlx::query("UPDATE step_reviews SET outbound_sha = ?")
            .bind("f".repeat(64))
            .execute(&mut c)
            .await
            .unwrap();
        sent(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(
            scanner.calls(),
            2,
            "a row that cannot prove its text is reviewed again"
        );
    }

    #[tokio::test]
    async fn answers_that_cannot_be_applied_block_the_pending_review() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        let id = park_one(&mut c, &scanner).await;
        record(&mut c, &[answer("s-a", "release_original", None)]).await;
        // The stored offsets are corrupted after the park.
        let mut row = store::pending_review_for_run(&mut c, RUN)
            .await
            .unwrap()
            .unwrap();
        row.spans.asked[0].end_byte = 9999;
        sqlx::query("UPDATE step_reviews SET spans_json = ? WHERE review_id = ?")
            .bind(serde_json::to_string(&row.spans).unwrap())
            .bind(&id)
            .execute(&mut c)
            .await
            .unwrap();
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert_eq!(p, Park::Blocked(BlockReason::Unappliable));
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Blocked);
        assert_eq!(r[0].reason.as_deref(), Some("unappliable"));
    }

    #[tokio::test]
    async fn a_refused_review_is_history_and_sends_nothing() {
        let mut c = conn().await;
        let scanner = FakeScanner::pending(vec![asked_span("s-a", PROMPT, "Ann Smith")]);
        let id = park_one(&mut c, &scanner).await;
        // The user refused (and answered) in another window while this attempt was
        // about to resume.
        record(&mut c, &[answer("s-a", "release_original", None)]).await;
        assert!(store::refuse_pending_review(&mut c, &id).await.unwrap());
        let p = parked(review_prompt(&mut c, &scanner, &input(PROMPT)).await);
        assert!(
            matches!(p, Park::Pending { .. }),
            "the refused review is not reused: a new review asks again"
        );
        let r = rows(&mut c).await;
        assert_eq!(r[0].outcome, ReviewOutcome::Refused);
        assert_eq!(r[1].outcome, ReviewOutcome::Pending);
    }

    // -- the outcome vocabulary ---------------------------------------------------

    fn spans(asked: &[&str], settled: Vec<SettledSpan>) -> StoredSpans {
        StoredSpans {
            asked: asked
                .iter()
                .enumerate()
                .map(|(i, id)| AskedSpan {
                    span_id: (*id).to_owned(),
                    start_byte: i * 2,
                    end_byte: i * 2 + 1,
                    category: "c".into(),
                    review_tier: "high".into(),
                })
                .collect(),
            settled,
        }
    }

    #[test]
    fn outcomes_are_derived_with_edited_winning_over_redacted() {
        let s = spans(&["a", "b", "c"], vec![]);
        // Everything released or generalised as suggested.
        let (o, c) = derive_outcome(
            &s,
            &[
                answer("a", "release_original", None),
                answer("b", "generalize", None),
                answer("c", "generalize", Some("[person]")),
            ],
        );
        assert_eq!(o, ReviewOutcome::ApprovedAsShown);
        assert_eq!((c.released, c.generalized, c.removed), (1, 2, 0));

        // One removed (kept private), none edited.
        let (o, c) = derive_outcome(
            &s,
            &[
                answer("a", "release_original", None),
                answer("b", "keep_private", None),
                answer("c", "generalize", None),
            ],
        );
        assert_eq!(o, ReviewOutcome::Redacted);
        assert_eq!(c.removed, 1);

        // One unanswered counts as removed.
        let (o, c) = derive_outcome(&s, &[answer("a", "release_original", None)]);
        assert_eq!(o, ReviewOutcome::Redacted);
        assert_eq!((c.released, c.removed), (1, 2));

        // One edited, one removed: edited wins, the counts keep the detail.
        let (o, c) = derive_outcome(
            &s,
            &[
                answer("a", "generalize", Some("a woman I know")),
                answer("b", "keep_private", None),
                answer("c", "release_original", None),
            ],
        );
        assert_eq!(o, ReviewOutcome::Edited);
        assert_eq!((c.generalized, c.removed, c.released), (1, 1, 1));

        // A generalise with no replacement text at all is a removal.
        let (o, c) = derive_outcome(
            &spans(&["a"], vec![]),
            &[RecordedSpanDecision {
                span_id: "a".into(),
                decision: "generalize".into(),
                suggestion_text: None,
                user_modified_text: None,
            }],
        );
        assert_eq!(o, ReviewOutcome::Redacted);
        assert_eq!(c.removed, 1);

        // An unknown decision string is the most private one.
        let (o, _) = derive_outcome(&spans(&["a"], vec![]), &[answer("a", "weird", None)]);
        assert_eq!(o, ReviewOutcome::Redacted);
    }

    #[test]
    fn no_spans_is_clean_and_settled_only_is_standing_applied() {
        let (o, c) = derive_outcome(&StoredSpans::default(), &[]);
        assert_eq!((o, c), (ReviewOutcome::Clean, Counts::default()));
        let settled = vec![SettledSpan {
            start_byte: 0,
            end_byte: 1,
            decision: "release_original".into(),
            suggestion_text: None,
            user_modified_text: None,
        }];
        let (o, c) = derive_outcome(&spans(&[], settled), &[]);
        assert_eq!(o, ReviewOutcome::StandingApplied);
        assert_eq!((c.total, c.auto_resolved, c.released), (1, 1, 1));
    }

    // -- offsets through redact::apply --------------------------------------------

    #[test]
    fn offsets_are_bytes_through_multibyte_text() {
        let prompt = "Écris à José García demain";
        let start = prompt.find("José García").unwrap();
        let stored = StoredSpans {
            asked: vec![AskedSpan {
                span_id: "x".into(),
                start_byte: start,
                end_byte: start + "José García".len(),
                category: "private_person".into(),
                review_tier: "high".into(),
            }],
            settled: vec![],
        };
        let out = outbound_for(
            prompt,
            &stored,
            &[answer("x", "generalize", Some("[a friend]"))],
        );
        assert_eq!(out.as_deref(), Some("Écris à [a friend] demain"));
        assert_eq!(
            outbound_for(prompt, &stored, &[]).as_deref(),
            Some("Écris à [removed] demain")
        );
    }

    // -- parks as failure results ---------------------------------------------------

    #[test]
    fn a_park_is_a_recoverable_failure_with_its_code_as_data() {
        let pending = park_result(
            &Park::Pending {
                review_id: "r1".into(),
            },
            "act",
            "foc",
        );
        assert_eq!(pending.action, FailureAction::AwaitConsent);
        assert_eq!(pending.severity, FailureSeverity::Pause);
        assert!(pending.is_recoverable);
        let m = pending.metadata.as_ref().unwrap();
        assert_eq!(m["code"], "step_review.required");
        assert_eq!(m["review_id"], "r1");

        let unavailable = park_result(&Park::FilterUnavailable, "act", "foc");
        assert_eq!(unavailable.action, FailureAction::AwaitUser);
        assert_eq!(unavailable.severity, FailureSeverity::Require);
        assert!(unavailable.is_recoverable);
        assert_eq!(
            unavailable.metadata.as_ref().unwrap()["code"],
            "step_review.privacy_filter_unavailable"
        );

        let too_long = park_result(&Park::TooLong, "act", "foc");
        assert_eq!(too_long.action, FailureAction::AwaitUser);
        assert_eq!(too_long.severity, FailureSeverity::Require);
        assert!(too_long.is_recoverable);
        let m = too_long.metadata.as_ref().unwrap();
        assert_eq!(m["code"], "step_review.too_long");
        assert_eq!(m["reason"], "prompt_too_long");
        assert_eq!(too_long.plain_language, "step_review.too_long");

        for reason in [
            BlockReason::NoAppHandle,
            BlockReason::Timeout,
            BlockReason::FilterBlocked,
            BlockReason::Indeterminate,
            BlockReason::AuditLog,
            BlockReason::RecordUnavailable,
            BlockReason::Unappliable,
        ] {
            let b = park_result(&Park::Blocked(reason), "act", "foc");
            assert_eq!(b.action, FailureAction::AwaitUser);
            assert!(b.is_recoverable);
            let m = b.metadata.as_ref().unwrap();
            assert_eq!(m["code"], "step_review.blocked");
            assert_eq!(m["reason"], reason.as_str());
            assert_eq!(b.plain_language, "step_review.blocked");
        }
    }

    // -- the prompt type and the held text ----------------------------------------

    /// Every step_review code has its placeholder copy under step_review.stop
    /// (Chat-BRAND owns the wording).
    #[test]
    fn every_step_review_code_has_a_stop_key_in_en_json() {
        let en: serde_json::Value =
            serde_json::from_str(include_str!("../../../frontend/src/i18n/en.json")).unwrap();
        for code in Code::ALL
            .iter()
            .filter(|c| c.as_str().starts_with("step_review."))
        {
            let leaf = code.as_str().trim_start_matches("step_review.");
            assert!(
                en["step_review"]["stop"][leaf].is_string(),
                "en.json has no step_review.stop.{leaf}"
            );
        }
    }

    #[test]
    fn a_reviewed_prompt_debug_shows_a_count_and_a_hash_never_the_text() {
        let p = ReviewedPrompt::for_test("ZX9-SECRET-WORDS");
        let shown = format!("{p:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(shown.contains("bytes: 16"), "{shown}");
        assert_eq!(p.sha(), sha256_hex("ZX9-SECRET-WORDS"));
    }

    #[test]
    fn held_text_is_bounded_and_drops_the_oldest() {
        let mut map = HashMap::new();
        let mut seq = 0;
        for i in 0..10 {
            insert_bounded(&mut map, &mut seq, 4, &format!("r{i}"), "sha", vec![]);
        }
        assert_eq!(map.len(), 4);
        let mut kept: Vec<_> = map.keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, ["r6", "r7", "r8", "r9"]);
    }

    // -- source scans --------------------------------------------------------------

    /// The production part of a source file: everything before its test module
    /// (or, for this file, before the test support). A `#[cfg(test)]` on a single
    /// statement earlier in a file is production code and stays in.
    fn production(src: &str) -> &str {
        let cuts = [
            "// Test support: an injectable scanner",
            "\n#[cfg(test)]\nmod tests",
            "\n#[cfg(test)]\n#[path",
        ];
        let end = cuts
            .iter()
            .filter_map(|c| src.find(c))
            .min()
            .unwrap_or(src.len());
        &src[..end]
    }

    fn rust_sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push((
                        path.to_string_lossy().into_owned(),
                        std::fs::read_to_string(&path).unwrap(),
                    ));
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out
    }

    /// Nothing but step_review builds the text a hosted call carries, and the one
    /// door that does not review (a bound API Chat run) is named once.
    #[test]
    fn only_step_review_constructs_a_reviewed_prompt() {
        for (path, src) in rust_sources() {
            if path.ends_with("conductor/step_review.rs") {
                continue;
            }
            let prod = production(&src);
            assert!(
                !prod.contains("ReviewedPrompt::new(") && !prod.contains("ReviewedPrompt {"),
                "{path} builds a ReviewedPrompt"
            );
            assert!(
                !prod.contains("ReviewedPrompt::for_test("),
                "{path}: the test constructor is test-only"
            );
            if !path.ends_with("conductor/executor.rs") {
                assert!(
                    !prod.contains("for_bound_run("),
                    "{path} takes the bound-run door"
                );
            }
        }
        // Inside step_review, for_bound_run is called once: review_hosted_step,
        // for a run whose ctx.bound is Some.
        let me = production(include_str!("step_review.rs"));
        assert_eq!(me.matches("ReviewedPrompt::for_bound_run(").count(), 1);
        assert!(me.contains("if let Some(bound) = &ctx.bound {\n        return Ok(ReviewedPrompt::for_bound_run(bound, prompt));"));
    }

    /// hosted_step_call has one production call site, in execute_once, reached only
    /// with a reviewed prompt, and no other file calls it.
    #[test]
    fn hosted_step_call_is_on_the_one_send_path() {
        for (path, src) in rust_sources() {
            if path.ends_with("conductor/executor.rs") {
                continue;
            }
            assert!(
                !production(&src).contains("hosted_step_call("),
                "{path} calls hosted_step_call"
            );
        }
        let exec = production(include_str!("executor.rs"));
        assert_eq!(
            exec.matches("hosted_step_call(").count(),
            1,
            "one call site"
        );
        assert_eq!(exec.matches("fn hosted_step_call<").count(), 1);
        assert!(
            exec.contains(
                "if treatment.hosted {\n            match step_review::review_hosted_step("
            ),
            "the review fires on the derived hosted predicate"
        );
        // The review sits between the render and the context-window check, before
        // the inference slot.
        let render = exec.find("let prompt = if treatment.hosted {").unwrap();
        let review = exec.find("step_review::review_hosted_step(").unwrap();
        let ctx_check = exec
            .find("let mut ctx_status = check_context_window")
            .unwrap();
        let slot = exec
            .find("scheduler\n            .acquire_inference_slot")
            .unwrap();
        assert!(render < review && review < ctx_check && ctx_check < slot);
        // The request body takes its text from the ReviewedPrompt.
        assert!(exec.contains("prompt: reviewed.as_str().to_owned(),"));
        assert!(exec.contains("system: None,"));
    }

    #[test]
    fn the_gate_parameters_are_those_of_the_api_chat_review() {
        const {
            assert!(REVIEW_SEVERITY == 1);
            assert!(!REVIEW_SEVERITY_AUTHORITATIVE);
            assert!(REVIEW_TARGET_TIER == 3);
            assert!(REVIEW_DESTINATION_RISK == 3);
        }
        let me = production(include_str!("step_review.rs"));
        for needle in [
            "REVIEW_SEVERITY,\n",
            "REVIEW_TARGET_TIER,\n",
            "self.ceiling,\n",
            "Some(REVIEW_DESTINATION_RISK),\n",
            "REVIEW_SEVERITY_AUTHORITATIVE,\n",
        ] {
            assert!(me.contains(needle), "gate3 call lost {needle:?}");
        }
        // The ceiling argument is the derived access, never the run ceiling.
        assert!(me.contains("ceiling: treatment.access,"));
        assert!(!me.contains("effective_access"));
    }

    /// No log line carries prompt text: counts and hash prefixes only.
    #[test]
    fn the_logs_carry_no_prompt_text() {
        let me = production(include_str!("step_review.rs"));
        let mut checked = 0;
        for part in me.split("log::").skip(1) {
            let stmt = part.split(");").next().unwrap();
            checked += 1;
            for banned in [
                "input.prompt",
                "original_text",
                "outbound)",
                "&outbound",
                "text)",
            ] {
                assert!(!stmt.contains(banned), "a log line names {banned}: {stmt}");
            }
        }
        assert!(checked >= 6, "the six events are logged");
        for event in [
            "step_review_pf_unavailable",
            "step_review_blocked",
            "step_review_clean",
            "step_review_pending",
            "step_review_applied",
            "step_review_refused",
        ] {
            assert!(me.contains(event), "event {event} is missing");
        }
        assert!(!me.contains("println!") && !me.contains("dbg!"));
    }
}
