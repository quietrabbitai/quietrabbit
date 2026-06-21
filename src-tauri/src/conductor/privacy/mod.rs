// src-tauri/src/conductor/privacy/mod.rs

pub mod abstraction;
pub mod errors;
pub mod gate1;
pub mod gate2;
pub mod gate3;
pub mod gate4;
pub mod logger;
pub mod types;

// PrivacyGateway<L: DisclosureLogger> -- mirrors Python's PrivacyGateway class.
// Gates are called as methods. Logger injected at construction.
// Use PrivacyGateway<NoopLogger> in production before persistence is ported.
// Use PrivacyGateway<TestLogger>  in golden-vector tests.
// Use PrivacyGateway<FailLogger>  to exercise disclosure-log failure paths.

use errors::DisclosureLogWriteError;
use logger::DisclosureLogger;
use types::{Gate1Result, Gate2Result, Gate3Result, Gate4Result, PersonalTrack};

pub struct PrivacyGateway<L: DisclosureLogger> {
    pub logger: L,
}

impl<L: DisclosureLogger> PrivacyGateway<L> {
    pub fn new(logger: L) -> Self {
        Self { logger }
    }

    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub async fn gate1(
        &self,
        step_id: &str,
        focus_run_id: &str,
        personal_track: &PersonalTrack,
        abstraction_tier: u8,
        raw_abstraction: u8,
        execution_tier: u8,
        provider: Option<String>,
    ) -> Result<Gate1Result, DisclosureLogWriteError> {
        gate1::gate1(
            &self.logger,
            step_id,
            focus_run_id,
            personal_track,
            abstraction_tier,
            raw_abstraction,
            execution_tier,
            provider,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub async fn gate2(
        &self,
        step_id: &str,
        focus_run_id: &str,
        response_content: &str,
        personal_track: &PersonalTrack,
        execution_tier: u8,
        provider: Option<String>,
        fields_shared: Option<&[String]>,
    ) -> Result<Gate2Result, DisclosureLogWriteError> {
        gate2::gate2(
            &self.logger,
            step_id,
            focus_run_id,
            response_content,
            personal_track,
            execution_tier,
            provider,
            fields_shared,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
    pub async fn gate3(
        &self,
        step_id: &str,
        focus_run_id: &str,
        content_key: &str,
        content_sensitivity_severity: u8,
        target_tier: u8,
        space_max_permitted_tier: u8,
        execution_tier: u8,
    ) -> Result<Gate3Result, DisclosureLogWriteError> {
        gate3::gate3(
            &self.logger,
            step_id,
            focus_run_id,
            content_key,
            content_sensitivity_severity,
            target_tier,
            space_max_permitted_tier,
            execution_tier,
        )
        .await
    }

    pub async fn gate4(
        &self,
        step_id: &str,
        focus_run_id: &str,
        content_sensitivity_severity: u8,
        execution_tier: u8,
    ) -> Result<Gate4Result, DisclosureLogWriteError> {
        gate4::gate4(
            &self.logger,
            step_id,
            focus_run_id,
            content_sensitivity_severity,
            execution_tier,
        )
        .await
    }
}
