---
paths:
  - "src-tauri/src/persistence/**"
  - "src-tauri/schema/**"
---

## Rust/Tauri Architecture

### SQLCipher + sqlx connection pattern
sqlx with SQLCipher-linked libsqlite3-sys. Not bundled vanilla SQLite.
PRAGMA key MUST be set before PRAGMA journal_mode on every connection.
Enforce via sqlx after_connect hook.
Connection topology: open single connections on demand per DB file.
Do not use a keyed pool — many small per-scope encrypted DBs. Reaffirmed
with full reasoning by items.id=475 (2026-09-11): PRAGMA key uses a raw
hex-blob literal, not a passphrase, so SQLCipher's KDF is bypassed and
re-keying per connection is already cheap — the usual argument for pooling
encrypted connections doesn't apply here. The real reason not to pool is
security-boundary lifetime: a pooled connection stays unlocked past
KeyRegistry's clear() at logout/idle-timeout, silently extending decrypted
access beyond the window that boundary exists to enforce. shared.db is the
one exception — genuinely unencrypted, pooled via items.id=483.
SQLCipher linkage: libsqlite3-sys sqlcipher feature (D6-346).

## Schema Authoring Rule
Do not use semicolons inside string literals in .sql files.
parse_statements() (migrations.rs) is not a general-purpose SQL parser.
A migration that drops a table another table still references via FOREIGN
KEY must be listed in FK_OFF_MIGRATIONS (migrations.rs) -- DROP TABLE
triggers ON DELETE CASCADE the same as an explicit DELETE would, not just
the latter; the every_drop_table_is_fk_off_or_audited test enforces this
(items.id=585/586).
