// src-tauri/src/persistence/dedup_store.rs
//
// cb-11 (Stage 2) — duplicate detection and user-authored resolution.
// items.id=128, decisions.id=502 (D6-460), decisions.id=621 §11.
//
// Stage 1 (source_registry_store.rs) established where records come from and
// what a refresh may do to them. This stage answers the other question
// decisions.id=502 poses: when two records might be the same thing, what
// does QR do about it?
//
// THE ANSWER, AND THE WHOLE POSTURE OF THIS MODULE: it surfaces, and stops.
// QR never auto-merges, never silently discards, and never decides that two
// records are the same. decisions.id=502 is unambiguous that the user is the
// sole authority on identity, and the reason is concrete: two records with
// near-identical names and 85% field overlap can still be categorically
// different things. A yeasted loaf and an unleavened one. Butter and oil.
// Confidence is not sameness, and this module is built so that no amount of
// confidence can trigger a write the user did not author.
//
// WHAT match_confidence IS FOR: ordering the review queue. Nothing else in
// this module branches on it.
//
// FOCUS-DECLARED, NOT FOCUS-SHAPED. decisions.id=502 defines six things a
// Focus must declare. They arrive here as FocusDedupDeclaration, supplied by
// the caller. Nothing in this file knows what a recipe is. Cooking is
// status='designed' and has recorded none of its declarations yet; when it
// does, it fills in a struct rather than editing this module.
//
// SECOND ADOPTER: decisions.id=617's synced household grants reconcile a
// recipient instance against the owner's through this same framework.
//
// WHAT IS NOT BUILT HERE, deliberately:
//   * Cross-source background dedup — decisions.id=502 defers it past R1.
//     Detection here is caller-triggered (at import, or on user request).
//   * Automatic re-scanning on any schedule. Nothing in this module runs on
//     a timer.
//   * Ranked or fuzzy text search. Matching uses exact URL, exact
//     normalised name, and token overlap. No FTS5 table exists.
//
// QUERY STYLE: runtime sqlx::query() only — no query!() macros.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::SqliteConnection;

use crate::persistence::entity_store::{get_entity_conn, Entity};
use crate::persistence::personal_store::{open_personal_db, PersonalStoreError};

// ---------------------------------------------------------------------------
// Focus declarations (decisions.id=502)
// ---------------------------------------------------------------------------

/// Which comparison produced a candidate. Ordered strongest first — the
/// scan records the strongest basis that fired for a pair, not every basis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchStrategy {
    /// Same source_url. decisions.id=502's highest-confidence signal:
    /// the same page imported twice.
    SourceUrl,
    /// Same normalised display_name.
    Name,
    /// Similar name plus overlapping content-field values.
    NameAndFieldOverlap,
}

impl MatchStrategy {
    /// The `match_basis` value persisted in dedup_candidates. Must stay in
    /// step with that column's CHECK constraint in personal_002.sql.
    fn as_db_str(self) -> &'static str {
        match self {
            MatchStrategy::SourceUrl => "url_match",
            MatchStrategy::Name => "name_match",
            MatchStrategy::NameAndFieldOverlap => "name_and_field_overlap",
        }
    }
}

/// How a user-generated field on the losing record is carried onto the
/// winner when the user resolves a pair.
///
/// decisions.id=502's invariant: user-generated data always survives.
/// Resolving a duplicate must never be the reason a note or a rating
/// disappears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CombineRule {
    /// Join both values, attributing the incoming one. For notes.
    ConcatenateWithAttribution,
    /// Union of comma-separated values, order-stable, de-duplicated.
    /// For tags.
    Union,
    /// Numeric sum. For cook counts and other tallies.
    Sum,
    /// Numeric maximum. For ratings and last-used dates held as numbers.
    Max,
    /// Keep the winner's value and drop the loser's. Only appropriate where
    /// the field is genuinely singular and the winner's is authoritative.
    KeepWinner,
}

/// One user-generated field and how to combine it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserGeneratedField {
    pub field_name: String,
    pub combine: CombineRule,
}

/// The six things decisions.id=502 requires a Focus to declare, as one
/// struct. A Focus that has not declared these cannot use this framework —
/// which is the intended gate, not an oversight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusDedupDeclaration {
    pub focus_slug: String,

    /// The record's substance — ingredients, steps, body text. NEVER merged
    /// across records: decisions.id=502 forbids constructing a hybrid record
    /// that matches neither original. These are compared, shown
    /// side-by-side, and otherwise left alone.
    pub content_fields: Vec<String>,

    /// Fields always surfaced in a comparison even when the overall match
    /// looks strong — the ones where a difference means "different thing",
    /// not "same thing described differently".
    pub key_difference_fields: Vec<String>,

    /// Fields the user authored, and how to preserve them on resolution.
    pub user_generated_fields: Vec<UserGeneratedField>,

    /// Which comparisons to run. An empty list disables detection entirely,
    /// which is a legitimate choice for a Focus whose records have no
    /// meaningful notion of duplication.
    pub strategies: Vec<MatchStrategy>,

    /// Token-overlap score in 0.0..=1.0 at or above which
    /// NameAndFieldOverlap fires. Unused by the other strategies.
    pub overlap_threshold: f64,

    /// The Focus's own wording for the question put to the user. Surfaced by
    /// the caller; this module never renders it.
    pub duplicate_question: Option<String>,
}

