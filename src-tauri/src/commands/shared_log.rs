// src-tauri/src/commands/shared_log.rs
//
// items.id=957 units 2 and 3 (decisions.id=967, 434, 944): the IPC surface of the
// "What QR shared" view. Read only, except clear_consent_history.
//
//   get_shared_log_summary(persona_id, window, utc_offset_minutes, focus_id,
//                          provider_id)        one row per (day, Focus) group
//   get_shared_log_group_items(persona_id, day, focus_id, utc_offset_minutes,
//                          provider_id)        the items of ONE group, opened
//   clear_consent_history(persona_id, confirm) the user-initiated purge of consent
//                          records only (unit 3): the one WRITE here
//
// The view loads from the summary only: one row per (day, Focus) with counts, never
// one row per log row. Items load when a group is opened.
//
// WHAT CROSSES IPC: counts, dates, Focus ids and display names, provider ids and
// display names and mode, sentence KEYS, field LABELS, and the fixed
// content_summary of a consent record. NEVER a raw event string (a sentence key
// stands in), a raw field name (a label stands in: commands/shared_log_deck.rs),
// or any field value (shared_log_read.rs never selects one).
//
// Every command takes writer_gate::command first, derives the user and key from the
// unlocked session (no user_id argument exists to spoof), checks the Persona id is a
// UUID the user belongs to, and does its work in a *_for_session body.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::commands::persona::session_user_and_key;
use crate::commands::shared_log_deck::{
    self as deck, action_outcome, classify_consent, classify_disclosure, ActionOutcome, Classified,
    Tag, HISTORY_CLEARED, SEND_EVENTS,
};
use crate::persistence::consent_history_purge as deletion;
use crate::persistence::output_store::API_SEND_REVIEW_FOCUS_ID;
use crate::persistence::provider_store::ProviderMode;
use crate::persistence::shared_log_read::{
    self as read, ConsentRowView, ProviderInfo, SharedLogRaw,
};

/// The Focus whose display name is "Quick chat" (CloudChatAccessPane's hardcoded id).
const QUICK_ASK: &str = "quick-ask";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SharedLogWindow {
    Last30,
    Last7,
    Everything,
}

