// src-tauri/src/commands/shared_log.rs
//
// items.id=957 unit 2 (decisions.id=967, 434, 944): the IPC surface of the
// "What QR shared" view. READ ONLY (the purge is unit 3, a separate command).
//
//   get_shared_log_summary(persona_id, window, utc_offset_minutes, focus_id,
//                          provider_id)        one row per (day, Focus) group
//   get_shared_log_group_items(persona_id, day, focus_id, utc_offset_minutes,
//                          provider_id)        the items of ONE group, opened
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
    self as deck, classify_consent, classify_disclosure, counts_as_send, Classified, Tag,
    HISTORY_CLEARED,
};
use crate::persistence::provider_store::ProviderMode;
use crate::persistence::shared_log_read::{
    self as read, ConsentRowView, DisclosureAggregate, ProviderInfo, SharedLogRaw,
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
    /// Consent records a purge would remove (the cleared-on marker is not one).
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

/// The Focus of an aggregate: the run's Focus, or "" when focus_runs has no row.
fn focus_of<'a>(raw: &'a SharedLogRaw, a: &DisclosureAggregate) -> &'a str {
    raw.run_focus
        .get(&a.focus_run_id)
        .map(String::as_str)
        .unwrap_or("")
}

/// The (day, Focus) accumulators of the raw rows under the filters.
fn accumulate(raw: &SharedLogRaw, f: &Filters<'_>) -> BTreeMap<(String, String), Acc> {
    let mut map: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for a in &raw.aggregates {
        let focus = focus_of(raw, a);
        if f.focus_id.is_some_and(|want| want != focus) {
            continue;
        }
        let Classified::Item { tag, key } = classify_disclosure(&a.event_type, a.override_declined)
        else {
            continue;
        };
        // A provider filter keeps only Sent items to that provider.
        if let Some(want) = f.provider_id {
            if tag != Tag::Sent || a.provider.as_deref() != Some(want) {
                continue;
            }
        }
        let acc = map.entry((a.day.clone(), focus.to_owned())).or_default();
        if !a.focus_run_id.is_empty() {
            acc.runs.insert(a.focus_run_id.clone());
        }
        let n = clamp_u32(a.rows);
        match tag {
            Tag::Sent => {
                if counts_as_send(key) {
                    acc.sends += n;
                    if key == "sent.reviewed" {
                        acc.reviewed += n;
                    }
                    acc.kept_back += clamp_u32(a.withheld);
                    if let Some(p) = &a.provider {
                        acc.providers.insert(p.clone());
                    }
                }
            }
            Tag::KeptBack => acc.kept_back += n,
            Tag::Hidden => acc.background += n,
            Tag::Choice => {}
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
    let consent_record_count = raw
        .consent
        .iter()
        .filter(|c| c.event_type != HISTORY_CLEARED)
        .count();
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

/// The items of one (day, Focus) group, oldest first. `details` are the disclosure
/// rows of that Focus's runs on that day; `consent` are that day's consent rows.
pub(crate) fn build_items(
    details: &[read::DisclosureDetailRow],
    consent: &[ConsentRowView],
    focus_id: &str,
    lk: &Lookups,
    provider_id: Option<&str>,
) -> Vec<SharedLogItem> {
    let mut items: Vec<SharedLogItem> = Vec::new();
    for d in details {
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
            shared: deck::labels_for(&d.shared)
                .into_iter()
                .map(wire_label)
                .collect(),
            kept_back: deck::labels_for(&d.withheld)
                .into_iter()
                .map(wire_label)
                .collect(),
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
    let mut focuses: BTreeSet<String> = raw.run_focus.values().cloned().collect();
    focuses.extend(raw.consent.iter().map(|c| c.focus_slug.clone()));
    focuses.retain(|f| !f.is_empty());
    let providers: BTreeSet<String> = raw
        .aggregates
        .iter()
        .filter_map(|a| a.provider.clone())
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
        .filter(|a| a.day == day && focus_of(&raw, a) == focus_id)
        .map(|a| a.focus_run_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut personal =
        crate::persistence::personal_store::open_personal_db(user_id, persona_id, key_hex)
            .await
            .map_err(|e| e.to_string())?;
    let details = read::disclosure_details(&mut personal, &run_ids, day, utc_offset_minutes)
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
    Ok(build_items(&details, &consent, focus_id, &lk, provider_id))
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::migrations::parse_statements;
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
                agg("2026-11-20", "r1", "gate1_pass", Some("prov-a"), 2, 3, 1),
                agg(
                    "2026-11-20",
                    "r1",
                    "gate3_promotion_approved",
                    None,
                    1,
                    1,
                    0,
                ),
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
                agg("2026-11-20", "r2", "gate1_pass", Some("prov-b"), 1, 1, 0),
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
                agg("2026-09-02", "r5", "gate1_pass", Some("prov-a"), 1, 1, 0),
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
        // The cleared marker is not a purgeable record and is the permanent line.
        assert_eq!(s.consent_record_count, 1);
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

    #[test]
    fn all_reviewed_only_when_every_send_went_through_review() {
        let r = SharedLogRaw {
            aggregates: vec![agg(
                "2026-11-20",
                "r1",
                "gate3_promotion_approved",
                None,
                2,
                1,
                0,
            )],
            run_focus: HashMap::from([("r1".to_owned(), "writing-assistant".to_owned())]),
            ..Default::default()
        };
        let s = build_summary(&r, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        assert!(s.groups[0].all_reviewed && s.groups[0].sends == 2);
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
        let details = read::disclosure_details(&mut c, &["r1".into()], "2026-11-20", 0)
            .await
            .unwrap();
        let raw = SharedLogRaw {
            aggregates: aggs,
            consent: vec![],
            run_focus: HashMap::from([("r1".to_owned(), "writing-assistant".to_owned())]),
        };
        let summary = build_summary(&raw, &lookups(), SharedLogWindow::Last30, today(), &NONE);
        let items = build_items(&details, &[], "writing-assistant", &lookups(), None);
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
                created_at: "2026-11-20T10:00:00+00:00".into(),
                event_type: "gate3_sensitivity_block".into(),
                provider: None,
                override_declined: true,
                shared: vec![],
                withheld: vec!["draft".into()],
            },
            read::DisclosureDetailRow {
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
        let items = build_items(&det, &[ceiling], "writing-assistant", &lookups(), None);
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
        let events = [
            "gate1_pass",
            "gate3_promotion_approved",
            "gate3_sensitivity_block",
            "output_scan_light",
        ];
        let mut aggregates = Vec::new();
        let mut run_focus = HashMap::new();
        for i in 0..27_772usize {
            let day = format!("2026-11-{:02}", 1 + i % 22);
            let run = format!("run-{}", i / 5);
            run_focus.insert(
                run.clone(),
                ["writing-assistant", "quick-ask", "research-and-buy"][i % 3].to_owned(),
            );
            aggregates.push(agg(&day, &run, events[i % 4], Some("prov-a"), 1, 2, 1));
        }
        let raw = SharedLogRaw {
            aggregates,
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
        for name in ["get_shared_log_summary", "get_shared_log_group_items"] {
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
}
