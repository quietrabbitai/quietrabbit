-- shared_027.sql
--
-- items.id=709 (decisions.id=875 point 1, Part 8b/8c): token-limit key on the
-- seeded hosted model rows. The generic hosted client FAILS CLOSED: a call
-- needs an output token limit from the call itself or the model row's
-- request_options, and with neither it returns a Misconfigured error before
-- any network traffic (items.id=710's cost cap needs a worst-case output
-- size, so there is no default in code). Every curated hosted model row
-- therefore carries a limit key; the row also names WHICH key (the chat
-- format accepts max_tokens everywhere, while reasoning models take
-- max_completion_tokens).
--
-- WHY A NEW MIGRATION, NOT AN EDIT OF shared_026.sql: shared_026 is a
-- numbered file already applied to existing shared.db files and committed
-- at c8b30c0. Data only -- no schema change.
--
-- VALUES:
--   First hosted default model (a reasoning model): max_completion_tokens
--   4096, no reasoning_effort in request_options (the provider default
--   effort applies), output_overhead 2.0. Basis is the items.id=709 live
--   check, two calls (2026-10-11), and it is SAMPLE-BASED, not a guarantee:
--   one default-effort sample showed an output-token overhead of about 1.8
--   against the visible answer, and a high-effort sample exhausted the whole
--   4096 budget. 2.0 is a rounded-up estimate from that one default-effort
--   sample, to be refined by per-user calibration (items.id=711). Reasoning
--   tokens consume the budget, so a high-effort setting can still hit the
--   limit and return the budget-exhausted error.
--   Second hosted default model (not a reasoning model): max_tokens 2048 is
--   a conservative value that is UNVERIFIED -- no live call was made for it.
--   items.id=724 refines it when it finalizes the rows.
--
-- MIGRATION NUMBERING: items.id=661 may also add a shared migration. Whichever
-- lands second renumbers.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

UPDATE provider_models
   SET request_options = '{"max_completion_tokens":4096}',
       output_overhead = 2.0
 WHERE id = 'groq:openai/gpt-oss-20b';

UPDATE provider_models
   SET request_options = '{"max_tokens":2048}'
 WHERE id = 'mistral:mistral-small-2603';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (27, datetime('now'),
    'items.id=709: the two seeded hosted default models carry an output token limit in request_options (the generic hosted client fails closed without one). Groq row: 4096 limit and output_overhead 2.0 from the items.id=709 live check (sample-based). Mistral row: 2048 is unverified, items.id=724 refines it');
