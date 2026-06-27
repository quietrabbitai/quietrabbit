-- src-tauri/schema/outputs_007.sql
-- Migration 7: reserved for element_consent CHECK constraint extension (19b-outputs_007).
-- This migration is a placeholder. The real migration recreates consent_decisions
-- with an extended CHECK constraint to include 'element_consent' as a valid
-- decision_type. That work requires table recreation (SQLite cannot ALTER CHECK
-- constraints in place) and is scheduled as a dedicated pre-release session.
--
-- Executed within run_migrations SAVEPOINT -- atomic or rolled back.

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (7, datetime('now'),
    'Placeholder: element_consent CHECK constraint extension reserved (19b-outputs_007)');
