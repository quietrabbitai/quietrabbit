// src-tauri/src/commands/active_board.rs
//
// Group 5 — Active Board.
// Commands: get_active_board, get_topic_list, update_topic_state.
//
// get_active_board: returns topic cards for the given persona.
//   daily_brief and quick_launch are placeholder fields (None / empty) --
//   not yet implemented. Fields are present in the response struct now to
//   avoid a breaking IPC contract change when they are wired.
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

use serde::{Deserialize, Serialize};
use specta::Type;

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
    /// Placeholder -- Daily Brief not yet implemented. Will become a typed
    /// struct when wired; serialized as JSON string in the interim.
    pub daily_brief: Option<String>,
    /// Placeholder -- Quick Launch Dock not yet implemented.
    pub quick_launch: Vec<String>,
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
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_active_board(
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<ActiveBoardResponse, String> {
    let topics = topic_store::list_topics(&user_id, &persona_id, &key_hex, None, None)
        .await
        .map_err(|e| e.to_string())?;

    Ok(ActiveBoardResponse {
        topics: topics.into_iter().map(TopicInfo::from).collect(),
        daily_brief: None,
        quick_launch: vec![],
    })
}

#[tauri::command]
pub async fn get_topic_list(
    focus_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<Vec<TopicInfo>, String> {
    let topics =
        topic_store::list_topics(&user_id, &persona_id, &key_hex, Some(&focus_id), None)
            .await
            .map_err(|e| e.to_string())?;

    Ok(topics.into_iter().map(TopicInfo::from).collect())
}

#[tauri::command]
pub async fn update_topic_state(
    request: UpdateTopicStateRequest,
) -> Result<(), String> {
    const VALID_STATES: &[&str] = &[
        "Active", "Paused", "Waiting on you", "Complete", "Closed",
    ];
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
