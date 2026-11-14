-- shared_044.sql
--
-- items.id=920 stage 1 (decisions.id=944, decisions.id=509): the columns the
-- collection-consent gate reads. Number checked against the live schema
-- directory on the build date: shared_043 was the latest. Numbers before the
-- items.id=633 cutover are provisional, since the cutover folds every prefix
-- into one baseline.
--
-- focus_settings.collection_consent: where this Persona stands on this Focus's
-- silent collection (not_asked, granted, declined, withdrawn). The default is
-- not_asked, so a row that was never asked never opens the gate. ONLY the
-- dedicated setter focus_settings_store::set_collection_consent writes it
-- (called by stage 2's command and by nothing else). The columns are NOT in
-- UpdateFocusSettingsRequest, and update_focus_settings does not touch them, so
-- neither the settings command nor submit_friction_gate_decision can move them.
--
-- focus_settings.collection_consent_version: the version of the consent text the
-- user agreed to (0 = none). The gate opens only while it equals the current
-- text version (focus_settings_store::COLLECTION_CONSENT_TEXT_VERSION), so a
-- changed text asks again.
--
-- focuses.collection_posture: whether the Focus needs the one setup consent.
-- per_field is the default and means the Focus collects through the per-field
-- gates and is never gated here. focus_level means the Focus collects silently
-- for its life after one informed consent (decisions.id=509): its store writers
-- refuse without it, and the generic extraction pass is skipped (that skip is
-- wired by items.id=914, not here). The default is NEVER "not required": whether
-- a Focus needs consent is declared by the Focus, so a missing or default row
-- can never open the gate.
--
-- CONTENT CLASS (decisions.id=862): the two focus_settings columns sit in a
-- KnownResidual table (shared_db_residual.rs) and show only a consent state per
-- Persona and Focus, no content. focuses.collection_posture is shipped Focus
-- catalog data (GlobalCatalog).
--
-- ALTER TABLE ... ADD COLUMN precedent: shared_024.sql, shared_039.sql,
-- shared_041.sql and shared_042.sql.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string literals or
-- comments -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE focus_settings ADD COLUMN collection_consent TEXT NOT NULL DEFAULT 'not_asked'
    CHECK (collection_consent IN ('not_asked', 'granted', 'declined', 'withdrawn'));

ALTER TABLE focus_settings ADD COLUMN collection_consent_version INTEGER NOT NULL DEFAULT 0;

ALTER TABLE focuses ADD COLUMN collection_posture TEXT NOT NULL DEFAULT 'per_field'
    CHECK (collection_posture IN ('per_field', 'focus_level'));

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (44, datetime('now'),
    'items.id=920 (decisions.id=944): focus_settings.collection_consent and collection_consent_version, focuses.collection_posture (the collection-consent gate)');