impl FocusDedupDeclaration {
    fn validate(&self) -> Result<(), PersonalStoreError> {
        if self.focus_slug.trim().is_empty() {
            return Err(PersonalStoreError::Validation(
                "FocusDedupDeclaration.focus_slug cannot be blank.".to_owned(),
            ));
        }
        if !(0.0..=1.0).contains(&self.overlap_threshold) {
            return Err(PersonalStoreError::Validation(format!(
                "overlap_threshold must be between 0.0 and 1.0, got {}.",
                self.overlap_threshold
            )));
        }
        if self
            .strategies
            .contains(&MatchStrategy::NameAndFieldOverlap)
            && self.content_fields.is_empty()
        {
            return Err(PersonalStoreError::Validation(
                "NameAndFieldOverlap needs at least one content field to \
                 compare — declare content_fields or drop the strategy."
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Records and comparison results
// ---------------------------------------------------------------------------

/// An entity plus its current facts, which is what a comparison actually
/// needs. Facts live in entity_facts (personal_store's table); only active
/// ones — valid_until IS NULL — participate, so a superseded value never
/// makes two records look alike.
#[derive(Debug, Clone, PartialEq)]
pub struct DedupRecord {
    pub entity: Entity,
    pub fields: BTreeMap<String, String>,
}

impl DedupRecord {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// A surfaced pair awaiting the user's judgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DedupCandidate {
    pub id: String,
    pub focus_slug: String,
    pub record_id_a: String,
    pub record_id_b: String,
    pub match_confidence: f64,
    pub match_basis: String,
    /// Field names that differ between the two records, plus every declared
    /// key-difference field. This — not match_confidence — is what the user
    /// reviews.
    pub differing_fields: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// What the user decided about a pair. There is no "merge" variant, by
/// design: decisions.id=502 forbids constructing a hybrid record. One
/// record wins whole, or they are different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Keep record A. B is tombstoned; its user-generated fields are
    /// carried onto A first.
    KeepA,
    /// Keep record B. A is tombstoned; its user-generated fields are
    /// carried onto B first.
    KeepB,
    /// They are genuinely different. Both survive untouched, and the pair
    /// is never surfaced again.
    ConfirmedDistinct,
}

impl Resolution {
    fn as_db_str(self) -> &'static str {
        match self {
            Resolution::KeepA => "resolved_keep_a",
            Resolution::KeepB => "resolved_keep_b",
            Resolution::ConfirmedDistinct => "user_confirmed_distinct",
        }
    }
}

// ---------------------------------------------------------------------------
// Pure comparison logic
// ---------------------------------------------------------------------------
//
// Everything in this section is a pure function of its inputs — no database,
// no clock, no IO. That is deliberate: the matching rules are the part of
// this block most likely to be argued with, and they should be arguable
// against a test rather than against a running application.

/// Lowercase, drop apostrophes, turn every other non-alphanumeric character
/// into a space, and collapse runs of whitespace. "Grandma's Bread!!" and
/// "grandmas bread" normalise alike.
///
/// Apostrophes are DELETED rather than replaced with a space, unlike other
/// punctuation. Replacing them splits a possessive into two tokens —
/// "grandma" and "s" — which both breaks exact-name matching against the
/// unpunctuated spelling and pollutes the token set with a meaningless "s"
/// that inflates overlap scores between unrelated records.
///
/// ASCII-oriented in its punctuation handling, but to_lowercase() does full
/// Unicode case folding and every alphanumeric character is kept, so
/// accented and non-Latin text survives normalisation rather than being
/// mangled.
fn normalise(text: &str) -> String {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalised whitespace-separated tokens.
fn token_set(text: &str) -> BTreeSet<String> {
    normalise(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Jaccard similarity: shared tokens over total distinct tokens. Two empty
/// inputs score 0.0, not 1.0 — "both records said nothing" is not evidence
/// that they are the same thing, and treating it as a perfect match would
/// make every record with an empty field a duplicate of every other.
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Mean token overlap across the declared content fields. A field absent
/// from both records is skipped rather than scored zero, so a Focus can
/// declare optional fields without every record being penalised for not
/// having them.
fn content_overlap(decl: &FocusDedupDeclaration, a: &DedupRecord, b: &DedupRecord) -> f64 {
    let mut scores = Vec::new();
    for field in &decl.content_fields {
        let (va, vb) = (a.field(field), b.field(field));
        if va.is_none() && vb.is_none() {
            continue;
        }
        scores.push(jaccard(
            &token_set(va.unwrap_or("")),
            &token_set(vb.unwrap_or("")),
        ));
    }
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    }
}

/// The strongest match basis that fires for a pair, with its confidence, or
/// None if the pair is not a candidate at all.
///
/// Strategies are tried strongest-first and the first hit wins — a pair that
/// shares a source_url is recorded as a URL match, not additionally as a
/// name match. Confidence is advisory ordering only.
fn evaluate_pair(
    decl: &FocusDedupDeclaration,
    a: &DedupRecord,
    b: &DedupRecord,
) -> Option<(MatchStrategy, f64)> {
    if decl.strategies.contains(&MatchStrategy::SourceUrl) {
        if let (Some(ua), Some(ub)) = (
            a.entity.source_url.as_deref(),
            b.entity.source_url.as_deref(),
        ) {
            if !ua.trim().is_empty() && ua.trim() == ub.trim() {
                return Some((MatchStrategy::SourceUrl, 1.0));
            }
        }
    }

    let name_a = normalise(&a.entity.display_name);
    let name_b = normalise(&b.entity.display_name);

    if decl.strategies.contains(&MatchStrategy::Name) && !name_a.is_empty() && name_a == name_b {
        return Some((MatchStrategy::Name, 0.9));
    }

    if decl
        .strategies
        .contains(&MatchStrategy::NameAndFieldOverlap)
    {
        let name_similarity = jaccard(&token_set(&name_a), &token_set(&name_b));
        if name_similarity > 0.0 {
            let overlap = content_overlap(decl, a, b);
            // Both halves must clear the bar: a shared word in the title is
            // not a duplicate signal on its own, and neither is generic
            // ingredient overlap between two unrelated records.
            if name_similarity >= decl.overlap_threshold && overlap >= decl.overlap_threshold {
                return Some((
                    MatchStrategy::NameAndFieldOverlap,
                    (name_similarity + overlap) / 2.0,
                ));
            }
        }
    }

    None
}

/// Fields the user should look at: every declared content field whose values
/// differ, plus every declared key-difference field regardless of whether it
/// differs.
///
/// Key-difference fields are included unconditionally because their whole
/// purpose is to be checked. decisions.id=502's example is leavening: the
/// user needs to see it to judge, even when both records happen to agree.
fn differing_fields(decl: &FocusDedupDeclaration, a: &DedupRecord, b: &DedupRecord) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    for field in &decl.content_fields {
        if normalise(a.field(field).unwrap_or("")) != normalise(b.field(field).unwrap_or("")) {
            out.push(field.clone());
        }
    }
    for field in &decl.key_difference_fields {
        if !out.contains(field) {
            out.push(field.clone());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Combining user-generated fields
// ---------------------------------------------------------------------------

/// Apply one CombineRule. Pure, and the single place decisions.id=502's
/// "user-generated data always survives" invariant is actually implemented.
///
/// Where a rule cannot be applied — a Sum over text that is not numeric —
/// the function falls back to keeping BOTH values rather than silently
/// dropping one. Losing a user's note because a Focus mis-declared a field
/// type is exactly the failure this invariant exists to prevent.
fn combine_values(rule: CombineRule, winner: Option<&str>, loser: Option<&str>) -> Option<String> {
    let (w, l) = match (winner, loser) {
        (None, None) => return None,
        (Some(w), None) => return Some(w.to_owned()),
        (None, Some(l)) => return Some(l.to_owned()),
        (Some(w), Some(l)) => (w, l),
    };

    if w.trim() == l.trim() {
        return Some(w.to_owned());
    }

    Some(match rule {
        CombineRule::KeepWinner => w.to_owned(),

        CombineRule::ConcatenateWithAttribution => {
            format!("{w}\n\n[from merged duplicate] {l}")
        }

        CombineRule::Union => {
            let mut seen: Vec<String> = Vec::new();
            for part in w.split(',').chain(l.split(',')) {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !seen.iter().any(|s| s.eq_ignore_ascii_case(trimmed)) {
                    seen.push(trimmed.to_owned());
                }
            }
            seen.join(", ")
        }

        CombineRule::Sum => match (w.trim().parse::<f64>(), l.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => format_number(a + b),
            _ => keep_both_fallback(w, l),
        },

        CombineRule::Max => match (w.trim().parse::<f64>(), l.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => format_number(a.max(b)),
            _ => keep_both_fallback(w, l),
        },
    })
}

/// Render a combined number without a trailing ".0" on whole values, so a
/// cook count reads "7" rather than "7.0".
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Last resort when a numeric rule meets non-numeric data: keep both values
/// visibly rather than discard either.
fn keep_both_fallback(w: &str, l: &str) -> String {
    log::warn!(
        "dedup: numeric combine rule applied to non-numeric values — \
         keeping both rather than dropping one"
    );
    format!("{w}\n\n[from merged duplicate] {l}")
}

// ---------------------------------------------------------------------------
// Loading records
// ---------------------------------------------------------------------------

/// Active facts for an entity, as field_name -> field_value.
///
/// entity_facts belongs to personal_store; this is a read of it, in the same
/// spirit as entity_store's cascade counts. Only valid_until IS NULL rows
/// participate, so superseded history never influences a match.
async fn load_fields_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<BTreeMap<String, String>, PersonalStoreError> {
    let rows = sqlx::query(
        "SELECT field_name, field_value FROM entity_facts
         WHERE entity_id = ? AND valid_until IS NULL",
    )
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut out = BTreeMap::new();
    for row in &rows {
        let name: String = row.try_get("field_name")?;
        let value: String = row.try_get("field_value")?;
        out.insert(name, value);
    }
    Ok(out)
}

/// Load one record with its facts. Ok(None) when the entity does not exist.
pub(crate) async fn load_record_conn(
    conn: &mut SqliteConnection,
    entity_id: &str,
) -> Result<Option<DedupRecord>, PersonalStoreError> {
    let entity = match get_entity_conn(conn, entity_id).await? {
        Some(e) => e,
        None => return Ok(None),
    };
    let fields = load_fields_conn(conn, entity_id).await?;
    Ok(Some(DedupRecord { entity, fields }))
}

// ---------------------------------------------------------------------------
// Candidate persistence helpers
// ---------------------------------------------------------------------------

/// Order a pair deterministically so that (A,B) and (B,A) are the same row.
/// personal_002.sql carries a UNIQUE index on
/// (focus_slug, record_id_a, record_id_b) which only holds if every writer
/// normalises first.
fn ordered_pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn row_to_candidate(row: &sqlx::sqlite::SqliteRow) -> Result<DedupCandidate, PersonalStoreError> {
    let id: String = row.try_get("id")?;
    let raw: Option<String> = row.try_get("differing_fields")?;
    let differing_fields: Vec<String> = match raw {
        Some(json) => serde_json::from_str(&json).map_err(|e| {
            PersonalStoreError::Validation(format!(
                "dedup_candidates.differing_fields for id '{id}' is not a JSON array: {e}"
            ))
        })?,
        None => Vec::new(),
    };

    Ok(DedupCandidate {
        id,
        focus_slug: row.try_get("focus_slug")?,
        record_id_a: row.try_get("record_id_a")?,
        record_id_b: row.try_get("record_id_b")?,
        match_confidence: row.try_get("match_confidence")?,
        match_basis: row.try_get("match_basis")?,
        differing_fields,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
        resolved_at: row.try_get("resolved_at")?,
    })
}

const CANDIDATE_COLUMNS: &str =
    "id, focus_slug, record_id_a, record_id_b, match_confidence, match_basis, \
     differing_fields, status, created_at, resolved_at";

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Compare a set of records pairwise and persist any candidates found.
/// Returns the ids of newly created candidate rows.
///
/// CALLER-TRIGGERED ONLY. decisions.id=502 defers background cross-source
/// dedup past R1, so nothing here runs on a schedule — the caller invokes it
/// after an import or when the user asks.
///
/// Already-judged pairs are left alone. A pair the user marked
/// ConfirmedDistinct is never resurfaced, and a pair already pending is not
/// duplicated. This is what makes the scan safe to re-run.
///
/// Records already tombstoned ('user_deleted') take no part: resolving a
/// duplicate must not cause the loser to reappear in a later scan.
pub async fn scan_for_duplicates(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    decl: &FocusDedupDeclaration,
    entity_ids: &[String],
) -> Result<Vec<String>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    scan_for_duplicates_conn(&mut conn, decl, entity_ids).await
}

pub(crate) async fn scan_for_duplicates_conn(
    conn: &mut SqliteConnection,
    decl: &FocusDedupDeclaration,
    entity_ids: &[String],
) -> Result<Vec<String>, PersonalStoreError> {
    decl.validate()?;

    if decl.strategies.is_empty() || entity_ids.len() < 2 {
        return Ok(Vec::new());
    }

    // Load once; pairwise comparison would otherwise re-read each record
    // O(n) times.
    let mut records: Vec<DedupRecord> = Vec::with_capacity(entity_ids.len());
    for id in entity_ids {
        if let Some(record) = load_record_conn(conn, id).await? {
            if record.entity.status != "user_deleted" {
                records.push(record);
            }
        }
    }

    let mut created = Vec::new();
    for i in 0..records.len() {
        for j in (i + 1)..records.len() {
            let (a, b) = (&records[i], &records[j]);
            let (strategy, confidence) = match evaluate_pair(decl, a, b) {
                Some(hit) => hit,
                None => continue,
            };

            let (id_a, id_b) = ordered_pair(&a.entity.id, &b.entity.id);

            let existing: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM dedup_candidates
                 WHERE focus_slug = ? AND record_id_a = ? AND record_id_b = ?",
            )
            .bind(&decl.focus_slug)
            .bind(id_a)
            .bind(id_b)
            .fetch_optional(&mut *conn)
            .await?;

            if existing.is_some() {
                continue;
            }

            let fields = differing_fields(decl, a, b);
            let fields_json = serde_json::to_string(&fields).map_err(|e| {
                PersonalStoreError::Validation(format!("differing_fields not serializable: {e}"))
            })?;

            let new_id = uuid::Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO dedup_candidates
                 (id, focus_slug, record_id_a, record_id_b, match_confidence,
                  match_basis, differing_fields, status, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
            )
            .bind(&new_id)
            .bind(&decl.focus_slug)
            .bind(id_a)
            .bind(id_b)
            .bind(confidence)
            .bind(strategy.as_db_str())
            .bind(&fields_json)
            .bind(crate::providers::utils::now())
            .execute(&mut *conn)
            .await?;

            created.push(new_id);
        }
    }

    Ok(created)
}

/// Pending candidates for a Focus, highest confidence first — the review
/// queue. Confidence orders this list and does nothing else.
pub async fn list_pending_candidates(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    focus_slug: &str,
) -> Result<Vec<DedupCandidate>, PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    list_pending_candidates_conn(&mut conn, focus_slug).await
}

pub(crate) async fn list_pending_candidates_conn(
    conn: &mut SqliteConnection,
    focus_slug: &str,
) -> Result<Vec<DedupCandidate>, PersonalStoreError> {
    let rows = sqlx::query(&format!(
        "SELECT {CANDIDATE_COLUMNS} FROM dedup_candidates
         WHERE focus_slug = ? AND status = 'pending'
         ORDER BY match_confidence DESC, created_at"
    ))
    .bind(focus_slug)
    .fetch_all(&mut *conn)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(row_to_candidate(row)?);
    }
    Ok(out)
}

/// Fetch one candidate. Ok(None) when it does not exist.
pub(crate) async fn get_candidate_conn(
    conn: &mut SqliteConnection,
    candidate_id: &str,
) -> Result<Option<DedupCandidate>, PersonalStoreError> {
    let row = sqlx::query(&format!(
        "SELECT {CANDIDATE_COLUMNS} FROM dedup_candidates WHERE id = ?"
    ))
    .bind(candidate_id)
    .fetch_optional(&mut *conn)
    .await?;

    match row {
        Some(r) => Ok(Some(row_to_candidate(&r)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Record the user's judgement on a candidate pair and carry it out.
///
/// This function is the only writer of a resolution, and it acts only on an
/// explicit Resolution supplied by a human decision. There is no threshold,
/// no confidence level, and no code path anywhere in this module that
/// reaches it on its own.
///
/// KeepA / KeepB:
///   1. Every declared user-generated field is combined onto the winner
///      first, per its CombineRule.
///   2. The winner inherits the loser's source_url if it had none, so the
///      surviving record keeps the pair's source references
///      (decisions.id=502).
///   3. The loser is tombstoned as 'user_deleted' — never hard-deleted, and
///      never re-imported even if it reappears in source.
/// Content fields are NEVER combined. The winner keeps its own substance.
///
/// ConfirmedDistinct: both records are left exactly as they are, and the
/// pair is never surfaced again.
///
/// The whole operation runs inside a SAVEPOINT: a resolution cannot leave
/// the loser tombstoned but the user's notes uncopied.
pub async fn resolve_candidate(
    user_id: &str,
    persona_id: &str,
    key_hex: &str,
    decl: &FocusDedupDeclaration,
    candidate_id: &str,
    resolution: Resolution,
) -> Result<(), PersonalStoreError> {
    let mut conn = open_personal_db(user_id, persona_id, key_hex).await?;
    resolve_candidate_conn(&mut conn, decl, candidate_id, resolution).await
}

pub(crate) async fn resolve_candidate_conn(
    conn: &mut SqliteConnection,
    decl: &FocusDedupDeclaration,
    candidate_id: &str,
    resolution: Resolution,
) -> Result<(), PersonalStoreError> {
    decl.validate()?;

    let candidate = get_candidate_conn(conn, candidate_id)
        .await?
        .ok_or_else(|| {
            PersonalStoreError::Validation(format!("No dedup candidate with id '{candidate_id}'."))
        })?;

    if candidate.status != "pending" {
        return Err(PersonalStoreError::Validation(format!(
            "Candidate '{candidate_id}' was already resolved as '{}' — \
             re-resolving would act on a judgement the user did not make.",
            candidate.status
        )));
    }

    sqlx::query("SAVEPOINT resolve_candidate")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), PersonalStoreError> = async {
        if resolution != Resolution::ConfirmedDistinct {
            let (winner_id, loser_id) = match resolution {
                Resolution::KeepA => (&candidate.record_id_a, &candidate.record_id_b),
                Resolution::KeepB => (&candidate.record_id_b, &candidate.record_id_a),
                Resolution::ConfirmedDistinct => unreachable!(),
            };

            let winner = load_record_conn(conn, winner_id).await?.ok_or_else(|| {
                PersonalStoreError::Validation(format!(
                    "Record '{winner_id}' no longer exists — cannot resolve."
                ))
            })?;
            let loser = load_record_conn(conn, loser_id).await?.ok_or_else(|| {
                PersonalStoreError::Validation(format!(
                    "Record '{loser_id}' no longer exists — cannot resolve."
                ))
            })?;

            carry_user_fields_conn(conn, decl, &winner, &loser).await?;

            if winner.entity.source_url.is_none() {
                if let Some(url) = loser.entity.source_url.as_deref() {
                    sqlx::query("UPDATE entities SET source_url = ? WHERE id = ?")
                        .bind(url)
                        .bind(winner_id)
                        .execute(&mut *conn)
                        .await?;
                }
            }

            sqlx::query("UPDATE entities SET status = 'user_deleted' WHERE id = ?")
                .bind(loser_id)
                .execute(&mut *conn)
                .await?;
        }

        sqlx::query("UPDATE dedup_candidates SET status = ?, resolved_at = ? WHERE id = ?")
            .bind(resolution.as_db_str())
            .bind(crate::providers::utils::now())
            .bind(candidate_id)
            .execute(&mut *conn)
            .await?;

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE resolve_candidate")
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Err(e) => {
            if let Err(rollback_err) = sqlx::query("ROLLBACK TO resolve_candidate")
                .execute(&mut *conn)
                .await
            {
                log::error!("Savepoint rollback failed in resolve_candidate: {rollback_err}");
            }
            let _ = sqlx::query("RELEASE resolve_candidate")
                .execute(&mut *conn)
                .await;
            Err(e)
        }
    }
}

/// Combine each declared user-generated field from loser onto winner.
///
/// Where the winner already holds the field, its active fact is updated in
/// place. Where it does not, the loser's fact is copied across, preserving
/// the original sensitivity and source_persona_id so Cross-Persona
/// provenance (decisions.id=546) is not laundered by a merge.
async fn carry_user_fields_conn(
    conn: &mut SqliteConnection,
    decl: &FocusDedupDeclaration,
    winner: &DedupRecord,
    loser: &DedupRecord,
) -> Result<(), PersonalStoreError> {
    for field in &decl.user_generated_fields {
        let name = field.field_name.as_str();
        let combined = combine_values(field.combine, winner.field(name), loser.field(name));

        let combined = match combined {
            Some(v) => v,
            None => continue,
        };

        if winner.fields.contains_key(name) {
            if winner.field(name) == Some(combined.as_str()) {
                continue;
            }
            sqlx::query(
                "UPDATE entity_facts SET field_value = ?
                 WHERE entity_id = ? AND field_name = ? AND valid_until IS NULL",
            )
            .bind(&combined)
            .bind(&winner.entity.id)
            .bind(name)
            .execute(&mut *conn)
            .await?;
        } else {
            // Copy the loser's row so provenance travels with the value.
            let source: Option<(String, String)> = sqlx::query_as(
                "SELECT sensitivity, source_persona_id FROM entity_facts
                 WHERE entity_id = ? AND field_name = ? AND valid_until IS NULL",
            )
            .bind(&loser.entity.id)
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;

            let (sensitivity, source_persona_id) = match source {
                Some(pair) => pair,
                None => continue,
            };

            sqlx::query(
                "INSERT INTO entity_facts
                 (id, entity_id, field_name, field_value, sensitivity,
                  created_at, source_persona_id)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&winner.entity.id)
            .bind(name)
            .bind(&combined)
            .bind(&sensitivity)
            .bind(crate::providers::utils::now())
            .bind(&source_persona_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use crate::persistence::source_registry_store::{import_record_conn, register_source_conn};
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;

    const V1: &str = include_str!("../../schema/personal_001.sql");
    const V2: &str = include_str!("../../schema/personal_002.sql");
    const V3: &str = include_str!("../../schema/personal_003.sql");

    async fn test_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .expect("in-memory connection failed");
        for schema in [V1, V2, V3] {
            for stmt in parse_statements(schema) {
                sqlx::query(&stmt)
                    .execute(&mut conn)
                    .await
                    .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{stmt}"));
            }
        }
        conn
    }

    /// A cooking-shaped declaration, built here in the test rather than in
    /// the module — the framework itself knows nothing about recipes.
    fn cooking_decl() -> FocusDedupDeclaration {
        FocusDedupDeclaration {
            focus_slug: "cooking".to_owned(),
            content_fields: vec!["ingredients".to_owned(), "steps".to_owned()],
            key_difference_fields: vec!["leavening".to_owned()],
            user_generated_fields: vec![
                UserGeneratedField {
                    field_name: "notes".to_owned(),
                    combine: CombineRule::ConcatenateWithAttribution,
                },
                UserGeneratedField {
                    field_name: "tags".to_owned(),
                    combine: CombineRule::Union,
                },
                UserGeneratedField {
                    field_name: "times_cooked".to_owned(),
                    combine: CombineRule::Sum,
                },
                UserGeneratedField {
                    field_name: "rating".to_owned(),
                    combine: CombineRule::Max,
                },
            ],
            strategies: vec![
                MatchStrategy::SourceUrl,
                MatchStrategy::Name,
                MatchStrategy::NameAndFieldOverlap,
            ],
            overlap_threshold: 0.6,
            duplicate_question: Some("Is this the same recipe?".to_owned()),
        }
    }

    async fn a_source(conn: &mut SqliteConnection) -> String {
        register_source_conn(conn, "persona-1", "cooking", "paprika_import", None, None)
            .await
            .unwrap()
    }

    async fn add_record(
        conn: &mut SqliteConnection,
        source: &str,
        name: &str,
        url: Option<&str>,
        fields: &[(&str, &str)],
    ) -> String {
        let id = import_record_conn(conn, source, "recipe", name, &[], url, None)
            .await
            .unwrap();
        for (field_name, value) in fields {
            sqlx::query(
                "INSERT INTO entity_facts
                 (id, entity_id, field_name, field_value, sensitivity,
                  created_at, source_persona_id)
                 VALUES (?, ?, ?, ?, 'general', '2026-07-25T00:00:00Z', 'persona-1')",
            )
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&id)
            .bind(field_name)
            .bind(value)
            .execute(&mut *conn)
            .await
            .unwrap();
        }
        id
    }

    // -- pure comparison logic ----------------------------------------------

    #[test]
    fn normalise_strips_punctuation_and_case() {
        assert_eq!(normalise("Grandma's Bread!!"), "grandmas bread");
        assert_eq!(normalise("  Multiple   Spaces  "), "multiple spaces");
        assert_eq!(normalise("Crème Brûlée"), "crème brûlée");
    }

    #[test]
    fn jaccard_scores_two_empty_sets_as_zero_not_one() {
        let empty = BTreeSet::new();
        assert_eq!(
            jaccard(&empty, &empty),
            0.0,
            "two records saying nothing is not evidence they are the same"
        );
    }

    #[test]
    fn jaccard_measures_shared_tokens() {
        let a = token_set("flour water salt");
        let b = token_set("flour water yeast");
        // 2 shared, 4 distinct.
        assert!((jaccard(&a, &b) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn declaration_rejects_impossible_configurations() {
        let mut decl = cooking_decl();
        decl.overlap_threshold = 1.5;
        assert!(decl.validate().is_err(), "threshold must be a ratio");

        let mut decl = cooking_decl();
        decl.content_fields.clear();
        assert!(
            decl.validate().is_err(),
            "overlap matching with nothing to overlap is a misconfiguration"
        );

        let mut decl = cooking_decl();
        decl.focus_slug = "  ".to_owned();
        assert!(decl.validate().is_err());
    }

    // -- combine rules ------------------------------------------------------

    #[test]
    fn combine_preserves_both_sides_of_a_note() {
        let out = combine_values(
            CombineRule::ConcatenateWithAttribution,
            Some("Winner note"),
            Some("Loser note"),
        )
        .unwrap();
        assert!(out.contains("Winner note"));
        assert!(
            out.contains("Loser note"),
            "the user's other note must survive"
        );
    }

    #[test]
    fn combine_unions_tags_without_duplicates() {
        let out = combine_values(
            CombineRule::Union,
            Some("bread, easy"),
            Some("Easy, weeknight"),
        )
        .unwrap();
        assert_eq!(out, "bread, easy, weeknight");
    }

    #[test]
    fn combine_sums_and_maxes_numbers_cleanly() {
        assert_eq!(
            combine_values(CombineRule::Sum, Some("3"), Some("4")).unwrap(),
            "7",
            "a tally should not read as 7.0"
        );
        assert_eq!(
            combine_values(CombineRule::Max, Some("3"), Some("5")).unwrap(),
            "5"
        );
    }

    #[test]
    fn numeric_rules_on_text_keep_both_rather_than_drop_one() {
        let out = combine_values(CombineRule::Sum, Some("often"), Some("twice")).unwrap();
        assert!(out.contains("often") && out.contains("twice"));
    }

    #[test]
    fn combine_handles_one_sided_and_identical_values() {
        assert_eq!(
            combine_values(CombineRule::Union, None, Some("only-loser")).unwrap(),
            "only-loser",
            "a value the winner lacks must still survive"
        );
        assert_eq!(
            combine_values(CombineRule::Sum, Some("same"), Some("same")).unwrap(),
            "same"
        );
        assert!(combine_values(CombineRule::Max, None, None).is_none());
    }

    #[test]
    fn keep_winner_is_the_only_rule_that_drops_a_value() {
        assert_eq!(
            combine_values(CombineRule::KeepWinner, Some("mine"), Some("theirs")).unwrap(),
            "mine"
        );
    }

    // -- detection ----------------------------------------------------------

    #[tokio::test]
    async fn same_source_url_is_the_strongest_basis() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Sourdough", Some("https://x.test/1"), &[]).await;
        let b = add_record(
            &mut conn,
            &s,
            "Totally Different Name",
            Some("https://x.test/1"),
            &[],
        )
        .await;

        let created = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a, b])
            .await
            .unwrap();
        assert_eq!(created.len(), 1);

        let pending = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap();
        assert_eq!(pending[0].match_basis, "url_match");
        assert!((pending[0].match_confidence - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn identical_names_match_despite_punctuation_and_case() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Grandma's Bread!", None, &[]).await;
        let b = add_record(&mut conn, &s, "grandmas bread", None, &[]).await;

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a, b])
            .await
            .unwrap();
        let pending = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].match_basis, "name_match");
    }

    #[tokio::test]
    async fn unrelated_records_are_not_surfaced() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(
            &mut conn,
            &s,
            "Sourdough Bread",
            None,
            &[("ingredients", "flour water salt starter")],
        )
        .await;
        let b = add_record(
            &mut conn,
            &s,
            "Chocolate Cake",
            None,
            &[("ingredients", "cocoa sugar butter eggs")],
        )
        .await;

        let created = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a, b])
            .await
            .unwrap();
        assert!(created.is_empty(), "a scan must not invent duplicates");
    }

    #[tokio::test]
    async fn near_identical_records_surface_with_their_key_difference() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(
            &mut conn,
            &s,
            "Simple Bread Loaf",
            None,
            &[
                ("ingredients", "flour water salt yeast"),
                ("steps", "mix knead rise bake"),
                ("leavening", "yeast"),
            ],
        )
        .await;
        let b = add_record(
            &mut conn,
            &s,
            "Simple Bread Loaf Recipe",
            None,
            &[
                ("ingredients", "flour water salt yeast"),
                ("steps", "mix knead rise bake"),
                ("leavening", "none"),
            ],
        )
        .await;

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a, b])
            .await
            .unwrap();
        let pending = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0]
                .differing_fields
                .contains(&"leavening".to_owned()),
            "a key-difference field must always be put in front of the user"
        );
    }

    #[tokio::test]
    async fn rescanning_does_not_duplicate_or_resurrect_judgements() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;
        let b = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;
        let ids = vec![a, b];

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &ids)
            .await
            .unwrap();
        let again = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &ids)
            .await
            .unwrap();
        assert!(again.is_empty(), "a pending pair must not be re-created");

        let candidate = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap()[0]
            .id
            .clone();
        resolve_candidate_conn(
            &mut conn,
            &cooking_decl(),
            &candidate,
            Resolution::ConfirmedDistinct,
        )
        .await
        .unwrap();

        let after = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &ids)
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "a pair the user called distinct must never be surfaced again"
        );
    }

    #[tokio::test]
    async fn scanning_is_inert_without_strategies_or_records() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;
        let b = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;

        let mut decl = cooking_decl();
        decl.strategies.clear();
        assert!(scan_for_duplicates_conn(&mut conn, &decl, &[a.clone(), b])
            .await
            .unwrap()
            .is_empty());

        assert!(scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a])
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn pair_order_does_not_create_two_candidates() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;
        let b = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let reversed = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[b, a])
            .await
            .unwrap();
        assert!(reversed.is_empty(), "(A,B) and (B,A) are the same pair");
    }

    // -- resolution ---------------------------------------------------------

    /// Build a resolved-ready pair: two duplicates, each with user-generated
    /// data the other lacks. Returns (candidate_id, record_a, record_b).
    async fn a_pair_with_user_data(conn: &mut SqliteConnection) -> (String, String, String) {
        let s = a_source(conn).await;
        let a = add_record(
            conn,
            &s,
            "Bread",
            Some("https://x.test/1"),
            &[
                ("ingredients", "flour water salt"),
                ("notes", "Winner note"),
                ("tags", "bread, easy"),
                ("times_cooked", "3"),
                ("rating", "4"),
            ],
        )
        .await;
        let b = add_record(
            conn,
            &s,
            "Bread",
            Some("https://x.test/1"),
            &[
                ("ingredients", "flour water salt"),
                ("notes", "Loser note"),
                ("tags", "weeknight"),
                ("times_cooked", "4"),
                ("rating", "5"),
            ],
        )
        .await;

        scan_for_duplicates_conn(conn, &cooking_decl(), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let candidate = list_pending_candidates_conn(conn, "cooking").await.unwrap()[0]
            .id
            .clone();
        (candidate, a, b)
    }

    #[tokio::test]
    async fn resolution_carries_every_user_generated_field_onto_the_winner() {
        let mut conn = test_db().await;
        let (candidate, a, b) = a_pair_with_user_data(&mut conn).await;
        let winner_is_a = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap()[0]
            .record_id_a
            == a;
        let resolution = if winner_is_a {
            Resolution::KeepA
        } else {
            Resolution::KeepB
        };

        resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate, resolution)
            .await
            .unwrap();

        let winner = load_record_conn(&mut conn, &a).await.unwrap().unwrap();
        let notes = winner.field("notes").unwrap();
        assert!(
            notes.contains("Winner note") && notes.contains("Loser note"),
            "both notes must survive: {notes}"
        );
        assert_eq!(winner.field("tags").unwrap(), "bread, easy, weeknight");
        assert_eq!(winner.field("times_cooked").unwrap(), "7");
        assert_eq!(winner.field("rating").unwrap(), "5");

        // The loser is tombstoned, not deleted.
        let loser = get_entity_conn(&mut conn, &b).await.unwrap().unwrap();
        assert_eq!(loser.status, "user_deleted");
    }

    #[tokio::test]
    async fn resolution_never_merges_content_fields() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(
            &mut conn,
            &s,
            "Bread",
            Some("https://x.test/1"),
            &[("ingredients", "flour water salt")],
        )
        .await;
        let b = add_record(
            &mut conn,
            &s,
            "Bread",
            Some("https://x.test/1"),
            &[("ingredients", "cocoa sugar butter")],
        )
        .await;

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let candidate = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap()[0]
            .clone();
        let winner_id = candidate.record_id_a.clone();

        resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate.id, Resolution::KeepA)
            .await
            .unwrap();

        let winner = load_record_conn(&mut conn, &winner_id)
            .await
            .unwrap()
            .unwrap();
        let ingredients = winner.field("ingredients").unwrap();
        assert!(
            ingredients == "flour water salt" || ingredients == "cocoa sugar butter",
            "the winner keeps its own substance — no hybrid record: {ingredients}"
        );
    }

    #[tokio::test]
    async fn confirmed_distinct_leaves_both_records_untouched() {
        let mut conn = test_db().await;
        let (candidate, a, b) = a_pair_with_user_data(&mut conn).await;

        resolve_candidate_conn(
            &mut conn,
            &cooking_decl(),
            &candidate,
            Resolution::ConfirmedDistinct,
        )
        .await
        .unwrap();

        for id in [&a, &b] {
            let e = get_entity_conn(&mut conn, id).await.unwrap().unwrap();
            assert_eq!(e.status, "active", "neither record may be tombstoned");
        }
        let record_a = load_record_conn(&mut conn, &a).await.unwrap().unwrap();
        assert_eq!(
            record_a.field("notes").unwrap(),
            "Winner note",
            "notes must not be combined when the records are distinct"
        );
    }

    #[tokio::test]
    async fn a_candidate_cannot_be_resolved_twice() {
        let mut conn = test_db().await;
        let (candidate, _, _) = a_pair_with_user_data(&mut conn).await;

        resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate, Resolution::KeepA)
            .await
            .unwrap();
        assert!(
            resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate, Resolution::KeepB)
                .await
                .is_err(),
            "re-resolving would act on a judgement the user did not make"
        );
    }

    #[tokio::test]
    async fn resolving_an_unknown_candidate_errors() {
        let mut conn = test_db().await;
        assert!(
            resolve_candidate_conn(&mut conn, &cooking_decl(), "nope", Resolution::KeepA)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn winner_inherits_the_losers_source_url_when_it_has_none() {
        let mut conn = test_db().await;
        let s = a_source(&mut conn).await;
        let a = add_record(&mut conn, &s, "Bread", None, &[]).await;
        let b = add_record(&mut conn, &s, "Bread", Some("https://x.test/9"), &[]).await;

        scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a.clone(), b.clone()])
            .await
            .unwrap();
        let candidate = list_pending_candidates_conn(&mut conn, "cooking")
            .await
            .unwrap()[0]
            .clone();

        // Keep whichever record has no source_url.
        let resolution = if candidate.record_id_a == a {
            Resolution::KeepA
        } else {
            Resolution::KeepB
        };
        resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate.id, resolution)
            .await
            .unwrap();

        let winner = get_entity_conn(&mut conn, &a).await.unwrap().unwrap();
        assert_eq!(
            winner.source_url,
            Some("https://x.test/9".to_owned()),
            "the surviving record keeps the pair's source reference"
        );
    }

    #[tokio::test]
    async fn a_tombstoned_record_is_excluded_from_later_scans() {
        let mut conn = test_db().await;
        let (candidate, a, b) = a_pair_with_user_data(&mut conn).await;
        resolve_candidate_conn(&mut conn, &cooking_decl(), &candidate, Resolution::KeepA)
            .await
            .unwrap();

        // A third identical record arrives. It must pair with the survivor
        // only, never with the tombstone.
        let s = a_source(&mut conn).await;
        let c = add_record(&mut conn, &s, "Bread", Some("https://x.test/1"), &[]).await;

        let created = scan_for_duplicates_conn(&mut conn, &cooking_decl(), &[a, b, c])
            .await
            .unwrap();
        assert_eq!(
            created.len(),
            1,
            "the tombstoned record must not be resurfaced as a duplicate"
        );
    }
}
