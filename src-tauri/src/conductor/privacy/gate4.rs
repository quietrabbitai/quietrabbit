// src-tauri/src/conductor/privacy/gate4.rs
//
// PG_GATE_4: pre-Tier-3 boundary gate (stub implementation).
// Mirrors Python PrivacyGateway.gate4() exactly.
//
// CLIPBOARD_MAX_SENSITIVITY_SEVERITY = 2
// clipboard_blocked = content_sensitivity_severity > 2
// content_approved  = always true (stub gate -- full implementation deferred)
// Always writes disclosure log.

use indexmap::IndexMap;

use super::{
    errors::DisclosureLogWriteError,
    logger::{DisclosureLogEntry, DisclosureLogger},
    types::{Gate4Result, CLIPBOARD_MAX_SENSITIVITY_SEVERITY},
};

pub async fn gate4<L: DisclosureLogger>(
    logger: &L,
    step_id: &str,
    focus_run_id: &str,
    content_sensitivity_severity: u8,
    execution_tier: u8,
) -> Result<Gate4Result, DisclosureLogWriteError> {
    let clipboard_blocked =
        content_sensitivity_severity > CLIPBOARD_MAX_SENSITIVITY_SEVERITY;

    logger
        .write(DisclosureLogEntry {
            step_id:           step_id.to_string(),
            focus_run_id:      focus_run_id.to_string(),
            execution_tier,
            abstraction_tier:  None,
            provider:          Some("tier3_validation".to_string()),
            fields_shared:     vec![],
            fields_abstracted: IndexMap::new(),
            fields_withheld:   vec![],
            override_declined: false,
            event_type:        "gate4_stub_validation".to_string(),
        })
        .await?;

    let plain_language = if clipboard_blocked {
        Some(
            "This content contains sensitive information and must be \
             copied manually -- it can't be sent to your clipboard automatically."
                .to_string(),
        )
    } else {
        None
    };

    Ok(Gate4Result {
        content_approved:  true,
        clipboard_blocked,
        plain_language,
    })
}
