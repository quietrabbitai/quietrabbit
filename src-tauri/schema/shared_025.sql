-- shared_025.sql
--
-- items.id=607 (decisions.id=852, decisions.id=854): inputs for the Step 1
-- recommendation engine (recommendation.rs).
--
-- qr_recommendation_rank: nullable integer, lower = more preferred, NULL =
-- no stated order. Compared only within the same kind of candidate (local
-- models against local models, hosted against hosted) -- never across
-- kinds. Curator-owned like the rest of this table, release-time seeding
-- only. The engine reads this column and names no provider or model id.
--
-- min_vram_gb: merged into hardware_requirement beside min_ram_class. Sizing
-- is the Ollama library download size (Q4_K_M, read 2026-10-06: 2.0 GB,
-- 4.9 GB, 4.7 GB) plus an assumed 1 GB for KV cache and runtime overhead --
-- the overhead is an assumption, not a measurement.
--
-- The three local UPDATEs below name the seeded local ids because they are
-- one-off seed data, like shared_024.sql's inserts. The hosted UPDATE is
-- field-based and names no ids.
--
-- SCHEMA AUTHORING RULE (migrations.rs): no semicolons inside string
-- literals -- parse_statements() is not a general-purpose SQL parser.

ALTER TABLE providers ADD COLUMN qr_recommendation_rank INTEGER
    CHECK (qr_recommendation_rank IS NULL OR qr_recommendation_rank >= 0);

UPDATE providers
   SET hardware_requirement = json_set(hardware_requirement, '$.min_vram_gb', 6),
       qr_recommendation_rank = 1
 WHERE id = 'ollama:llama3.1:8b';

UPDATE providers
   SET hardware_requirement = json_set(hardware_requirement, '$.min_vram_gb', 6),
       qr_recommendation_rank = 2
 WHERE id = 'ollama:qwen2.5:7b';

UPDATE providers
   SET hardware_requirement = json_set(hardware_requirement, '$.min_vram_gb', 3),
       qr_recommendation_rank = 3
 WHERE id = 'ollama:llama3.2:3b';

UPDATE providers
   SET qr_recommendation_rank = 1
 WHERE qr_recommended = 1 AND provider_type = 'cloud_inference_api';

INSERT OR IGNORE INTO schema_version (version, applied_at, description)
VALUES (25, datetime('now'),
    'items.id=607: providers gains qr_recommendation_rank (nullable, lower = more preferred, compared within a kind) plus min_vram_gb in the local models hardware_requirement, seeded for the three curated local models and for qr_recommended cloud_inference_api rows');
