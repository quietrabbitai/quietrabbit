-- messages_005.sql
--
-- items.id=501 slice 3 (decisions.id=846 addendum Q5): the per-reply Privacy
-- Guardian review and its message-bound status machine are retired, so the
-- two columns that backed it go. gate3_review_status (messages_001.sql)
-- carried the drafted/pending-review/approved/withheld lifecycle, and
-- reviewed_at_risk_rating (messages_003.sql) the destination risk a Tier-3
-- approval was scored against for the provider-selection re-check. Nothing
-- reads or writes either any more.
--
-- ALTER TABLE ... DROP COLUMN, not a table rebuild: both columns' CHECK
-- constraints are column-level constraints on the column itself, which
-- SQLite drops together with it (it refuses only when a CHECK on ANOTHER
-- column, an index, a key, a trigger or a view references the dropped one,
-- and none does here). No DROP TABLE, so no FK_OFF_MIGRATIONS entry is
-- needed, and the context_key index and every other column are untouched.
--
-- Values in the dropped columns are discarded for good, including any
-- 'withheld' marks (a past decision not to send a message).
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE messages DROP COLUMN gate3_review_status;
ALTER TABLE messages DROP COLUMN reviewed_at_risk_rating;

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (5, datetime('now'),
    'items.id=501 slice 3 (decisions.id=846): drop messages.gate3_review_status and messages.reviewed_at_risk_rating -- the per-reply review status machine is retired');
