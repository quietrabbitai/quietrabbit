-- persistence/schema/shared_005.sql
-- Phase C: seed focus_settings for role-assessment Focus (D6-303).
-- role-assessment is an Organized profile Focus:
--   context_flow:       bidirectional
--   library_visibility: persona_visible  (default seed for Job Hunting Persona)
--   privacy_tier:       2 (yellow)
--   max_permitted_tier: 2 (no Tier 3 in Release 1 Role Assessment)
--   focus_profile:      organized
--
-- Seeds for ALL existing personas (not LIMIT 1 — that pattern is only correct
-- for single-persona dev migration bootstrap in shared_004.sql). A new Focus
-- seeded with LIMIT 1 would leave all personas after the first without a
-- focus_settings row, causing a hard AUTHORIZE failure (D6-303).
-- INSERT OR IGNORE: safe on re-runs.
--
-- Note: focus_settings are Persona-agnostic in the Focus artifact.
-- This seed establishes organized profile as the default for all current
-- personas. Real users configure focus_settings during onboarding.

INSERT OR IGNORE INTO focus_settings
    (persona_id, focus_id, context_flow, library_visibility,
     privacy_tier, max_permitted_tier, focus_profile, voice_override,
     created_at, updated_at)
SELECT
    p.id, 'role-assessment', 'bidirectional', 'persona_visible', 2, 2, 'organized', NULL,
    datetime('now'), datetime('now')
FROM personas p;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'), 'Seed focus_settings for role-assessment Focus (organized profile, all personas)');
