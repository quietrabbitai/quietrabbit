// src-tauri/src/commands/active_board.rs
//
// Group 5 — Active Board.
// Commands: get_active_board, get_topic_list, update_topic_state.
//
// get_active_board: returns topic cards for the given persona.
//   high_priority (IA_SPEC Section 2a/2b, decisions.id=712, items.id=236):
//   a subset of topics whose Focus type has declared a high_priority_trigger
//   in display_config and whose declared anchor date (read from the topic's
//   extra_metadata) has crossed that trigger's offset window. See
//   conductor::lifecycle::{HighPriorityTrigger, load_focus_definition}.
//   No .focus file in this repo declares a trigger yet -- Travel/Habit were
//   scoped as "currently-built" in items.id=236's original framing, but
//   neither exists (correction tracked separately by Jason, not this code).
//   daily_brief is a placeholder field (always None) -- not yet implemented,
//   out of scope for items.id=236. quick_launch (Quick Launch Dock) was
//   removed entirely -- retired feature, decisions.id=375->637->652.
// get_topic_list: returns topics for a focus.
// update_topic_state: updates topic lifecycle state.
//   Valid states per D6-220: "Active", "Paused", "Waiting on you",
//   "Complete", "Closed". Invalid values rejected at the IPC boundary.
//
// Ownership enforcement: update_topic_state passes user_id and persona_id to
//   the store, which opens the per-scope encrypted outputs.db for that
//   (user_id, persona_id) pair. A caller with wrong credentials opens a
//   different DB -- the topic_id will not exist there. Ownership is enforced
//   by the SQLCipher per-scope DB topology, not by a WHERE clause.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::conductor::lifecycle::{load_focus_definition, FocusDefinition};
use crate::persistence::topic_store::{self, Topic};

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct TopicInfo {
    pub id: String,
    pub focus_id: String,
    pub lifecycle_state: String,
    pub display_name: String,
    pub updated_at: String,
}

impl From<Topic> for TopicInfo {
    fn from(t: Topic) -> Self {
        let display_name = t.display_name().to_string();
        Self {
            id: t.id,
            focus_id: t.focus_id,
            lifecycle_state: t.lifecycle_state,
            display_name,
            updated_at: t.updated_at,
        }
    }
}

#[derive(Debug, Serialize, Type)]
pub struct ActiveBoardResponse {
    pub topics: Vec<TopicInfo>,
    /// IA_SPEC Section 2a/2b, decisions.id=712. Subset of `topics` whose
    /// Focus type declared a high_priority_trigger that has fired for that
    /// topic's anchor date. Additive, not exclusive of `topics` -- the
    /// frontend (not yet built) owns rendering/dedup between the bordered
    /// high-priority container and the full list.
    pub high_priority: Vec<TopicInfo>,
    /// Placeholder -- Daily Brief not yet implemented. Will become a typed
    /// struct when wired; serialized as JSON string in the interim.
    pub daily_brief: Option<String>,
}

#[derive(Debug, Deserialize, Type)]
pub struct UpdateTopicStateRequest {
    pub topic_id: String,
    pub user_id: String,
    pub persona_id: String,
    pub key_hex: String,
    pub state: String,
}

// ---------------------------------------------------------------------------
// High-priority trigger evaluation (decisions.id=712, items.id=236)
// ---------------------------------------------------------------------------

