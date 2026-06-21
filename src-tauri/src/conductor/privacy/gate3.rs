// src-tauri/src/conductor/privacy/gate3.rs
//
// PG_GATE_3: cross-tier content promotion guardian.
// Mirrors Python PrivacyGateway.gate3() exactly.
//
// Check ordering is load-bearing -- do NOT reorder:
//   1. Tier ceiling check fires FIRST  (target_tier > space_max_permitted_tier)
//   2. Sensitivity check fires SECOND  (severity >= 3 AND target_tier >= 2)
// Oracle-confirmed via gate3::ceiling_beats_sensitivity vector:
//   event_type = "gate3_tier_ceiling_block" when both conditions are true.

use indexmap::IndexMap;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    types::Gate3Result,
};

#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn gate3<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    content_key: &str,
    content_sensitivity_severity: u8,
    target_tier: u8,
    space_max_permitted_tier: u8,
    execution_tier: u8,
) -> Result<Gate3Result, DisclosureLogWriteError> {
    // Check 1: tier ceiling block (fires before sensitivity check).
    if target_tier > space_max_permitted_tier {
        logger
            .write(DisclosureLogEntry {
                step_id:           step_id.to_string(),
                focus_run_id:      focus_run_id.to_string(),
                execution_tier,
                abstraction_tier:  None,
                provider:          None,
                fields_shared:     vec![],
                fields_abstracted: IndexMap::new(),
                fields_withheld:   vec![content_key.to_string()],
                override_declined: true,
                event_type:        "gate3_tier_ceiling_block".to_string(),
            })
            .await?;

        return Ok(Gate3Result {
            approved: false,
            blocked:  true,
            plain_language: Some(
                "This content can't be shared with a higher-tier service \
                 from this Focus. [Change Focus settings] [Use local only]"
                    .to_string(),
            ),
        });
    }

    // Check 2: sensitivity block.
    if content_sensitivity_severity >= 3 && target_tier >= 2 {
        logger
            .write(DisclosureLogEntry {
                step_id:           step_id.to_string(),
                focus_run_id:      focus_run_id.to_string(),
                execution_tier,
                abstraction_tier:  None,
                provider:          None,
                fields_shared:     vec![],
                fields_abstracted: IndexMap::new(),
                fields_withheld:   vec![content_key.to_string()],
                override_declined: true,
                event_type:        "gate3_sensitivity_block".to_string(),
            })
            .await?;

        return Ok(Gate3Result {
            approved: false,
            blocked:  true,
            plain_language: Some(
                "This content contains medical or financial information \
                 and can't be shared with external services. \
                 [Use local only] [Get help]"
                    .to_string(),
            ),
        });
    }

    // Approved.
    logger
        .write(DisclosureLogEntry {
            step_id:           step_id.to_string(),
            focus_run_id:      focus_run_id.to_string(),
            execution_tier,
            abstraction_tier:  None,
            provider:          None,
            fields_shared:     vec![content_key.to_string()],
            fields_abstracted: IndexMap::new(),
            fields_withheld:   vec![],
            override_declined: false,
            event_type:        "gate3_promotion_approved".to_string(),
        })
        .await?;

    Ok(Gate3Result {
        approved:      true,
        blocked:       false,
        plain_language: None,
    })
}
