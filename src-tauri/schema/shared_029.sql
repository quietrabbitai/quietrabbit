-- shared_029.sql
--
-- items.id=711 (decisions.id=875 point 5, PROVIDER_REGISTRY_AND_TIER_MODEL_
-- SPEC.md Part 8f; Chat-PM ruling on the plan's Q1 = A): a provider-level
-- request-options field.
--
-- WHY: request options live on model rows (provider_models.request_options).
-- A model the user picks from a refreshed live list has no curated row, so a
-- privacy option that must ride on every request to a provider (for example a
-- routing restriction to endpoints that do not collect data) would be missing
-- from it. This column holds those options once, on the provider. The
-- request builder (providers/hosted_call.rs, CallContext::build) merges them
-- OVER the model's request_options on every call; a model row that sets the
-- same key to a different value makes the call Misconfigured (fail closed).
--
-- DATA: the column is backfilled from the documentation_gate key
-- required_request_options that items.id=724 recorded on the rows that need
-- it. That key stays where it is (changing documentation_gate would change
-- the consent attributes hash and ask every user to consent again), so the
-- two must agree: migrations.rs test_required_request_options_ride_on_every_
-- model_row asserts it. No provider is named here: the backfill is driven by
-- the presence of the key.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN required_request_options TEXT
    CHECK (required_request_options IS NULL
        OR (json_valid(required_request_options)
            AND json_type(required_request_options) = 'object'));

UPDATE providers
SET required_request_options = json_extract(documentation_gate, '$.required_request_options')
WHERE json_valid(documentation_gate)
  AND json_type(documentation_gate, '$.required_request_options') = 'object';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (29, datetime('now'),
    'items.id=711: providers.required_request_options, a provider-level request-options field merged over every model row request_options (decisions.id=875), backfilled from documentation_gate required_request_options');