/// True when `topic`'s Focus type has declared a high_priority_trigger and
/// the topic's anchor date (read from extra_metadata) has crossed that
/// trigger's offset window. Missing/malformed anchor data, or a Focus type
/// declaring no trigger at all, fail closed (not high-priority) rather than
/// erroring -- a topic simply never qualifies.
fn topic_is_high_priority(topic: &Topic, focus_def: &FocusDefinition, now: DateTime<Utc>) -> bool {
    let Some(trigger) = &focus_def.high_priority_trigger else {
        return false;
    };
    let Some(anchor_str) = topic
        .extra_metadata
        .get(&trigger.anchor_field)
        .and_then(|v| v.as_str())
    else {
        return false;
    };
    let Ok(anchor) = DateTime::parse_from_rfc3339(anchor_str) else {
        return false;
    };
    trigger.is_active(anchor.with_timezone(&Utc), now)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn get_active_board(
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<ActiveBoardResponse, String> {
    let topics = topic_store::list_topics(&user_id, &persona_id, &key_hex, None, None)
        .await
        .map_err(|e| e.to_string())?;

    let now = Utc::now();
    let mut focus_def_cache: HashMap<String, Option<FocusDefinition>> = HashMap::new();
    let mut high_priority: Vec<TopicInfo> = Vec::new();

    for topic in &topics {
        let def = focus_def_cache
            .entry(topic.focus_id.clone())
            .or_insert_with(|| None);
        if def.is_none() {
            match load_focus_definition(&topic.focus_id).await {
                Ok(d) => *def = Some(d),
                Err(e) => {
                    log::warn!(
                        "get_active_board: could not load focus '{}' for high-priority check: {e}",
                        topic.focus_id
                    );
                    continue;
                }
            }
        }
        if let Some(d) = def {
            if topic_is_high_priority(topic, d, now) {
                high_priority.push(TopicInfo::from(topic.clone()));
            }
        }
    }

    Ok(ActiveBoardResponse {
        topics: topics.into_iter().map(TopicInfo::from).collect(),
        high_priority,
        daily_brief: None,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn get_topic_list(
    focus_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<Vec<TopicInfo>, String> {
    let topics = topic_store::list_topics(&user_id, &persona_id, &key_hex, Some(&focus_id), None)
        .await
        .map_err(|e| e.to_string())?;

    Ok(topics.into_iter().map(TopicInfo::from).collect())
}

#[tauri::command]
#[specta::specta]
pub async fn update_topic_state(request: UpdateTopicStateRequest) -> Result<(), String> {
    const VALID_STATES: &[&str] = &["Active", "Paused", "Waiting on you", "Complete", "Closed"];
    if !VALID_STATES.contains(&request.state.as_str()) {
        return Err(format!(
            "invalid lifecycle state: {}. Valid: Active, Paused, \
             Waiting on you, Complete, Closed",
            request.state
        ));
    }

    topic_store::update_topic_state(
        &request.user_id,
        &request.persona_id,
        &request.key_hex,
        &request.topic_id,
        &request.state,
        None, // dormant_since: not exposed in IPC v1
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::lifecycle::HighPriorityTrigger;
    use chrono::Duration;
    use serde_json::json;

    // Synthetic fixture modeled after Travel's departure-time anchor
    // (decisions.id=712) — no travel.focus file exists in this repo
    // (items.id=236 scope correction: Travel is not a built Focus type).
    fn travel_like_focus_def() -> FocusDefinition {
        FocusDefinition {
            focus_id: "travel-like-test-fixture".to_owned(),
            display_name: "Travel-like test fixture".to_owned(),
            description: String::new(),
            version: "1.0".to_owned(),
            max_routing_tier: 1,
            steps: vec![],
            output_type: "general".to_owned(),
            suggest_in_focuses: vec![],
            multi_source_validation: false,
            generic_title_template: "Hidden item".to_owned(),
            high_priority_trigger: Some(HighPriorityTrigger {
                anchor_field: "departure_time".to_owned(),
                offset: Duration::hours(-4),
            }),
        }
    }

    // Synthetic fixture modeled after Habit's due-date anchor
    // (decisions.id=712) — no habit.focus file exists in this repo
    // (items.id=236 scope correction: Habit is not a built Focus type).
    fn habit_like_focus_def() -> FocusDefinition {
        FocusDefinition {
            focus_id: "habit-like-test-fixture".to_owned(),
            display_name: "Habit-like test fixture".to_owned(),
            description: String::new(),
            version: "1.0".to_owned(),
            max_routing_tier: 1,
            steps: vec![],
            output_type: "general".to_owned(),
            suggest_in_focuses: vec![],
            multi_source_validation: false,
            generic_title_template: "Hidden item".to_owned(),
            high_priority_trigger: Some(HighPriorityTrigger {
                anchor_field: "due_date".to_owned(),
                offset: Duration::zero(),
            }),
        }
    }

    // No high_priority_trigger declared — matches every real .focus file
    // in this repo today (writing-assistant, quick-ask, research-and-buy,
    // role-assessment), none of which participate in the high-priority
    // section.
    fn no_trigger_focus_def() -> FocusDefinition {
        FocusDefinition {
            focus_id: "quick-ask".to_owned(),
            display_name: "Quick Ask".to_owned(),
            description: String::new(),
            version: "1.0".to_owned(),
            max_routing_tier: 1,
            steps: vec![],
            output_type: "general".to_owned(),
            suggest_in_focuses: vec![],
            multi_source_validation: false,
            generic_title_template: "Hidden item".to_owned(),
            high_priority_trigger: None,
        }
    }

    fn make_topic(focus_id: &str, extra_metadata: serde_json::Value) -> Topic {
        Topic {
            id: "topic-1".to_owned(),
            focus_id: focus_id.to_owned(),
            user_id: "user-1".to_owned(),
            persona_id: "persona-1".to_owned(),
            lifecycle_state: "Active".to_owned(),
            placeholder_name: "Untitled".to_owned(),
            created_at: "2026-08-10T00:00:00Z".to_owned(),
            updated_at: "2026-08-10T00:00:00Z".to_owned(),
            name: None,
            dormant_since: None,
            closed_at: None,
            extra_metadata,
        }
    }

    #[test]
    fn travel_like_topic_is_high_priority_departing_in_two_hours() {
        // IA_SPEC 2b's own example: "a trip departing in two hours does
        // [qualify]" -- falls inside the -4h pre-departure window.
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let departure = now + Duration::hours(2);
        let topic = make_topic(
            "travel-like-test-fixture",
            json!({ "departure_time": departure.to_rfc3339() }),
        );
        assert!(topic_is_high_priority(&topic, &travel_like_focus_def(), now));
    }

    #[test]
    fn travel_like_topic_is_not_high_priority_outside_lead_window() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let departure = now + Duration::hours(10);
        let topic = make_topic(
            "travel-like-test-fixture",
            json!({ "departure_time": departure.to_rfc3339() }),
        );
        assert!(!topic_is_high_priority(&topic, &travel_like_focus_def(), now));
    }

    #[test]
    fn habit_like_topic_is_high_priority_when_overdue() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let due = now - Duration::days(1);
        let topic = make_topic("habit-like-test-fixture", json!({ "due_date": due.to_rfc3339() }));
        assert!(topic_is_high_priority(&topic, &habit_like_focus_def(), now));
    }

    #[test]
    fn habit_like_topic_is_high_priority_exactly_at_due_instant() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let topic = make_topic("habit-like-test-fixture", json!({ "due_date": now.to_rfc3339() }));
        assert!(topic_is_high_priority(&topic, &habit_like_focus_def(), now));
    }

    #[test]
    fn habit_like_topic_is_not_high_priority_before_due() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let due = now + Duration::hours(1);
        let topic = make_topic("habit-like-test-fixture", json!({ "due_date": due.to_rfc3339() }));
        assert!(!topic_is_high_priority(&topic, &habit_like_focus_def(), now));
    }

    #[test]
    fn topic_missing_anchor_field_in_extra_metadata_is_not_high_priority() {
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let topic = make_topic("travel-like-test-fixture", json!({}));
        assert!(!topic_is_high_priority(&topic, &travel_like_focus_def(), now));
    }

    #[test]
    fn topic_whose_focus_declares_no_trigger_is_never_high_priority() {
        // Regression guard: today's four real .focus files are unaffected.
        let now = "2026-08-10T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let topic = make_topic(
            "quick-ask",
            json!({ "departure_time": (now - Duration::days(1)).to_rfc3339() }),
        );
        assert!(!topic_is_high_priority(&topic, &no_trigger_focus_def(), now));
    }
}