impl SharedLogWindow {
    fn days(self) -> Option<i64> {
        match self {
            SharedLogWindow::Last30 => Some(30),
            SharedLogWindow::Last7 => Some(7),
            SharedLogWindow::Everything => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SharedLogMode {
    Web,
    Api,
    /// A local provider or a missing provider row.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogProvider {
    pub id: String,
    /// None when the provider row is gone: the view says "an outside service".
    pub name: Option<String>,
    pub mode: SharedLogMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogFocus {
    pub id: String,
    /// None when the Focus cannot be resolved: the view says "A Focus".
    pub name: Option<String>,
    /// True for quick-ask, shown as "Quick chat".
    pub quick_chat: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogGroup {
    pub day: String,
    pub focus: SharedLogFocus,
    pub runs: u32,
    pub sends: u32,
    /// True when the group has sends and every one went through review.
    pub all_reviewed: bool,
    pub providers: Vec<SharedLogProvider>,
    pub kept_back: u32,
    pub choices: u32,
    /// Background checks, shown only under "Show background checks".
    pub background: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogMonth {
    /// "2026-10".
    pub month: String,
    pub runs: u32,
    pub sends: u32,
    pub kept_back: u32,
    pub groups: Vec<SharedLogGroup>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogSummary {
    /// Newest day first.
    pub groups: Vec<SharedLogGroup>,
    /// Older than the window, newest month first. Empty for "Everything".
    pub months: Vec<SharedLogMonth>,
    /// Filter options, from the unfiltered data.
    pub focuses: Vec<SharedLogFocus>,
    pub providers: Vec<SharedLogProvider>,
    /// False only when nothing at all is recorded (the empty state).
    pub has_any: bool,
    /// Consent records a purge would delete: the older ones. Records showing what is in
    /// force, and the cleared-on markers, are never counted.
    pub consent_record_count: u32,
    /// created_at of the newest "history cleared" record: the permanent line.
    pub cleared_on: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SharedLogTag {
    Sent,
    KeptBack,
    Choice,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SharedLogLabelKind {
    Catalogued,
    Draft,
    Message,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogLabel {
    pub kind: SharedLogLabelKind,
    /// The catalogue label for `catalogued`, else None.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct SharedLogItem {
    /// RFC 3339.
    pub time: String,
    pub tag: SharedLogTag,
    /// A key under navShell.shared.sentence.*, never an event string.
    pub sentence: String,
    pub provider: Option<SharedLogProvider>,
    pub shared: Vec<SharedLogLabel>,
    pub kept_back: Vec<SharedLogLabel>,
    /// The display name of a document type (document_ceiling_raised only).
    pub document_type: Option<String>,
    /// A consent record's fixed description, shown under Details only.
    pub summary: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure builders
// ---------------------------------------------------------------------------

/// Everything a build needs besides the raw rows.
pub(crate) struct Lookups {
    pub focus_names: HashMap<String, String>,
    pub providers: HashMap<String, ProviderInfo>,
}

impl Lookups {
    fn focus(&self, id: &str) -> SharedLogFocus {
        SharedLogFocus {
            id: id.to_owned(),
            name: self.focus_names.get(id).cloned(),
            quick_chat: id == QUICK_ASK,
        }
    }

    fn provider(&self, id: &str) -> SharedLogProvider {
        match self.providers.get(id) {
            Some(p) => SharedLogProvider {
                id: id.to_owned(),
                name: Some(p.display_name.clone()),
                mode: match p.mode {
                    ProviderMode::EmbeddedWeb => SharedLogMode::Web,
                    ProviderMode::Api => SharedLogMode::Api,
                    ProviderMode::Local => SharedLogMode::Unknown,
                },
            },
            None => SharedLogProvider {
                id: id.to_owned(),
                name: None,
                mode: SharedLogMode::Unknown,
            },
        }
    }
}

#[derive(Default)]
struct Acc {
    runs: HashSet<String>,
    sends: u32,
    reviewed: u32,
    providers: BTreeSet<String>,
    kept_back: u32,
    choices: u32,
    background: u32,
}

impl Acc {
    fn visible(&self) -> bool {
        self.sends > 0 || self.kept_back > 0 || self.choices > 0
    }
}

pub(crate) struct Filters<'a> {
    pub focus_id: Option<&'a str>,
    pub provider_id: Option<&'a str>,
}

fn clamp_u32(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

/// The Focus a run's rows are shown under: the run's Focus, or "" when focus_runs has
/// no row. The synthetic run that reviews one API Chat message (items.id=716) is shown
/// under the api-chat Focus, so its background and held rows land in that group.
fn focus_of<'a>(raw: &'a SharedLogRaw, run_id: &str) -> &'a str {
    match raw.run_focus.get(run_id).map(String::as_str) {
        Some(API_SEND_REVIEW_FOCUS_ID) => crate::api_chat::API_CHAT_FOCUS_ID,
        Some(f) => f,
        None => "",
    }
}

/// True for a run that only reviewed an API Chat message. Its rows are the review's
/// audit rows: the send itself is the api-chat run's own gate1_pass, so nothing under
/// this run ever counts as a send.
fn is_review_run(raw: &SharedLogRaw, run_id: &str) -> bool {
    raw.run_focus.get(run_id).map(String::as_str) == Some(API_SEND_REVIEW_FOCUS_ID)
}

/// The (day, Focus) accumulators of the raw rows under the filters.
fn accumulate(raw: &SharedLogRaw, f: &Filters<'_>) -> BTreeMap<(String, String), Acc> {
    let mut map: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for a in &raw.aggregates {
        let focus = focus_of(raw, &a.focus_run_id);
        if f.focus_id.is_some_and(|want| want != focus) {
            continue;
        }
        // A row that makes a send is counted through its action below, never here.
        if SEND_EVENTS.contains(&a.event_type.as_str()) {
            continue;
        }
        let Classified::Item { tag, key: _ } =
            classify_disclosure(&a.event_type, a.override_declined)
        else {
            continue;
        };
        // A provider filter keeps only sends to that provider; no event row is one.
        if f.provider_id.is_some() {
            continue;
        }
        let acc = map.entry((a.day.clone(), focus.to_owned())).or_default();
        if !a.focus_run_id.is_empty() {
            acc.runs.insert(a.focus_run_id.clone());
        }
        let n = clamp_u32(a.rows);
        match tag {
            Tag::KeptBack => acc.kept_back += n,
            Tag::Hidden => acc.background += n,
            // A reapplied choice is shown as an item and is not a send.
            Tag::Sent | Tag::Choice => {}
        }
    }
    // One send per (run, action): the counting rule is deck::action_outcome.
    for a in &raw.actions {
        if is_review_run(raw, &a.focus_run_id) {
            continue;
        }
        let focus = focus_of(raw, &a.focus_run_id);
        if f.focus_id.is_some_and(|want| want != focus) {
            continue;
        }
        let outcome = action_outcome(a.gate1, a.approved, a.marked, a.held);
        if outcome == ActionOutcome::NotSent {
            continue;
        }
        if let Some(want) = f.provider_id {
            if a.provider.as_deref() != Some(want) {
                continue;
            }
        }
        let acc = map.entry((a.day.clone(), focus.to_owned())).or_default();
        if !a.focus_run_id.is_empty() {
            acc.runs.insert(a.focus_run_id.clone());
        }
        acc.sends += 1;
        if outcome == ActionOutcome::Reviewed {
            acc.reviewed += 1;
        }
        acc.kept_back += clamp_u32(a.withheld);
        if let Some(p) = &a.provider {
            acc.providers.insert(p.clone());
        }
    }
    if f.provider_id.is_none() {
        for c in &raw.consent {
            if f.focus_id.is_some_and(|want| want != c.focus_slug) {
                continue;
            }
            match classify_consent(&c.event_type, c.has_reference) {
                Some((Tag::Choice, _)) => {
                    map.entry((c.day.clone(), c.focus_slug.clone()))
                        .or_default()
                        .choices += 1
                }
                Some((Tag::Hidden, _)) => {
                    map.entry((c.day.clone(), c.focus_slug.clone()))
                        .or_default()
                        .background += 1
                }
                _ => {}
            }
        }
    }
    map
}

fn group_of(day: &str, focus: &str, acc: &Acc, lk: &Lookups) -> SharedLogGroup {
    SharedLogGroup {
        day: day.to_owned(),
        focus: lk.focus(focus),
        runs: clamp_u32(acc.runs.len() as i64),
        sends: acc.sends,
        all_reviewed: acc.sends > 0 && acc.reviewed == acc.sends,
        providers: acc.providers.iter().map(|p| lk.provider(p)).collect(),
        kept_back: acc.kept_back,
        choices: acc.choices,
        background: acc.background,
    }
}

/// The summary: groups newest first, older days rolled into months, and the filter
/// options taken from the unfiltered rows.
pub(crate) fn build_summary(
    raw: &SharedLogRaw,
    lk: &Lookups,
    window: SharedLogWindow,
    today: NaiveDate,
    f: &Filters<'_>,
) -> SharedLogSummary {
    let cutoff = window.days().map(|d| {
        (today - Duration::days(d - 1))
            .format("%Y-%m-%d")
            .to_string()
    });

    let mut groups = Vec::new();
    let mut month_groups: BTreeMap<String, Vec<(SharedLogGroup, HashSet<String>)>> =
        BTreeMap::new();
    // Newest day first, Focuses in id order within a day.
    let mut accs: Vec<_> = accumulate(raw, f).into_iter().collect();
    accs.sort_by(|a, b| b.0 .0.cmp(&a.0 .0).then_with(|| a.0 .1.cmp(&b.0 .1)));
    for ((day, focus), acc) in accs {
        if !acc.visible() {
            continue;
        }
        let g = group_of(&day, &focus, &acc, lk);
        match &cutoff {
            Some(c) if day < *c => month_groups
                .entry(day.chars().take(7).collect())
                .or_default()
                .push((g, acc.runs)),
            _ => groups.push(g),
        }
    }
    let months = month_groups
        .into_iter()
        .rev()
        .map(|(month, gs)| {
            let runs: HashSet<&String> = gs.iter().flat_map(|(_, r)| r.iter()).collect();
            SharedLogMonth {
                month,
                runs: clamp_u32(runs.len() as i64),
                sends: gs.iter().map(|(g, _)| g.sends).sum(),
                kept_back: gs.iter().map(|(g, _)| g.kept_back).sum(),
                groups: gs.into_iter().map(|(g, _)| g).collect(),
            }
        })
        .collect();

    // Filter options and the empty state come from the unfiltered rows.
    let all = accumulate(
        raw,
        &Filters {
            focus_id: None,
            provider_id: None,
        },
    );
    let mut focus_ids: BTreeSet<&str> = BTreeSet::new();
    let mut provider_ids: BTreeSet<&str> = BTreeSet::new();
    let mut has_any = false;
    for ((_, focus), acc) in &all {
        if acc.visible() {
            has_any = true;
            focus_ids.insert(focus.as_str());
            provider_ids.extend(acc.providers.iter().map(String::as_str));
        }
    }
    // What a purge would delete: the SAME rule the purge applies (consent_history_purge::
    // deletable_ids), over each Focus's records oldest first, so the count in the
    // confirm text is the number actually deleted.
    let mut by_focus: BTreeMap<&str, Vec<deletion::KeepRow<'_>>> = BTreeMap::new();
    for c in &raw.consent {
        by_focus
            .entry(c.focus_slug.as_str())
            .or_default()
            .push(deletion::KeepRow {
                id: &c.id,
                event_type: &c.event_type,
                document_type: c.document_type.as_deref(),
            });
    }
    let consent_record_count: usize = by_focus
        .values()
        .map(|rows| deletion::deletable_ids(rows).len())
        .sum();
    let cleared_on = raw
        .consent
        .iter()
        .filter(|c| c.event_type == HISTORY_CLEARED)
        .map(|c| c.created_at.clone())
        .max();
    SharedLogSummary {
        groups,
        months,
        focuses: focus_ids.into_iter().map(|id| lk.focus(id)).collect(),
        providers: provider_ids.into_iter().map(|id| lk.provider(id)).collect(),
        has_any: has_any || cleared_on.is_some(),
        consent_record_count: clamp_u32(consent_record_count as i64),
        cleared_on,
    }
}

fn wire_label(l: deck::Label) -> SharedLogLabel {
    SharedLogLabel {
        kind: match l.kind {
            deck::LabelKind::Catalogued => SharedLogLabelKind::Catalogued,
            deck::LabelKind::Draft => SharedLogLabelKind::Draft,
            deck::LabelKind::Message => SharedLogLabelKind::Message,
            deck::LabelKind::Other => SharedLogLabelKind::Other,
        },
        text: l.text,
    }
}

fn wire_tag(t: Tag) -> SharedLogTag {
    match t {
        Tag::Sent => SharedLogTag::Sent,
        Tag::KeptBack => SharedLogTag::KeptBack,
        Tag::Choice => SharedLogTag::Choice,
        Tag::Hidden => SharedLogTag::Hidden,
    }
}

/// The items of one (day, Focus) group, oldest first. `details` are every disclosure
/// row of that Focus's runs (each with its own local day); `consent` are that day's
/// consent rows; `review_runs` are the runs that only reviewed an API Chat message.
///
/// Sends are folded the way the summary counts them: ONE item per (run, action), made
/// from its sending rows, never one per row (items.id=958 unit 2). Every other row
/// (held, background, a reapplied choice) is one item as before, on its own day.
pub(crate) fn build_items(
    details: &[read::DisclosureDetailRow],
    consent: &[ConsentRowView],
    focus_id: &str,
    day: &str,
    review_runs: &HashSet<String>,
    lk: &Lookups,
    provider_id: Option<&str>,
) -> Vec<SharedLogItem> {
    let mut items: Vec<SharedLogItem> = Vec::new();
    let labels = |names: &[String]| -> Vec<SharedLogLabel> {
        deck::labels_for(names)
            .into_iter()
            .map(wire_label)
            .collect()
    };
    for d in details {
        if d.day != day || SEND_EVENTS.contains(&d.event_type.as_str()) {
            continue;
        }
        let Classified::Item { tag, key } = classify_disclosure(&d.event_type, d.override_declined)
        else {
            continue;
        };
        if let Some(want) = provider_id {
            if tag != Tag::Sent || d.provider.as_deref() != Some(want) {
                continue;
            }
        }
        items.push(SharedLogItem {
            time: d.created_at.clone(),
            tag: wire_tag(tag),
            sentence: key.to_owned(),
            provider: d.provider.as_deref().map(|p| lk.provider(p)),
            shared: labels(&d.shared),
            kept_back: labels(&d.withheld),
            document_type: None,
            summary: None,
        });
    }
    // The sends: group the rows of each (run, action), oldest first.
    let mut actions: BTreeMap<(&str, &str), Vec<&read::DisclosureDetailRow>> = BTreeMap::new();
    for d in details {
        if deck::SEND_EVENTS.contains(&d.event_type.as_str())
            || deck::HOLD_EVENTS.contains(&d.event_type.as_str())
            || deck::REVIEW_MARK_EVENTS.contains(&d.event_type.as_str())
        {
            actions
                .entry((d.focus_run_id.as_str(), d.action_id.as_str()))
                .or_default()
                .push(d);
        }
    }
    for ((run, _), rows) in actions {
        if review_runs.contains(run) {
            continue;
        }
        let is_in =
            |set: &[&str], d: &&read::DisclosureDetailRow| set.contains(&d.event_type.as_str());
        // The action's day is that of its newest sending or held row, as in step 1.
        let action_day = rows
            .iter()
            .filter(|d| is_in(&deck::SEND_EVENTS, d) || is_in(&deck::HOLD_EVENTS, d))
            .map(|d| d.day.as_str())
            .max();
        if action_day != Some(day) {
            continue;
        }
        let outcome = action_outcome(
            rows.iter().any(|d| d.event_type == "gate1_pass"),
            rows.iter().any(|d| {
                d.event_type == "gate3_promotion_approved" || d.event_type == "gate3_pf_no_spans"
            }),
            rows.iter().any(|d| is_in(&deck::REVIEW_MARK_EVENTS, d)),
            rows.iter().any(|d| is_in(&deck::HOLD_EVENTS, d)),
        );
        let Some(key) = outcome.key() else { continue };
        let provider = rows.iter().filter_map(|d| d.provider.as_deref()).max();
        if provider_id.is_some_and(|want| provider != Some(want)) {
            continue;
        }
        let sending: Vec<&&read::DisclosureDetailRow> = rows
            .iter()
            .filter(|d| is_in(&deck::SEND_EVENTS, d))
            .collect();
        // What was shared and kept back: the newest gate1_pass row says it; an action
        // with none (a promotion only) shows its first sending row.
        let shown = sending
            .iter()
            .rev()
            .find(|d| d.event_type == "gate1_pass")
            .or(sending.first());
        let Some(shown) = shown else { continue };
        items.push(SharedLogItem {
            time: sending
                .iter()
                .map(|d| d.created_at.as_str())
                .min()
                .unwrap_or(shown.created_at.as_str())
                .to_owned(),
            tag: SharedLogTag::Sent,
            sentence: key.to_owned(),
            provider: provider.map(|p| lk.provider(p)),
            shared: labels(&shown.shared),
            kept_back: labels(&shown.withheld),
            document_type: None,
            summary: None,
        });
    }
    if provider_id.is_none() {
        for c in consent.iter().filter(|c| c.focus_slug == focus_id) {
            let Some((tag, key)) = classify_consent(&c.event_type, c.has_reference) else {
                continue;
            };
            items.push(SharedLogItem {
                time: c.created_at.clone(),
                tag: wire_tag(tag),
                sentence: key.to_owned(),
                provider: None,
                shared: vec![],
                kept_back: vec![],
                document_type: c
                    .document_type
                    .as_deref()
                    .and_then(deck::document_type_name),
                // A fixed description by decision (434, 944), never content.
                summary: (tag == Tag::Choice).then(|| c.content_summary.clone()),
            });
        }
    }
    items.sort_by(|a, b| a.time.cmp(&b.time));
    items
}

// ---------------------------------------------------------------------------
// Session bodies
// ---------------------------------------------------------------------------

fn local_today(offset_minutes: i32) -> NaiveDate {
    let m = offset_minutes.clamp(-14 * 60, 14 * 60);
    (chrono::Utc::now() + Duration::minutes(i64::from(m))).date_naive()
}

fn parse_day(day: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .filter(|d| d.format("%Y-%m-%d").to_string() == day)
        .ok_or_else(|| "invalid_day".to_owned())
}

fn ids_of_raw(raw: &SharedLogRaw) -> (Vec<String>, Vec<String>) {
    let mut focuses: BTreeSet<String> = raw
        .run_focus
        .keys()
        .map(|run| focus_of(raw, run).to_owned())
        .collect();
    focuses.extend(raw.consent.iter().map(|c| c.focus_slug.clone()));
    focuses.retain(|f| !f.is_empty());
    let providers: BTreeSet<String> = raw
        .aggregates
        .iter()
        .filter_map(|a| a.provider.clone())
        .chain(raw.actions.iter().filter_map(|a| a.provider.clone()))
        .collect();
    (
        focuses.into_iter().collect(),
        providers.into_iter().collect(),
    )
}

#[allow(clippy::too_many_arguments)] // a session body takes what the command derives, as in commands/collection_consent.rs
pub(crate) async fn summary_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    window: SharedLogWindow,
    utc_offset_minutes: i32,
    focus_id: Option<&str>,
    provider_id: Option<&str>,
) -> Result<SharedLogSummary, String> {
    crate::commands::collection_consent::require_persona(pool, user_id, persona_id).await?;
    let raw = read::load_raw(user_id, persona_id, key_hex, None, utc_offset_minutes)
        .await
        .map_err(|e| e.to_string())?;
    let (focuses, providers) = ids_of_raw(&raw);
    let lk = Lookups {
        focus_names: read::focus_names(pool, &focuses)
            .await
            .map_err(|e| e.to_string())?,
        providers: read::provider_infos(pool, &providers)
            .await
            .map_err(|e| e.to_string())?,
    };
    Ok(build_summary(
        &raw,
        &lk,
        window,
        local_today(utc_offset_minutes),
        &Filters {
            focus_id,
            provider_id,
        },
    ))
}

#[allow(clippy::too_many_arguments)] // a session body takes what the command derives, as in commands/collection_consent.rs
pub(crate) async fn items_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    day: &str,
    focus_id: &str,
    utc_offset_minutes: i32,
    provider_id: Option<&str>,
) -> Result<Vec<SharedLogItem>, String> {
    crate::commands::collection_consent::require_persona(pool, user_id, persona_id).await?;
    let date = parse_day(day)?;
    // One day of slack either side of the UTC window covers any local offset.
    let since = format!(
        "{}T00:00:00+00:00",
        (date - Duration::days(1)).format("%Y-%m-%d")
    );
    let raw = read::load_raw(
        user_id,
        persona_id,
        key_hex,
        Some(&since),
        utc_offset_minutes,
    )
    .await
    .map_err(|e| e.to_string())?;
    let run_ids: Vec<String> = raw
        .aggregates
        .iter()
        .filter(|a| a.day == day)
        .map(|a| &a.focus_run_id)
        .chain(
            raw.actions
                .iter()
                .filter(|a| a.day == day)
                .map(|a| &a.focus_run_id),
        )
        .filter(|run| focus_of(&raw, run) == focus_id)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let review_runs: HashSet<String> = run_ids
        .iter()
        .filter(|run| is_review_run(&raw, run))
        .cloned()
        .collect();
    let mut personal =
        crate::persistence::personal_store::open_personal_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| e.to_string())?;
    let details = read::disclosure_details(&mut personal, &run_ids, utc_offset_minutes)
        .await
        .map_err(|e| e.to_string())?;
    drop(personal);
    let consent: Vec<ConsentRowView> = raw
        .consent
        .iter()
        .filter(|c| c.day == day)
        .cloned()
        .collect();
    let provider_ids: Vec<String> = details.iter().filter_map(|d| d.provider.clone()).collect();
    let lk = Lookups {
        focus_names: HashMap::new(),
        providers: read::provider_infos(pool, &provider_ids)
            .await
            .map_err(|e| e.to_string())?,
    };
    Ok(build_items(
        &details,
        &consent,
        focus_id,
        day,
        &review_runs,
        &lk,
        provider_id,
    ))
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_shared_log_summary(
    persona_id: String,
    window: SharedLogWindow,
    utc_offset_minutes: i32,
    focus_id: Option<String>,
    provider_id: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<SharedLogSummary, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    summary_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        window,
        utc_offset_minutes,
        focus_id.as_deref(),
        provider_id.as_deref(),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn get_shared_log_group_items(
    persona_id: String,
    day: String,
    focus_id: String,
    utc_offset_minutes: i32,
    provider_id: Option<String>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Vec<SharedLogItem>, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    items_for_session(
        &pool,
        &user_id,
        &key_hex,
        &persona_id,
        &day,
        &focus_id,
        utc_offset_minutes,
        provider_id.as_deref(),
    )
    .await
}

/// The purge of a Persona's consent records (items.id=957 unit 3, GATE-CRITICAL).
/// `confirm` must be true: the frontend sets it only after the user ticks "I
/// understand this can't be undone" and presses the button. Consent records only:
/// disclosure_log is never deleted (persistence/consent_history_purge.rs). Errors are
/// short codes: confirmation_required, nothing_to_clear, persona_not_found.
pub(crate) async fn clear_history_for_session(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    key_hex: &str,
    persona_id: &str,
    confirm: bool,
) -> Result<u32, String> {
    if !confirm {
        return Err("confirmation_required".to_owned());
    }
    crate::commands::collection_consent::require_persona(pool, user_id, persona_id).await?;
    let mut conn =
        crate::persistence::personal_store::open_personal_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| e.to_string())?;
    match crate::persistence::consent_history_purge::purge_consent_history(&mut conn).await {
        Ok(outcome) => Ok(clamp_u32(
            i64::try_from(outcome.removed).unwrap_or(i64::MAX),
        )),
        Err(crate::persistence::consent_history_purge::PurgeError::NothingToClear) => {
            Err("nothing_to_clear".to_owned())
        }
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn clear_consent_history(
    persona_id: String,
    confirm: bool,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<u32, String> {
    let _lease = crate::auth::writer_gate::command(&key_registry).await?;
    let (user_id, key_hex) = session_user_and_key(&key_registry).await?;
    clear_history_for_session(&pool, &user_id, &key_hex, &persona_id, confirm).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
    use crate::persistence::shared_log_read::{ActionAggregate, DisclosureAggregate};
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::{ConnectOptions, SqliteConnection};

    const RAW_KEY: &str = "SENTINEL_RAW_KEY_4417";
    const RAW_VALUE: &str = "SENTINEL-RAW-VALUE-4417";

    fn agg(
        day: &str,
        run: &str,
        ev: &str,
        prov: Option<&str>,
        n: i64,
        shared: i64,
        withheld: i64,
    ) -> DisclosureAggregate {
        DisclosureAggregate {
            day: day.into(),
            focus_run_id: run.into(),
            event_type: ev.into(),
            provider: prov.map(str::to_owned),
            override_declined: false,
            rows: n,
            shared,
            withheld,
        }
    }

    /// One action: its rows as flags, the way step 1 returns them.
    #[allow(clippy::too_many_arguments)]
    fn act(
        day: &str,
        run: &str,
        action: &str,
        prov: Option<&str>,
        withheld: i64,
        flags: &str,
    ) -> ActionAggregate {
        // flags: g = gate1_pass, a = promotion or no-span approval, m = review mark,
        // h = held row.
        ActionAggregate {
            day: day.into(),
            focus_run_id: run.into(),
            action_id: action.into(),
            provider: prov.map(str::to_owned),
            withheld,
            gate1: flags.contains('g'),
            approved: flags.contains('a'),
            marked: flags.contains('m'),
            held: flags.contains('h'),
        }
    }

    fn consent(day: &str, focus: &str, ev: &str, at: &str) -> ConsentRowView {
        ConsentRowView {
            id: format!("{day}{ev}"),
            day: day.into(),
            created_at: at.into(),
            event_type: ev.into(),
            focus_slug: focus.into(),
            has_reference: false,
            document_type: None,
            content_summary: "A fixed description.".into(),
        }
    }

    fn lookups() -> Lookups {
        Lookups {
            focus_names: HashMap::from([(
                "writing-assistant".to_owned(),
                "Writing Assistant".to_owned(),
            )]),
            providers: HashMap::from([(
                "prov-a".to_owned(),
                ProviderInfo {
                    display_name: "Provider A".into(),
                    mode: ProviderMode::EmbeddedWeb,
                },
            )]),
        }
    }

    fn raw() -> SharedLogRaw {
        SharedLogRaw {
            aggregates: vec![
                agg("2026-11-20", "r1", "gate3_sensitivity_block", None, 1, 0, 1),
                agg(
                    "2026-11-20",
                    "r1",
                    "gate2_contamination_detected",
                    Some("prov-a"),
                    1,
                    0,
                    1,
                ),
                agg("2026-11-19", "r3", "output_scan_light", None, 4, 0, 0),
                agg(
                    "2026-11-18",
                    "r4",
                    "provenance_cross_persona_omitted",
                    None,
                    9,
                    0,
                    9,
                ),
            ],
            actions: vec![
                // r1: two plain sends (one withheld field) and one reviewed send.
                act("2026-11-20", "r1", "a1", Some("prov-a"), 1, "g"),
                act("2026-11-20", "r1", "a2", Some("prov-a"), 0, "g"),
                act("2026-11-20", "r1", "a3", None, 0, "am"),
                act("2026-11-20", "r2", "a4", Some("prov-b"), 0, "g"),
                act("2026-09-02", "r5", "a5", Some("prov-a"), 0, "g"),
            ],
            consent: vec![
                consent(
                    "2026-11-20",
                    "writing-assistant",
                    "collection_consent_granted",
                    "2026-11-20T09:00:00+00:00",
                ),
                consent(
                    "2026-11-20",
                    "writing-assistant",
                    "history_cleared",
                    "2026-11-20T12:00:00+00:00",
                ),
            ],
            run_focus: HashMap::from([
                ("r1".to_owned(), "writing-assistant".to_owned()),
                ("r2".to_owned(), "quick-ask".to_owned()),
                ("r3".to_owned(), "writing-assistant".to_owned()),
                ("r5".to_owned(), "writing-assistant".to_owned()),
            ]),
        }
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 22).unwrap()
    }

    const NONE: Filters<'static> = Filters {
        focus_id: None,
        provider_id: None,
    };

    #[test]
    fn groups_are_one_row_per_day_and_focus_with_counts() {
        let s = build_summary(&raw(), &lookups(), SharedLogWindow::Last30, today(), &NONE);
        // 11-20: writing-assistant and quick-ask. 11-19 is background only: dropped.
        let days: Vec<(&str, &str)> = s
            .groups
            .iter()
            .map(|g| (g.day.as_str(), g.focus.id.as_str()))
            .collect();
        assert_eq!(
            days,
            vec![
                ("2026-11-20", "quick-ask"),
                ("2026-11-20", "writing-assistant")
            ]
        );
        let wa = s
            .groups
            .iter()
            .find(|g| g.focus.id == "writing-assistant")
            .unwrap();
        // Sends: 2 plain + 1 reviewed. Kept back: 1 withheld on gate1 + 1 held item.
        assert_eq!(
            (wa.runs, wa.sends, wa.kept_back, wa.choices, wa.background),
            (1, 3, 2, 1, 1)
        );
        assert!(!wa.all_reviewed);
        assert_eq!(wa.providers.len(), 1);
        assert_eq!(wa.providers[0].name.as_deref(), Some("Provider A"));
        assert_eq!(wa.providers[0].mode, SharedLogMode::Web);
        let q = s.groups.iter().find(|g| g.focus.id == "quick-ask").unwrap();
        assert!(q.focus.quick_chat && q.focus.name.is_none());
        // An unresolved provider has no name.
        assert_eq!(q.providers[0].name, None);
    }

    #[test]
    fn older_days_roll_into_months_and_everything_has_none() {
        let s = build_summary(&raw(), &lookups(), SharedLogWindow::Last30, today(), &NONE);
        assert_eq!(s.months.len(), 1);
        assert_eq!(
            (
                s.months[0].month.as_str(),
                s.months[0].sends,
                s.months[0].runs
            ),
            ("2026-09", 1, 1)
        );
        let all = build_summary(
            &raw(),
            &lookups(),
            SharedLogWindow::Everything,
            today(),
            &NONE,
        );
        assert!(all.months.is_empty() && all.groups.iter().any(|g| g.day == "2026-09-02"));
    }

    #[test]
    fn a_tier_one_only_type_and_a_background_only_day_are_never_listed() {
        let s = build_summary(
            &raw(),
            &lookups(),
            SharedLogWindow::Everything,
            today(),
            &NONE,
        );
        assert!(s
            .groups
            .iter()
            .all(|g| g.day != "2026-11-18" && g.day != "2026-11-19"));
    }

    #[test]
    fn a_provider_filter_keeps_only_sends_to_that_provider() {
        let f = Filters {
            focus_id: None,
            provider_id: Some("prov-a"),
        };
        let s = build_summary(&raw(), &lookups(), SharedLogWindow::Last30, today(), &f);
        assert_eq!(s.groups.len(), 1);
        let g = &s.groups[0];
        // Held items, choices and background checks are hidden under a provider filter.
        assert_eq!((g.sends, g.choices, g.background), (2, 0, 0));
        // Options still come from the unfiltered data.
        assert_eq!(s.providers.len(), 2);
    }

    #[test]
    fn a_focus_filter_narrows_and_the_options_stay_whole() {
        let f = Filters {
            focus_id: Some("quick-ask"),
            provider_id: None,
        };
        let s = build_summary(&raw(), &lookups(), SharedLogWindow::Last30, today(), &f);
        assert_eq!(s.groups.len(), 1);
        assert_eq!(s.focuses.len(), 2);
    }

    #[test]
    fn the_empty_state_the_cleared_line_and_the_record_count() {
        let empty = build_summary(
            &SharedLogRaw::default(),
            &lookups(),
            SharedLogWindow::Last30,
            today(),
            &NONE,
        );
        assert!(!empty.has_any && empty.groups.is_empty() && empty.cleared_on.is_none());
        let s = build_summary(&raw(), &lookups(), SharedLogWindow::Last30, today(), &NONE);
        // An in-force grant and the cleared marker are never counted: the count is what a
        // purge would DELETE, so nothing here.
        assert_eq!(s.consent_record_count, 0);
        // Older records are counted by the purge's own rule: the superseded grant, the
        // withdrawal and the other type (3), not the newest grant.
        let with = |id: &str, ev: &str| ConsentRowView {
            id: id.into(),
            ..consent(
                "2026-11-20",
                "tech-support",
                ev,
                "2026-11-20T09:00:00+00:00",
            )
        };
        let older = SharedLogRaw {
            consent: vec![
                with("g1", "collection_consent_granted"),
                with("w1", "collection_consent_withdrawn"),
                with("g2", "collection_consent_granted"),
                with("o1", "sensitivity_override"),
            ],
            ..Default::default()
        };
        let counted = build_summary(&older, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        assert_eq!(counted.consent_record_count, 3);
        assert_eq!(s.cleared_on.as_deref(), Some("2026-11-20T12:00:00+00:00"));
        // A Persona that only has a cleared marker is not "nothing".
        let only = SharedLogRaw {
            consent: vec![consent(
                "2026-11-20",
                "x",
                "history_cleared",
                "2026-11-20T12:00:00+00:00",
            )],
            ..Default::default()
        };
        assert!(build_summary(&only, &lookups(), SharedLogWindow::Last30, today(), &NONE).has_any);
    }

    fn run_of(actions: Vec<ActionAggregate>) -> SharedLogRaw {
        let runs: HashSet<String> = actions.iter().map(|a| a.focus_run_id.clone()).collect();
        SharedLogRaw {
            actions,
            run_focus: runs
                .into_iter()
                .map(|r| (r, "writing-assistant".to_owned()))
                .collect(),
            ..Default::default()
        }
    }

    fn one_group(raw: &SharedLogRaw, f: &Filters<'_>) -> SharedLogGroup {
        let s = build_summary(raw, &lookups(), SharedLogWindow::Last30, today(), f);
        assert_eq!(s.groups.len(), 1, "{:?}", s.groups);
        s.groups[0].clone()
    }

    #[test]
    fn all_reviewed_only_when_every_send_went_through_review() {
        // Two actions each reviewed by a promotion: two sends, both reviewed.
        let r = run_of(vec![
            act("2026-11-20", "r1", "a1", None, 0, "am"),
            act("2026-11-20", "r1", "a2", None, 0, "am"),
        ]);
        let g = one_group(&r, &NONE);
        assert!(g.all_reviewed && g.sends == 2);
        // A plain send beside them: no longer every send.
        let r = run_of(vec![
            act("2026-11-20", "r1", "a1", None, 0, "am"),
            act("2026-11-20", "r1", "a2", None, 0, "g"),
        ]);
        let g = one_group(&r, &NONE);
        assert!(!g.all_reviewed && g.sends == 2);
    }

    /// items.id=958 unit 2, one send per (run, action): the rows one send writes are
    /// ONE send, and what the review adds decides whether it reads reviewed.
    #[test]
    fn a_hosted_send_reads_one_send_whatever_rows_it_wrote() {
        // gate1_pass + a no-span review row: one PLAIN send (sentNoReview), not reviewed.
        let g = one_group(
            &run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 2, "ga")]),
            &NONE,
        );
        assert_eq!((g.sends, g.all_reviewed, g.kept_back), (1, false, 2));
        // gate1_pass + a promotion: one reviewed send.
        let g = one_group(
            &run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 2, "gam")]),
            &NONE,
        );
        assert_eq!((g.sends, g.all_reviewed, g.kept_back), (1, true, 2));
        // A pending consent, then an answer and a second gate1_pass: one reviewed send.
        let g = one_group(
            &run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 2, "gm")]),
            &NONE,
        );
        assert_eq!((g.sends, g.all_reviewed), (1, true));
        // A gate1_pass only (a step with no review row, a bound API Chat run): plain.
        let g = one_group(
            &run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 0, "g")]),
            &NONE,
        );
        assert_eq!((g.sends, g.all_reviewed), (1, false));
        // Withheld counts once per send, from the gate1_pass: a second action adds its own.
        let g = one_group(
            &run_of(vec![
                act("2026-11-20", "r1", "a", Some("prov-a"), 2, "gam"),
                act("2026-11-20", "r1", "b", Some("prov-a"), 1, "g"),
            ]),
            &NONE,
        );
        assert_eq!((g.sends, g.kept_back), (2, 3));
    }

    #[test]
    fn a_held_action_is_no_send_and_its_held_row_still_counts_as_kept_back() {
        let mut r = run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 2, "gh")]);
        r.aggregates = vec![agg(
            "2026-11-20",
            "r1",
            "gate3_pf_unavailable_block",
            Some("prov-a"),
            1,
            0,
            1,
        )];
        let g = one_group(&r, &NONE);
        // Nothing was sent; one item was held (the gate1_pass's withheld is not counted).
        assert_eq!((g.sends, g.kept_back), (0, 1));
        // A retry that then went out reads one send, still with the held row.
        r.actions[0].approved = true;
        let g = one_group(&r, &NONE);
        assert_eq!((g.sends, g.kept_back), (1, 3));
    }

    #[test]
    fn a_reapplied_choice_is_shown_but_is_not_a_send() {
        let mut r = SharedLogRaw {
            aggregates: vec![agg(
                "2026-11-20",
                "r1",
                "gate3_fact_reapplied",
                Some("prov-a"),
                3,
                0,
                0,
            )],
            run_focus: HashMap::from([("r1".to_owned(), "writing-assistant".to_owned())]),
            ..Default::default()
        };
        let s = build_summary(&r, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        assert!(
            s.groups.is_empty(),
            "a reapplied choice alone is not a send"
        );
        // Beside a gate1_pass it makes that send reviewed and adds none.
        r.actions = vec![act("2026-11-20", "r1", "a", Some("prov-a"), 0, "gm")];
        let g = one_group(&r, &NONE);
        assert_eq!((g.sends, g.all_reviewed), (1, true));
    }

    /// items.id=958 unit 2: the synthetic run that reviews one API Chat message never
    /// counts as a send; the api-chat run's own gate1_pass is the send, and it reads plain.
    #[test]
    fn the_api_chat_review_run_never_counts_and_its_send_reads_plain() {
        let raw = SharedLogRaw {
            aggregates: vec![
                agg(
                    "2026-11-20",
                    "stub",
                    "gate3_consent_pending",
                    Some("prov-a"),
                    1,
                    0,
                    1,
                ),
                agg(
                    "2026-11-20",
                    "stub",
                    "gate3_pf_unavailable_block",
                    Some("prov-a"),
                    1,
                    0,
                    1,
                ),
            ],
            actions: vec![
                // The review's own approval row, under the stub run, and the send.
                act(
                    "2026-11-20",
                    "stub",
                    "api-chat-send",
                    Some("prov-a"),
                    0,
                    "a",
                ),
                act("2026-11-20", "chat", "a4", Some("prov-a"), 2, "g"),
            ],
            consent: vec![],
            run_focus: HashMap::from([
                ("stub".to_owned(), "api-chat-send".to_owned()),
                ("chat".to_owned(), "api-chat".to_owned()),
            ]),
        };
        let s = build_summary(&raw, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        assert_eq!(
            s.groups.len(),
            1,
            "one group, not one per run: {:?}",
            s.groups
        );
        let g = &s.groups[0];
        assert_eq!(g.focus.id, "api-chat");
        // One plain send; the stub's held row and the send's withheld count as kept back;
        // the stub's pending consent is a background check of the same group.
        assert_eq!(
            (g.sends, g.all_reviewed, g.kept_back, g.background),
            (1, false, 3, 1)
        );
        assert!(s.focuses.iter().all(|f| f.id != "api-chat-send"));
        // The filter keeps the one send.
        let f = Filters {
            focus_id: None,
            provider_id: Some("prov-a"),
        };
        assert_eq!(one_group(&raw, &f).sends, 1);
    }

    /// items.id=958: a reviewed send with a provider is named in the group and kept by
    /// that provider's filter; the same send with no provider (an older row) keeps the
    /// fallback and is absent under the filter.
    #[test]
    fn a_reviewed_send_with_a_provider_is_named_and_survives_the_provider_filter() {
        let f = Filters {
            focus_id: None,
            provider_id: Some("prov-a"),
        };
        let named = run_of(vec![act("2026-11-20", "r1", "a", Some("prov-a"), 0, "gam")]);
        let g = one_group(&named, &NONE);
        assert!(g.all_reviewed && g.sends == 1);
        assert_eq!(g.providers[0].name.as_deref(), Some("Provider A"));
        let g = one_group(&named, &f);
        assert_eq!((g.sends, g.all_reviewed), (1, true));
        // Another provider's filter does not keep it.
        let other = Filters {
            focus_id: None,
            provider_id: Some("prov-b"),
        };
        assert!(
            build_summary(&named, &lookups(), SharedLogWindow::Last30, today(), &other)
                .groups
                .is_empty()
        );

        let unnamed = run_of(vec![act("2026-11-20", "r1", "a", None, 0, "gam")]);
        let g = one_group(&unnamed, &NONE);
        assert!(
            g.providers.is_empty() && g.sends == 1,
            "no provider listed: the view falls back to the generic wording"
        );
        let s = build_summary(&unnamed, &lookups(), SharedLogWindow::Last30, today(), &f);
        assert!(s.groups.is_empty());
    }

    // ---- the group's items fold the same way ---------------------------------

    #[allow(clippy::too_many_arguments)]
    fn det(
        run: &str,
        action: &str,
        day: &str,
        at: &str,
        ev: &str,
        prov: Option<&str>,
        shared: &[&str],
        withheld: &[&str],
    ) -> read::DisclosureDetailRow {
        read::DisclosureDetailRow {
            focus_run_id: run.into(),
            action_id: action.into(),
            day: day.into(),
            created_at: at.into(),
            event_type: ev.into(),
            provider: prov.map(str::to_owned),
            override_declined: false,
            shared: shared.iter().map(|s| (*s).to_owned()).collect(),
            withheld: withheld.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn items_of(
        rows: &[read::DisclosureDetailRow],
        review_runs: &[&str],
        day: &str,
        provider: Option<&str>,
    ) -> Vec<SharedLogItem> {
        let review: HashSet<String> = review_runs.iter().map(|r| (*r).to_owned()).collect();
        build_items(
            rows,
            &[],
            "writing-assistant",
            day,
            &review,
            &lookups(),
            provider,
        )
    }

    #[test]
    fn one_send_is_one_item_with_the_review_deciding_its_sentence() {
        let d = "2026-11-20";
        // gate1_pass + promotion: ONE reviewed item that shows what gate1 shared.
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &["job_title"],
                &["draft"],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_promotion_approved",
                Some("prov-a"),
                &["x"],
                &[],
            ),
        ];
        let items = items_of(&rows, &[], d, None);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(
            (items[0].tag, items[0].sentence.as_str()),
            (SharedLogTag::Sent, "sent.reviewed")
        );
        assert_eq!(items[0].shared[0].text.as_deref(), Some("Job title"));
        assert_eq!(items[0].kept_back[0].kind, SharedLogLabelKind::Draft);
        assert_eq!(
            items[0].provider.as_ref().unwrap().name.as_deref(),
            Some("Provider A")
        );
        // gate1_pass + no-span row: one item, the no-review sentence.
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_pf_no_spans",
                Some("prov-a"),
                &[],
                &[],
            ),
        ];
        let items = items_of(&rows, &[], d, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sentence, "sent.noReview");
        // gate1_pass alone, twice (a retry): one plain item.
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:09:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
        ];
        let items = items_of(&rows, &[], d, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sentence, "sent.plain");
        assert_eq!(items[0].time, "2026-11-20T10:00:00+00:00");
        // A pending consent beside the gate1_pass: reviewed, and the background row shows too.
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_consent_pending",
                Some("prov-a"),
                &[],
                &[],
            ),
        ];
        let items = items_of(&rows, &[], d, None);
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.sentence == "sent.reviewed"));
        assert!(items.iter().any(|i| i.tag == SharedLogTag::Hidden));
    }

    #[test]
    fn a_held_action_shows_the_held_item_and_no_send_item() {
        let d = "2026-11-20";
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_pf_unavailable_block",
                Some("prov-a"),
                &[],
                &["draft"],
            ),
        ];
        let items = items_of(&rows, &[], d, None);
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0].tag, items[0].sentence.as_str()),
            (SharedLogTag::KeptBack, "held.pfUnavailable")
        );
    }

    #[test]
    fn the_api_chat_review_runs_rows_are_not_send_items_and_a_send_shows_on_its_newest_day_only() {
        let d = "2026-11-20";
        // The stub run's approval row is not an item; its pending consent still is.
        let rows = [
            det(
                "stub",
                "api-chat-send",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate3_pf_no_spans",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "stub",
                "api-chat-send",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_consent_pending",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "chat",
                "a4",
                d,
                "2026-11-20T10:00:02+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
        ];
        let items = items_of(&rows, &["stub"], d, None);
        let sentences: Vec<&str> = items.iter().map(|i| i.sentence.as_str()).collect();
        assert_eq!(sentences, ["bg.reviewRaised", "sent.plain"]);
        // An action across two days: the second day's gate1_pass is the newest, so it is
        // that day's send and the earlier day shows only its background row.
        let rows = [
            det(
                "r1",
                "a",
                "2026-11-19",
                "2026-11-19T23:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                "2026-11-19",
                "2026-11-19T23:01:00+00:00",
                "gate3_consent_pending",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                "2026-11-20",
                "2026-11-20T09:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
        ];
        let early = items_of(&rows, &[], "2026-11-19", None);
        assert!(
            early.iter().all(|i| i.tag != SharedLogTag::Sent),
            "{early:?}"
        );
        let late = items_of(&rows, &[], "2026-11-20", None);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].sentence, "sent.reviewed");
    }

    #[test]
    fn the_provider_filter_keeps_only_the_sends_of_that_provider_in_the_items() {
        let d = "2026-11-20";
        let rows = [
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:00+00:00",
                "gate1_pass",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "a",
                d,
                "2026-11-20T10:00:01+00:00",
                "gate3_promotion_approved",
                Some("prov-a"),
                &[],
                &[],
            ),
            det(
                "r1",
                "b",
                d,
                "2026-11-20T10:01:00+00:00",
                "gate1_pass",
                Some("prov-b"),
                &[],
                &[],
            ),
            det(
                "r1",
                "c",
                d,
                "2026-11-20T10:02:00+00:00",
                "gate3_sensitivity_block",
                None,
                &[],
                &["draft"],
            ),
        ];
        let items = items_of(&rows, &[], d, Some("prov-a"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sentence, "sent.reviewed");
    }

    async fn personal_db() -> SqliteConnection {
        let mut conn = SqliteConnectOptions::new()
            .filename(":memory:")
            .connect()
            .await
            .unwrap();
        for sql in [
            include_str!("../../schema/personal_001.sql"),
            include_str!("../../schema/personal_008.sql"),
            include_str!("../../schema/personal_010.sql"),
        ] {
            for stmt in parse_statements(sql) {
                sqlx::query(&stmt).execute(&mut conn).await.unwrap();
            }
        }
        conn
    }

    /// A raw field name placed in fields_shared, fields_abstracted (as a key) and
    /// fields_withheld, and a raw value in fields_abstracted, travel through the real
    /// SQL read and the item builder: neither appears in the serialized command output.
    #[tokio::test]
    async fn no_raw_field_name_or_value_reaches_the_serialized_output() {
        let mut c = personal_db().await;
        sqlx::query(
            "INSERT INTO disclosure_log
             (id, user_id, persona_id, focus_run_id, action_id, routing_tier, provider,
              fields_shared, fields_abstracted, fields_withheld, override_declined,
              created_at, extra_metadata, execution_tier)
             VALUES ('1', 'u', 'p', 'r1', 'a', 2, 'prov-a', ?, ?, ?, 0,
                     '2026-11-20T10:00:00+00:00', '{\"event_type\":\"gate1_pass\"}', 2)",
        )
        .bind(format!("[\"job_title\",\"{RAW_KEY}_s\"]"))
        .bind(format!("{{\"{RAW_KEY}_a\":\"{RAW_VALUE}\"}}"))
        .bind(format!("[\"{RAW_KEY}_w\"]"))
        .execute(&mut c)
        .await
        .unwrap();
        let aggs = read::disclosure_aggregates(&mut c, None, 0).await.unwrap();
        let details = read::disclosure_details(&mut c, &["r1".into()], 0)
            .await
            .unwrap();
        let raw = SharedLogRaw {
            aggregates: aggs.events,
            actions: aggs.actions,
            consent: vec![],
            run_focus: HashMap::from([("r1".to_owned(), "writing-assistant".to_owned())]),
        };
        let summary = build_summary(&raw, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        let items = build_items(
            &details,
            &[],
            "writing-assistant",
            "2026-11-20",
            &HashSet::new(),
            &lookups(),
            None,
        );
        let out = format!(
            "{} {}",
            serde_json::to_string(&summary_json(&summary)).unwrap(),
            serde_json::to_string(&items_json(&items)).unwrap()
        );
        assert!(!out.contains(RAW_KEY), "a raw field name leaked: {out}");
        assert!(!out.contains(RAW_VALUE), "a value leaked: {out}");
        assert!(
            !out.contains("gate1_pass"),
            "a raw event string leaked: {out}"
        );
        // The catalogued name came out as its label, the rest as one "other".
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].shared.len(), 2);
        assert_eq!(items[0].shared[0].text.as_deref(), Some("Job title"));
        assert_eq!(items[0].shared[1].kind, SharedLogLabelKind::Other);
        assert_eq!(items[0].kept_back[0].kind, SharedLogLabelKind::Other);
    }

    // Serialized with the real serde derives, as the IPC layer does.
    fn summary_json(s: &SharedLogSummary) -> serde_json::Value {
        serde_json::to_value(s).unwrap()
    }
    fn items_json(i: &[SharedLogItem]) -> serde_json::Value {
        serde_json::to_value(i).unwrap()
    }

    #[test]
    fn items_carry_sentence_keys_labels_and_a_fixed_consent_summary() {
        let det = vec![
            read::DisclosureDetailRow {
                focus_run_id: "r1".into(),
                action_id: "a".into(),
                day: "2026-11-20".into(),
                created_at: "2026-11-20T10:00:00+00:00".into(),
                event_type: "gate3_sensitivity_block".into(),
                provider: None,
                override_declined: true,
                shared: vec![],
                withheld: vec!["draft".into()],
            },
            read::DisclosureDetailRow {
                focus_run_id: "r1".into(),
                action_id: "a".into(),
                day: "2026-11-20".into(),
                created_at: "2026-11-20T10:01:00+00:00".into(),
                event_type: "provenance_cross_persona_omitted".into(),
                provider: None,
                override_declined: false,
                shared: vec![],
                withheld: vec!["other-persona-entity:serial".into()],
            },
        ];
        let mut ceiling = consent(
            "2026-11-20",
            "writing-assistant",
            "document_ceiling_raised",
            "2026-11-20T10:02:00+00:00",
        );
        ceiling.document_type = Some("cover_letter".into());
        let items = build_items(
            &det,
            &[ceiling],
            "writing-assistant",
            "2026-11-20",
            &HashSet::new(),
            &lookups(),
            None,
        );
        // The tier-1 provenance row is dropped; the block and the choice remain.
        assert_eq!(items.len(), 2);
        assert_eq!(
            (items[0].tag, items[0].sentence.as_str()),
            (SharedLogTag::KeptBack, "held.sensitivity")
        );
        assert_eq!(items[0].kept_back[0].kind, SharedLogLabelKind::Draft);
        assert_eq!(items[1].document_type.as_deref(), Some("Cover letter"));
        assert_eq!(items[1].summary.as_deref(), Some("A fixed description."));
        // Another Focus's consent rows do not appear.
        assert!(build_items(
            &[],
            &[consent(
                "2026-11-20",
                "other",
                "collection_consent_granted",
                "t"
            )],
            "writing-assistant",
            "2026-11-20",
            &HashSet::new(),
            &lookups(),
            None
        )
        .is_empty());
    }

    /// Not a gate: how many summary rows a heavy month produces, and what it costs.
    /// Run with `cargo test --lib summary_size -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn summary_size_for_a_heavy_month() {
        let held_and_background = ["gate3_sensitivity_block", "output_scan_light"];
        let mut aggregates = Vec::new();
        let mut actions = Vec::new();
        let mut run_focus = HashMap::new();
        for i in 0..27_772usize {
            let day = format!("2026-11-{:02}", 1 + i % 22);
            let run = format!("run-{}", i / 5);
            run_focus.insert(
                run.clone(),
                ["writing-assistant", "quick-ask", "research-and-buy"][i % 3].to_owned(),
            );
            if i % 4 < 2 {
                // Sends: a plain one and a reviewed one, one action each.
                let flags = if i % 4 == 0 { "g" } else { "am" };
                actions.push(act(&day, &run, &format!("a{i}"), Some("prov-a"), 1, flags));
            } else {
                let ev = held_and_background[i % 2];
                aggregates.push(agg(&day, &run, ev, Some("prov-a"), 1, 2, 1));
            }
        }
        let raw = SharedLogRaw {
            aggregates,
            actions,
            consent: vec![],
            run_focus,
        };
        let t = std::time::Instant::now();
        let s = build_summary(&raw, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        let took = t.elapsed();
        let bytes = serde_json::to_string(&s).unwrap().len();
        println!(
            "{} groups, {} bytes of JSON, built in {took:?}",
            s.groups.len(),
            bytes
        );
    }

    #[test]
    fn a_day_must_be_a_real_calendar_date() {
        assert!(parse_day("2026-11-20").is_ok());
        for bad in ["2026-13-01", "2026-11-31", "20261120", "", "2026-1-2", "x"] {
            assert!(parse_day(bad).is_err(), "{bad}");
        }
    }

    /// Each command takes the writer gate first and derives the user from the session.
    #[test]
    fn every_command_takes_the_gate_first_and_derives_the_user_from_the_session() {
        let src = include_str!("shared_log.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for name in [
            "get_shared_log_summary",
            "get_shared_log_group_items",
            "clear_consent_history",
        ] {
            let body = production
                .split(&format!("pub async fn {name}("))
                .nth(1)
                .unwrap_or_else(|| panic!("{name} missing"));
            let body = body.split("\n}\n").next().unwrap();
            let gate = body.find("writer_gate::command").expect("gate");
            let session = body.find("session_user_and_key").expect("session");
            assert!(gate < session, "{name}");
            assert!(
                !body.contains("user_id: String"),
                "{name} takes a client user id"
            );
        }
    }

    // -- clear_consent_history: real encrypted files under a scratch QR_DATA_ROOT --

    use crate::auth::registry::key_hex;
    use crate::persistence::consent_audit_store::{
        self as cstore, ConsentEventType, NewConsentEvent,
    };
    use crate::persistence::persona_store;
    use crate::test_support::ENV_MUTEX;
    use sqlx::Row;

    const MASTER_KEY: [u8; crate::auth::kdf::MASTER_KEY_LEN] =
        [0xABu8; crate::auth::kdf::MASTER_KEY_LEN];

    struct Env {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
        user: String,
        persona: String,
        other_persona: String,
        key: String,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn env() -> Env {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();
        let tempdir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());
        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db");
        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("pool");
        let key = key_hex(&MASTER_KEY);
        let user = uuid::Uuid::new_v4().to_string();
        crate::auth::user_store::create_user(
            &pool,
            &user,
            "Purge User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user");
        let mut ids = Vec::new();
        for _ in 0..2 {
            let id = uuid::Uuid::new_v4().to_string();
            persona_store::create_persona(&pool, &id, "P", "personal", &user, &key, None)
                .await
                .expect("create_persona");
            ids.push(id);
        }
        // The settings row the consent commands read (the Focus the grant is about).
        crate::persistence::focus_settings_store::create_focus_settings(
            &pool,
            &ids[0],
            "tech-support",
            "bidirectional",
            "shared",
            2,
            crate::conductor::tokens::ExternalAccess::LocalOnly,
            "open",
            None,
        )
        .await
        .expect("focus_settings row");
        Env {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
            user,
            persona: ids[0].clone(),
            other_persona: ids[1].clone(),
            key,
        }
    }

    async fn seed(e: &Env, persona: &str, consents: usize, disclosures: usize) {
        let mut c = crate::persistence::personal_store::open_personal_db(&e.user, persona, &e.key)
            .await
            .unwrap();
        for _ in 0..consents {
            cstore::append(
                &mut c,
                &NewConsentEvent {
                    event_type: ConsentEventType::SensitivityOverride,
                    focus_slug: "writing-assistant",
                    run_id: None,
                    text_version: None,
                    consent_reference_id: None,
                    content_summary: "A fixed description.",
                    document_type: None,
                },
            )
            .await
            .unwrap();
        }
        for i in 0..disclosures {
            sqlx::query(
                "INSERT INTO disclosure_log
                 (id, user_id, persona_id, focus_run_id, action_id, routing_tier, created_at,
                  extra_metadata, execution_tier)
                 VALUES (?, 'u', 'p', 'r', 'a', 2, '2026-11-20T10:00:00+00:00',
                         '{\"event_type\":\"gate1_pass\"}', 2)",
            )
            .bind(format!("{persona}-{i}"))
            .execute(&mut c)
            .await
            .unwrap();
        }
    }

    async fn counts(e: &Env, persona: &str) -> (i64, i64, i64) {
        let mut c = crate::persistence::personal_store::open_personal_db(&e.user, persona, &e.key)
            .await
            .unwrap();
        let consent = sqlx::query(
            "SELECT COUNT(*) FROM consent_audit_records WHERE event_type != 'history_cleared'",
        )
        .fetch_one(&mut c)
        .await
        .unwrap()
        .get::<i64, _>(0);
        let markers = sqlx::query(
            "SELECT COUNT(*) FROM consent_audit_records WHERE event_type = 'history_cleared'",
        )
        .fetch_one(&mut c)
        .await
        .unwrap()
        .get::<i64, _>(0);
        let disclosure = sqlx::query("SELECT COUNT(*) FROM disclosure_log")
            .fetch_one(&mut c)
            .await
            .unwrap()
            .get::<i64, _>(0);
        (consent, markers, disclosure)
    }

    #[tokio::test]
    async fn the_purge_needs_confirm_and_a_persona_the_user_owns() {
        let e = env().await;
        seed(&e, &e.persona, 2, 3).await;
        // No confirmation: refused before anything is opened, nothing changes.
        assert_eq!(
            clear_history_for_session(&e.pool, &e.user, &e.key, &e.persona, false).await,
            Err("confirmation_required".to_owned())
        );
        // A non-UUID Persona and a Persona of another user are refused.
        for bad in ["not-a-uuid".to_owned(), uuid::Uuid::new_v4().to_string()] {
            assert_eq!(
                clear_history_for_session(&e.pool, &e.user, &e.key, &bad, true).await,
                Err("persona_not_found".to_owned()),
                "{bad}"
            );
        }
        assert_eq!(counts(&e, &e.persona).await, (2, 0, 3));
    }

    #[tokio::test]
    async fn the_purge_removes_this_personas_consent_records_and_nothing_else() {
        let e = env().await;
        seed(&e, &e.persona, 2, 3).await;
        seed(&e, &e.other_persona, 4, 5).await;
        assert_eq!(
            clear_history_for_session(&e.pool, &e.user, &e.key, &e.persona, true).await,
            Ok(2)
        );
        // This Persona: no consent record, one marker, every disclosure row kept.
        assert_eq!(counts(&e, &e.persona).await, (0, 1, 3));
        // The other Persona is untouched.
        assert_eq!(counts(&e, &e.other_persona).await, (4, 0, 5));
        // Nothing left to clear: an error code, and no second marker.
        assert_eq!(
            clear_history_for_session(&e.pool, &e.user, &e.key, &e.persona, true).await,
            Err("nothing_to_clear".to_owned())
        );
        assert_eq!(counts(&e, &e.persona).await, (0, 1, 3));
    }

    /// The ruling's end-to-end case, on real encrypted files: an in-force grant and an
    /// in-force raise survive; everything older goes; and the consent commands and the
    /// raise's own lookup still work on what is left.
    #[tokio::test]
    async fn a_purge_keeps_what_is_in_force_and_the_consent_commands_still_work() {
        use crate::commands::collection_consent::{
            submit_decision_for_session, withdraw_for_session, CollectionConsentDecision,
        };
        use crate::commands::document_raise::{previous_raise, Ids};
        use crate::conductor::collection_consent_text::current_version;
        let e = env().await;
        let hook = || Ok(());
        let cv = current_version();
        let grant = || {
            submit_decision_for_session(
                &e.pool,
                &e.user,
                &e.key,
                &e.persona,
                "tech-support",
                CollectionConsentDecision::Granted,
                cv,
                &hook,
            )
        };
        // grant, withdraw, grant again: three collection events, the last is in force.
        grant().await.unwrap();
        withdraw_for_session(&e.pool, &e.user, &e.key, &e.persona, "tech-support", &hook)
            .await
            .unwrap();
        grant().await.unwrap();
        // Two raises of one document type (the second is in force) and an unrelated event.
        let mut c =
            crate::persistence::personal_store::open_personal_db(&e.user, &e.persona, &e.key)
                .await
                .unwrap();
        let mut raise = async |doc: &str| {
            cstore::append(
                &mut c,
                &NewConsentEvent {
                    event_type: ConsentEventType::DocumentCeilingRaised,
                    focus_slug: "writing-assistant",
                    run_id: None,
                    text_version: None,
                    consent_reference_id: None,
                    content_summary: "A fixed description.",
                    document_type: Some(doc),
                },
            )
            .await
            .unwrap()
        };
        let _old_raise = raise("cover_letter").await;
        let new_raise = raise("cover_letter").await;
        drop(c);
        seed(&e, &e.persona, 1, 4).await; // one SensitivityOverride, four disclosure rows

        let kept_grant = {
            let mut c =
                crate::persistence::personal_store::open_personal_db(&e.user, &e.persona, &e.key)
                    .await
                    .unwrap();
            let all = cstore::list(&mut c, "tech-support").await.unwrap();
            assert_eq!(all.len(), 3);
            all.last().unwrap().id.clone()
        };
        // Deleted: the older grant, the withdrawal, the older raise, the override = 4.
        assert_eq!(
            clear_history_for_session(&e.pool, &e.user, &e.key, &e.persona, true).await,
            Ok(4)
        );
        let mut c =
            crate::persistence::personal_store::open_personal_db(&e.user, &e.persona, &e.key)
                .await
                .unwrap();
        let left = cstore::list(&mut c, "tech-support").await.unwrap();
        assert_eq!(
            left.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
            vec![kept_grant.clone()]
        );
        let markers = cstore::list(&mut c, "*").await.unwrap();
        assert_eq!(markers.len(), 1);
        drop(c);
        assert_eq!(counts(&e, &e.persona).await, (2, 1, 4)); // grant + raise, one marker, log intact

        // The raise's own lookup still sees the surviving record.
        let ids = Ids {
            user_id: &e.user,
            key_hex: &e.key,
            persona_id: &e.persona,
            context_key: "ctx",
            focus_id: "writing-assistant",
        };
        let prev = previous_raise(&ids, "cover_letter")
            .await
            .unwrap()
            .expect("kept raise");
        assert_eq!(prev.id, new_raise);

        // A grant again is idempotent: no second grant event.
        grant().await.unwrap();
        let mut c =
            crate::persistence::personal_store::open_personal_db(&e.user, &e.persona, &e.key)
                .await
                .unwrap();
        assert_eq!(cstore::list(&mut c, "tech-support").await.unwrap().len(), 1);
        drop(c);
        // Only kept records remain: a second purge writes nothing.
        assert_eq!(
            clear_history_for_session(&e.pool, &e.user, &e.key, &e.persona, true).await,
            Err("nothing_to_clear".to_owned())
        );
        assert_eq!(counts(&e, &e.persona).await, (2, 1, 4));
        // A withdrawal still works and references the surviving grant.
        withdraw_for_session(&e.pool, &e.user, &e.key, &e.persona, "tech-support", &hook)
            .await
            .unwrap();
        let mut c =
            crate::persistence::personal_store::open_personal_db(&e.user, &e.persona, &e.key)
                .await
                .unwrap();
        let after = cstore::list(&mut c, "tech-support").await.unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(
            after[1].event_type,
            ConsentEventType::CollectionConsentWithdrawn
        );
        assert_eq!(
            after[1].consent_reference_id.as_deref(),
            Some(kept_grant.as_str())
        );
    }

    /// items.id=958 unit 2: the double count, reproduced with ROWS WRITTEN BY THE REAL
    /// WRITERS into an encrypted personal.db and read back by the real read path.
    ///
    /// REAL: every gate1_pass (gate1 through PrivacyGateway) and the legacy-path
    /// gate3_promotion_approved (gate3 itself, without the Privacy Filter).
    /// EMULATED, because the Privacy Filter and an app handle are not available in a
    /// unit test: `emulated_no_spans_row` (gate3_pf_no_spans), `emulated_consent_pending_row`
    /// (gate3_consent_pending) and `emulated_unavailable_block_row`
    /// (gate3_pf_unavailable_block). Each is written through the real logger with the
    /// entry shape gate3_with_pf / pf_unavailable_block build (gate3.rs), so the read
    /// path sees what a real review would leave.
    #[tokio::test]
    async fn real_writers_leave_rows_that_read_as_one_send_each() {
        use crate::conductor::privacy::gate3::KeptPrivate;
        use crate::conductor::privacy::logger::{DisclosureLogEntry, DisclosureLogger};
        use crate::conductor::privacy::types::{
            AbstractionPolicy, PersonalField, PersonalTrack, Sensitivity,
        };
        use crate::conductor::privacy::PrivacyGateway;
        use crate::conductor::tokens::ExternalAccess;
        use crate::persistence::disclosure_log_store::SqliteDisclosureLogger;
        type Gw = PrivacyGateway<SqliteDisclosureLogger>;

        let e = env().await;
        let gw = PrivacyGateway::new(SqliteDisclosureLogger::new(&e.user, &e.persona, &e.key));
        let mut track = PersonalTrack::new();
        for (n, v) in [("city", "Leeds"), ("employer", "Acme")] {
            track
                .add_field(PersonalField {
                    field_name: n.into(),
                    field_value: v.into(),
                    sensitivity: Sensitivity::Personal,
                    sensitivity_severity: 2,
                    source_id: "s".into(),
                    abstraction_tier2: AbstractionPolicy::Omit,
                    abstraction_tier3: AbstractionPolicy::Omit,
                })
                .unwrap();
        }
        track.seal();
        let scope: HashSet<String> = ["city".to_owned(), "employer".to_owned()].into();
        for (run, focus) in [
            ("r-s1", "writing-assistant"),
            ("r-s2", "writing-assistant"),
            ("r-s3", "writing-assistant"),
            ("r-s5", "writing-assistant"),
            ("r-s6", "writing-assistant"),
            ("api-chat-send-stub", "api-chat-send"),
            ("r-chat", "api-chat"),
        ] {
            crate::persistence::output_store::test_seed_focus_run(
                &e.user, &e.persona, &e.key, run, focus,
            )
            .await
            .unwrap();
        }
        // REAL: gate1 withholds both fields (the Omit policy), one row per attempt.
        async fn gate1_row(
            gw: &Gw,
            run: &str,
            action: &str,
            t: &PersonalTrack,
            sc: &HashSet<String>,
        ) {
            gw.gate1(
                action,
                run,
                t,
                2,
                1,
                2,
                ExternalAccess::AnonymousRequired,
                Some("prov-a".into()),
                Some(sc),
            )
            .await
            .unwrap();
        }
        // REAL: gate3 on its legacy path writes gate3_promotion_approved.
        async fn real_promotion_row(gw: &Gw, run: &str, action: &str) {
            let r = gw
                .gate3(
                    action,
                    run,
                    "Focus",
                    action,
                    "text",
                    1,
                    2,
                    ExternalAccess::AnonymousRequired,
                    2,
                    None,
                    Some(1),
                    false,
                    "",
                    "",
                    "",
                    KeptPrivate::RemovesSpans,
                    &[],
                    Some("prov-a"),
                )
                .await
                .unwrap();
            assert!(r.approved);
        }
        // EMULATED (see the doc comment): written through the real logger.
        async fn emulated(
            gw: &Gw,
            run: &str,
            action: &str,
            ev: &str,
            shared: &[&str],
            withheld: &[&str],
        ) {
            gw.logger
                .write(DisclosureLogEntry {
                    action_id: action.into(),
                    focus_run_id: run.into(),
                    execution_tier: 2,
                    abstraction_tier: None,
                    provider: Some("prov-a".into()),
                    fields_shared: shared.iter().map(|s| (*s).to_owned()).collect(),
                    fields_abstracted: Default::default(),
                    fields_withheld: withheld.iter().map(|s| (*s).to_owned()).collect(),
                    override_declined: false,
                    event_type: ev.into(),
                    category: None,
                    fact_key: None,
                })
                .await
                .unwrap();
        }
        async fn emulated_no_spans_row(gw: &Gw, run: &str, action: &str) {
            emulated(gw, run, action, "gate3_pf_no_spans", &[action], &[]).await;
        }
        async fn emulated_consent_pending_row(gw: &Gw, run: &str, action: &str) {
            emulated(gw, run, action, "gate3_consent_pending", &[], &[action]).await;
        }
        async fn emulated_unavailable_block_row(gw: &Gw, run: &str, action: &str) {
            emulated(
                gw,
                run,
                action,
                "gate3_pf_unavailable_block",
                &[],
                &[action],
            )
            .await;
        }

        // S1: a hosted step whose review approves on the legacy path.
        gate1_row(&gw, "r-s1", "a1", &track, &scope).await;
        real_promotion_row(&gw, "r-s1", "a1").await;
        // S2: the review finds no spans (the production path).
        gate1_row(&gw, "r-s2", "a2", &track, &scope).await;
        emulated_no_spans_row(&gw, "r-s2", "a2").await;
        // S3: spans asked, the user answers, the resume runs gate1 again.
        gate1_row(&gw, "r-s3", "a3", &track, &scope).await;
        emulated_consent_pending_row(&gw, "r-s3", "a3").await;
        gate1_row(&gw, "r-s3", "a3", &track, &scope).await;
        // S4: API Chat: the review's rows sit under a synthetic run, the send under the real one.
        emulated_no_spans_row(&gw, "api-chat-send-stub", "api-chat-send").await;
        gate1_row(&gw, "r-chat", "a4", &track, &scope).await;
        // S5: a hosted step with no review row.
        gate1_row(&gw, "r-s5", "a5", &track, &scope).await;
        // S6: gate1 runs, then the review blocks.
        gate1_row(&gw, "r-s6", "a6", &track, &scope).await;
        emulated_unavailable_block_row(&gw, "r-s6", "a6").await;

        let summary = |provider| {
            let (pool, user, key, persona) = (&e.pool, &e.user, &e.key, &e.persona);
            async move {
                summary_for_session(
                    pool,
                    user,
                    key,
                    persona,
                    SharedLogWindow::Everything,
                    0,
                    None,
                    provider,
                )
                .await
                .unwrap()
            }
        };
        let s = summary(None).await;
        // Five writing-assistant runs: S1 reviewed, S2 plain (no review step), S3 reviewed,
        // S5 plain, S6 held (0 sends). Four sends, two reviewed, two withheld fields each,
        // plus the one held item. One background check (the pending consent).
        let wa = s
            .groups
            .iter()
            .find(|g| g.focus.id == "writing-assistant")
            .unwrap();
        assert_eq!(
            (
                wa.runs,
                wa.sends,
                wa.all_reviewed,
                wa.kept_back,
                wa.background
            ),
            (5, 4, false, 9, 1)
        );
        // API Chat: one plain send in ONE group, no group for the review's synthetic run.
        assert_eq!(s.groups.len(), 2, "{:?}", s.groups);
        let chat = s.groups.iter().find(|g| g.focus.id == "api-chat").unwrap();
        assert_eq!(
            (chat.sends, chat.all_reviewed, chat.kept_back),
            (1, false, 2)
        );
        assert!(s.focuses.iter().all(|f| f.id != "api-chat-send"));
        // The provider filter keeps the sends, not the held item.
        let f = summary(Some("prov-a")).await;
        let wa = f
            .groups
            .iter()
            .find(|g| g.focus.id == "writing-assistant")
            .unwrap();
        assert_eq!((wa.sends, wa.kept_back), (4, 8));
        assert_eq!(
            f.groups
                .iter()
                .find(|g| g.focus.id == "api-chat")
                .unwrap()
                .sends,
            1
        );

        // The group's items fold the same way: one item per send, plus the held item and
        // the background row.
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let items = items_for_session(
            &e.pool,
            &e.user,
            &e.key,
            &e.persona,
            &day,
            "writing-assistant",
            0,
            None,
        )
        .await
        .unwrap();
        let mut keys: Vec<&str> = items.iter().map(|i| i.sentence.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bg.reviewRaised",
                "held.pfUnavailable",
                "sent.noReview",
                "sent.plain",
                "sent.reviewed",
                "sent.reviewed"
            ]
        );
        let chat_items = items_for_session(
            &e.pool, &e.user, &e.key, &e.persona, &day, "api-chat", 0, None,
        )
        .await
        .unwrap();
        assert_eq!(chat_items.len(), 1);
        assert_eq!(chat_items[0].sentence, "sent.plain");
    }
}
