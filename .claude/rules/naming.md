---
paths:
  - "src-tauri/src/**"
  - "src-tauri/schema/**"
  - "app/core_artifacts/**"
---

## Naming Architecture — term map

Most legacy terms are retired; canonical terms are used in all new code.
Two rows below (Plan->Topic, Task->Action) are ADR-013-locked canonical
pairs; both are now propagated to code (Task->Action by items.id=552).
Check the Notes column for any remaining exception.

| Retired term         | Canonical term      | Notes                                        |
|----------------------|---------------------|----------------------------------------------|
| Space                | Persona             | space_id → life_id → persona_id              |
| space_id             | persona_id          | Column and parameter name                    |
| Life                 | Persona             | Intermediate migration term — fully retired  |
| life_id              | persona_id          | Migration complete (commit 8579f17)          |
| user_lives           | user_personas       | Join table in shared.db                      |
| lives/ (path)        | personas/ (path)    | Filesystem segment                           |
| life_affinity        | (removed)           | Dropped from FocusDefinition (D6-300)        |
| life_context         | persona_context     | SYSTEM_TOKENS in tokens.rs                   |
| life_store.py        | persona_store.py    | Migration complete                           |
| Path                 | Focus               | File extension: .focus                       |
| path_id              | focus_id            | Column and parameter name                    |
| path_run             | focus_run           | DB table: focus_runs                         |
| path_run_id          | focus_run_id        | Column and parameter name                    |
| path_run_snapshot    | focus_run_snap      | DB table: focus_run_snapshots                |
| path_context         | focus_context       | SYSTEM_TOKENS                                |
| paths/ dir           | focuses/ dir        | core_artifacts/focuses/                      |
| Specialist           | Guide / Operator    | .guide (community), .operator (sys)          |
| specialist_id        | source_id           | personal_fields, voice_profiles              |
| specialists/ dir     | guides/ + ops/      | core_artifacts/guides/ + operators/          |
| quick_draft          | quick_ask           | output_type and focus_id                     |
| Personal Specialist  | (no named term)     | User sees: "What QR knows about you"         |
|                      |                     | personal-specialist.operator display_name    |
|                      |                     | needs update before Focus builds             |
| Plan                 | Topic               | plan_id → topic_id; migration complete,      |
|                      |                     | topic_id/topic_store.rs used throughout      |
| Task                 | Action              | ADR-013-locked (D6-214--D6-225); migration   |
|                      |                     | complete (items.id=552): action_id and       |
|                      |                     | ActionDefinition used throughout code and    |
|                      |                     | schema. Retired: step_id, StepDefinition.    |
|                      |                     | Table focus_run_steps and reenter_step()     |
|                      |                     | keep their "step" names (not renamed).       |
