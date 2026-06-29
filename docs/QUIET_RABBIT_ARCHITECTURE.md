# Quiet Rabbit — Architecture Reference
# QUIET_RABBIT_ARCHITECTURE.md

**Engine:** Conductor
**Version:** 0.2
**Status:** Release 1 active development
**Last updated:** June 21, 2026 — Full Rust/Tauri rewrite complete (D6-339). All sections current: Section 1.7 tech stack, Sections 2, 7, 8, 9, 10 rewritten for Rust/Tauri. Python/Flask/Docker references retired. Release 1 scope locked (D6-359).
**Companion document:** QUIET_RABBIT_DESIGN.md

---

## DOCUMENT STATUS

| Section | Status | Notes |
|---|---|---|
| 1 — System Overview | validated | Terminology updated June 8 2026 |
| 2 — Deployment Topologies | validated | Rewritten for Tauri native app June 21 2026 (D6-339, D6-353) |
| 3 — Data Model Reference | validated | Terminology updated June 8 2026 |
| 4 — File Format Specifications | validated | File format names updated June 8 2026 |
| 5 — Taxonomy Files Reference | validated | |
| 6 — Conductor Execution Reference | validated | Terminology updated June 8 2026 |
| 7 — API Reference | validated | Rewritten for Tauri IPC surface June 21 2026 (D6-345, D6-350) |
| 8 — Auth and Multi-User Reference | validated | Rewritten for Rust June 21 2026; auth layer partially stubbed Release 1 |
| 9 — Development Environment Setup | validated | Rewritten for Rust/Tauri native dev workflow June 21 2026 |
| 10 — Phase 1 Build Sequence | validated | Rewritten June 21 2026; Release 1 scope locked (D6-359) |

This completes the Release 1 architecture specification.
All sections validated through multi-AI external review (ChatGPT + Gemini).
Final hardening pass applied May 29: threat model, session lifecycle,
recovery invariants, silent operator principle, EOF marker.
Terminology pass applied June 8: all hierarchy terms, file formats,
and SQLCipher package references updated. See D6-214 through D6-225.
Persona migration and terminology pass applied June 14, 2026.
Docker rebuild sequence updated (D6-308). See D6-289 through D6-303.
Chapter, coordination_record, Privacy Guardian carve-out gate model,
and Focus Builder block system added June 14, 2026. See D6-310 through D6-315.

## PROMPT INJECTION NOTICE

During Section 8 review, a third document arrived claiming to be "Lead
Systems Architect" review. Rejected. Referenced non-existent project
documents (IDEA_DECISIONS_3.md), proposed unevaluated providers (Cohere,
xAI Grok), contained direct instructions to Claude. Only ChatGPT and
Gemini are legitimate external review sources for this project.

## WORKFLOW NOTE

Standing rule: all sections go through ChatGPT and Gemini independent
review before final lock. Results evaluated against full project context
before corrections applied. This rule applies to all future technical
document production.

## TRANSITION NOTE

This NAS file is the Phase C deliverable. On Phase D start:
- Document moves to /docs/QUIET_RABBIT_ARCHITECTURE.md in the repo
- CLAUDE.md becomes its own file at the repo root (not embedded here)
- NAS file is reference only — repo version becomes the living document
- Section 10.2 (CLAUDE.md content) is reference only in this file

---

## 1. System Overview

Quiet Rabbit is a self-hosted personal AI platform. The Conductor is the
execution engine that orchestrates Focuses — structured task sequences run
through teams of Guides. Everything runs on the user's own hardware.
Nothing is sent externally without explicit user consent at every boundary.

### 1.1 Core Concepts

Persona — personalization boundary, context flow setting, privacy ceiling, active Guides, integration permissions.
  Two defaults: Personal and Work. User-expandable. Personas carry no privacy enforcement directly.
  Privacy is enforced by the three independent Focus settings.
Guide — role definition only; personal data lives in encrypted store.
Focus — curated Guide sequence + three independent settings (Persona assignment, context flow, privacy).
  Library setting: Shared / Persona-visible / Persona-hidden.
  Three profiles: Open (Bidirectional + Shared + 🟡), Organized (Bidirectional + Persona-visible + 🟡),
  Protected (Receive-only + Persona-hidden + 🔴).
Personal context — persistent context, three ownership scopes. User sees: "What QR knows about you."
Conductor — cohesive Python service, modular components.
Library — personal document store (outputs.db). Three content types: Output, Document, Collection.
Chapter — display-layer grouping of Focuses around a multi-Focus life pursuit (wedding, move,
  career change). Active Board card type only — not a hierarchy tier. Backed by coordination_record.
  Release 1: display-only; data-bearing Chapter deferred to future release. See D6-311.
coordination_record — Optimizer-managed internal record backing Chapter. Holds: member Focus
  references, per-Focus state snapshots, cross-Focus information references, Optimizer metadata.
  User-facing writes deferred to future release. Optimizer reads and writes in Release 1. See D6-310.

Note: life_id → persona_id migration complete (commit 8579f17, D6-289 through D6-303).
Schema, storage paths, and file formats updated throughout.

### 1.2 Three-Tier Model Routing

Tier 1: Local Ollama. Private, offline, always available.
Tier 2: User-configurable external API. Mistral (EU/GDPR, paid) or
        Groq (US, free tier). User choice at install. No prescribed default.
Tier 3: Validation providers (Claude, ChatGPT, Gemini). Always optional,
        always explicit, never automatic.

```python
# Hard security ceiling — cannot be exceeded under any circumstance
effective_tier = min(
    persona.max_permitted_tier,
    focus.max_routing_tier,
    step.routing_tier
)
# User preference — default, may be elevated with consent up to ceiling
preferred_tier = min(persona.life_privacy_default_tier, effective_tier)
```

**External call consent invariant:** No content crosses a tier boundary
without a corresponding disclosure_log entry and explicit user
acknowledgment at the appropriate Privacy Guardian gate. No silent
fallback to higher tiers. No automatic retry to cloud provider.

### 1.3 Guide Architecture

Three types of Guides serve distinct roles:

Type 1 — Personal context (always active, per Persona):
  Holds the user's personal context within a Persona. Never named.
  Users see: "What QR knows about you." Never declared in .focus files.
  Stores field-level encrypted data in personal.db.

Type 2 — Focus experts (Guides):
  Calibrated over time across all Focuses that use them.
  Contextual compounding happens here. Community-shareable as .guide files.
  Examples: Writing & Voice, Research & Analysis, Financial Reasoning,
  Nutritional Analysis, Code & Technical, Summarization.

Type 3 — System operators:
  Always active, serve every Focus in every Persona. Never targeted.
  System-managed .operator files — never shared.
  Users see individual names only — never the category "Operators."
  Names: Librarian, Privacy Guardian, Optimizer, Security Checker,
         Support Specialist, Focus Builder.

Sub-context splitting: first-class operation — Optimizer suggests split
when a sub-context grows significantly or quality feedback diverges.
Split triggers: different sensitivity levels, different ownership,
complexity exceeding single-Guide scope.

### 1.4 The Silent Operator Principle

When the Conductor injects personal context into a Focus step, the output
reflects that context without narrating it. The system uses personal data
to inform the answer — it does not reference the data source.

Correct: output naturally reflects user's location, tone, situation.
Wrong: "Since you mentioned you live in Avon..." or "Based on your
preference for direct communication..."

This applies to all Guide prompts and all output types. The personal
context is ambient, not cited. Violation of this principle is a
prompt engineering bug, not a user-facing feature.

### 1.5 Privacy Enforcement Model

Field-level sensitivity: general / personal / medical / financial.
HKDF name-based key derivation — new levels: append to yaml, zero migration.
Instance-scope restricted to general and personal sensitivity only.
Privacy Guardian invoked at four gates before any external call.
No telemetry. No usage data sent anywhere.

🔴 carve-out gate model (D6-312, D6-313): two-path routing, determined before surfacing anything.
Path A (generalized sufficient): Gate 1 only — user approves generalized version.
Path B (identity-attached required): Gate 2 directly — user sees both versions side by side,
  three options: Release generalized / Share data one time / Keep private.
User sees exactly one gate per invocation. All carve-out actions logged. No standing permissions.
Full vocabulary and button labels in DESIGN.md Privacy Guardian gate model section.

### 1.6 R1 Focuses and Personas

**Canonical R1 Focus list:** FOCUS_ROADMAP.md → § R1 Confirmed scope
13 Focuses confirmed across 4 R1 Personas (Personal, Work, Student, Medical).
Wellness is post-R1 pending wearable API and design session.
Do not duplicate the Focus list here — FOCUS_ROADMAP.md is the single source of truth.

R1 Persona summary:
- Personal (default): Writing Assistant, Travel, Cooking, Books, Movies & TV,
  Music, Learn a New Skill
- Work (default, intentionally minimal): Writing Assistant, Code Review,
  Research & Purchase, Travel (business mode)
- Student (optional): Class Focus spawnable template
- Medical (optional): Medical Writing Assistant, Medical Library

### 1.7 Technical Stack

**Backend:** Rust (async/Tokio) · Tauri v2 IPC layer (D6-339, D6-341)
**Database:** SQLite + SQLCipher via sqlx + libsqlite3-sys sqlcipher feature (D6-346)
  PRAGMA key must precede PRAGMA journal_mode on every connection (non-negotiable)
  Per-scope encrypted databases: one per persona, one per focus, one per topic
**Conductor:** Custom Rust orchestration engine — no CrewAI or litellm
  Actor model for FocusRun track ownership — no Arc<Mutex> (D6-342)
  Single Tokio actor with resumable step loop, explicit current_step: usize (D6-347)
**IPC:** tauri-specta 2.0.0-rc.25 — TypeScript types generated from Rust structs (D6-345, D6-350)
  33 typed IPC commands + 4 push events — see HANDOFF_IPC_SURFACE.md
**Auth:** PBKDF2 600k iterations · HKDF · BIP39 recovery (Layer 8, partially stubbed Release 1)
  Master key never persisted — held in Tauri managed state (post-Release 1: tauri::State)
**Ollama:** detect-first, bundle-as-fallback (D6-353)
  Models: llama3.2:3b / llama3.1:8b / qwen2.5:7b
**Frontend:** Static SPA (framework TBD) — communicates via Tauri IPC only
**License:** BSL 1.1

Python backend fully retired June 21, 2026 (commit a27a2b1, D6-339 fulfilled).

---

## 2. Deployment Topologies

QR ships as a Tauri native desktop application. No Docker. No container.
All data stored locally on the user's machine.

### 2.1 Runtime Configuration

Key runtime values — set via Tauri managed state or .env at app startup:

```
QR_DATA_ROOT        — root for all user data (default: ~/.quietrabbit/)
QR_ENV              — development | production (NEVER development in production)
QR_NETWORK_STORAGE  — true if data root is a network mount (disables WAL mode)
QR_ALLOW_HTTP       — LAN only; set false when HTTPS introduced
```

Optional tuning (set in app config, not env):
```
QR_LOOP_DETECTION_THRESHOLD=3
QR_CONTEXT_WARNING_THRESHOLD=0.75
QR_QUALITY_FLOOR=0.55
QR_INTERRUPT_THRESHOLD_MINUTES=5
QR_MAX_CONCURRENT_FOCUSES=3
QR_CHECKPOINT_EVERY_N_STEPS=3
```

### 2.2 Topologies

A: Single machine — default. No configuration needed.
   Ollama: detect-first at 127.0.0.1:11434, bundle-as-fallback (D6-353).
B: Separate inference machine — set OLLAMA_HOST to remote IP.
C: Separate storage — QR_NETWORK_STORAGE=true for network mounts.
   WAL mode unreliable on NFS/SMB (locking semantics vary) — rollback journal used.
D: Laptop + Tailscale remote access — set OLLAMA_HOST to Tailscale IP.

### 2.3 Data Root Structure

```
{QR_DATA_ROOT}/
├── instance/shared.db
├── users/{user_id}/personas/{persona_id}/personal.db + outputs.db
├── users/{user_id}/integration_keys.db
├── models/scores.db
├── cache/last_known_good/ · config/ · sessions/ · linked/
└── community_artifacts/focuses/ guides/ operators/ integrations/
```

### 2.4 Startup Sequence

Steps 1-5 halt on failure. Steps 6-9 degrade. 11 steps total.
Ollama detection runs at step 6 — detect system Ollama first,
start bundled sidecar if not found (D6-353).
Headless boot (multi-user): instance databases accessible, per-user
databases locked until login, queued notifications at first login.

### 2.5 Interaction Design Constraints

Plain Language Rule: non-technical, action-focused, one clear action,
reassure-then-guide.
Severity: INFO (notification center) / SUGGEST (contextual) /
          REQUIRE (blocking) / STOP (hard block).
Passive acquisition order before any prompt:
  current run context → personal context fields → Persona context →
  Library outputs → user uploads → explicit prompt (last resort).

---

## 3. Data Model Reference

### 3.1 Overview

sqlcipher3-binary throughout (from sqlcipher3 import dbapi2 as sqlite3).
Additive schema evolution. migration_lock in every database.
WAL locally, rollback journal on network storage.

Field encryption: HKDF(master_key, info=f"qr-field-key-{label}").

### 3.2 instance/shared.db

personas — canonical source of truth. max_permitted_tier (hard ceiling)
  and life_privacy_default_tier (preference) are distinct.
users — no password_hash on User object; returned separately in
  credential lookup.
user_salts, user_personas — ON DELETE CASCADE.
instance_context — CHECK(sensitivity IN ('general','personal')) only.
  Medical and financial never instance-scoped.
context_groups, context_group_members — Release 1 schema, Release 2 UX.
artifact_versions — in shared.db (not per-Persona). All file types tracked.

### 3.3 personal.db

personal_fields — sensitivity_severity GENERATED ALWAYS (1-4, unknown=99).
personal_field_groups — local FK enforcement. group_id is cross-db
  reference resolved at application layer.
voice_profiles — precedence: model baseline → guide defaults →
  global → Persona → writing context overrides.
disclosure_log — NEVER deleted. override_declined + declined_at columns.
staleness_check_state — UNIQUE(user_id, persona_id), one row per Persona.

### 3.4 outputs.db

outputs — sensitivity_severity GENERATED. Purge tracking columns.
  Deletion sequence: zero content → COALESCE FTS5 update → set deleted.
focus_runs — status includes 'initializing'. Created as initializing,
  promoted to running after Phase 3 success only.
focus_run_snapshots — PersonalTrack never serialized. Two-phase commit.
  PersonalContextManifest in metadata. Snapshot retention:
    paused/awaiting_user: preserve
    awaiting_feedback: purge after Phase 5
    cancelled/complete: purge immediately

### 3.5 integration_keys.db

UNIQUE(user_id, provider, key_type, integration_id).

### 3.6 models/scores.db

effective_score GENERATED ALWAYS (seeded_score × hardware_factor).

### 3.7 Invariants

Privacy: no external call above permitted threshold without PG gate.
External call: no tier boundary crossing without disclosure_log entry.
Deletion: zero content before setting deleted status.
Migration: migration_lock prevents concurrent migrations.
Audit: disclosure_log never deleted.
Signal: invalid runs contribute nothing to quality scores.

---

## 4. File Format Specifications

YAML → schema validation → internal JSON → Security Checker → activate.
Declarative only. No executable logic.
/app/core_artifacts/focuses/ guides/ operators/ (immutable) vs
{QR_DATA_ROOT}/community_artifacts/focuses/ guides/ operators/ integrations/.
Prompt template: {variable} for injection, {{ }} for literal braces.
output_var required on every step producing output consumed downstream.
Type 3 operators never declared in .focus files — always active.

artifact_versions in shared.db (not per-Persona).
life_affinity: [legal-finance] (matches actual persona id, not 'financial').
Security Checker: 5a whitelist → 5b structural (DAG validation) →
  5c semantic (provider registry) → 5d pattern heuristics.
Validation pipeline: 8 steps, atomic activation.
Multi-source validation: max 2 providers, SYNTHESIS_CONTENT_LIMIT=2000
  (synthesis summary only — full content always in outputs.db).

### File format summary

| Format | Purpose | Sharing |
|---|---|---|
| .persona | Persona definition — YAML | Per-install only, never shared |
| .focus | Focus definition — YAML, human-readable | Community-shareable |
| .guide | Type 2 Guide (Focus expert) — YAML | Community-shareable template |
| .operator | Type 3 system operator — YAML | System-managed, never shared |
| .integration | Integration definition — YAML, versioned in repo | Re-disclosure on change |

---

## 5. Taxonomy Files Reference

Five files in memory at startup. SHA-256 manifest verification.
Bundled files hash-verified; user config schema-validated only.
validation_mode: development=warn, production=fail_fast.

Key decisions:
- long_context threshold 0.75: surface Tier 2 option, never auto-route
- not_applicable: registered fourth retroactive_extraction enum value
- life_affinity [legal-finance] for financial output types
- Mistral + Groq both first-class with honest trade-off framing
- url_overflow: clipboard_and_base_url for all Tier 3 providers
- Explicit per-task-type fallback chains in routing_table
- Optimizer thresholds defined in routing_table.yaml

Travel & Vacation output types (add to output_types.yaml):
  itinerary, attraction_research, packing_list, trip_summary
  life_affinity: [nature-travel], sensitivity: general

---

## 6. Conductor Execution Reference

### 6.1 Seven-Phase Lifecycle

Phases 1-5 and 7 mandatory. Phase 6 async and optional.

Phase 1 LOAD:       load .focus, validate, check artifact_versions
Phase 2 AUTHORIZE:  tier checks, create focus_run status=initializing
Phase 3 INITIALIZE: open personal.db, assemble tracks, promote to running
Phase 4 EXECUTE:    step loop (Tier 3 steps are terminal boundaries)
Phase 5 OUTPUT:     write outputs.db, purge snapshots, offer validation
Phase 6 FEEDBACK:   paste-back, diff, quality signals (async, optional)
Phase 7 CLEANUP:    release connections, clear tracks, enforce retention

### 6.2 Three Context Tracks

PersonalTrack: read-only, from personal.db. NEVER serialized. Re-fetched
  fresh on resume. PersonalContextManifest in checkpoint metadata.
TaskTrack: accumulates step outputs. sensitivity_ceiling GENERATED from
  max severity across contributing fields.
SharedStateTrack: content approved through PG_GATE_3 only.
  step_disclosure_buffers: step_id → {field_name: abstracted_value}.
  NOT safe to cross tier boundaries until PG_GATE_3 approves.

SYSTEM_TOKENS (frozenset in tokens.py):
  {user_input, persona_context, voice_profile, previous_output, focus_context}

### 6.3 Step Execution — 15-Step Sequence

1. Load StepDefinition
2. Determine routing tiers (effective_tier + preferred_tier)
3. Tier gate check — STOP if exceeds max_permitted_tier
4. Tier 3 boundary check — terminal: checkpoint, awaiting_user, break loop
5. Context window check
6. Assemble fields — PG_GATE_1 writes to step_disclosure_buffer
7. Inbound classification setup
8. Assemble prompt — Tier 2+: read from step_disclosure_buffer ONLY
9. Apply parameter overlay
10. Execute via StepExecutor adapter (no bare generate calls)
11. Inbound classification — PG_GATE_2 if flagged
12. Update TaskTrack (update_sensitivity_ceiling)
13. Cross-tier promotion — PG_GATE_3 if tier increases
14. Write checkpoint (configurable policy)
15. Log disclosure, advance

### 6.4 Privacy Guardian — Four Gates

All Tier 1 local. Pre-generated templates at LOAD phase.
PG_GATE_1: disclosure → writes step_disclosure_buffer (REQUIRE)
PG_GATE_2: inbound response evaluation (REQUIRE if flagged)
PG_GATE_3: cross-tier content promotion (REQUIRE if sensitive)
PG_GATE_4: validation content preparation (REQUIRE before Tier 3)

### 6.5 Failure Modes F1-F10

F1: Ollama unavailable → retry, offer Tier 2, STOP if Tier 1 only
F2: Quality below floor → fast model, offer Tier 2 if confidence ≥ 0.55
F3: Context window → warn 0.75 / hard 0.95, user chooses explicitly
F4: PG hard block → STOP, offer alternatives
F5: Security Checker flag → STOP, no retry
F6: Inbound contamination → hold, PG_GATE_2, await decision
F7: personal.db unavailable → STOP immediately
F8: Snapshot write failure → memory-only mode, suspend checkpointing
F9: Loop detection → normalized semantic hash, STOP
F10: Provider error → per HTTP status (401/429/5xx/timeout)

Confidence < 0.55 REQUIRE supersedes F2 never-block.

### 6.6 Context Compaction

ContextCompactor: direct local model call. No Guide routing.
No PG gates. No Focus orchestration. Avoids Conductor-Librarian
circular dependency. routing_table passed explicitly.

### 6.7 Confidence Framework

0.90-1.0: proceed silently
0.75-0.89: proceed + internal log
0.55-0.74: SUGGEST
< 0.55: REQUIRE (supersedes F2)
Weights: data 20%, model 30%, routing 35%, output 15%.
Starting points — Phase 2 harness data will inform tuning.

### 6.8 Resume and Cancel

Resume: load snapshot → verify SHA-256 → decrypt → re-fetch PersonalTrack
fresh → check PersonalContextManifest → re-authorize → resume.
Expired snapshot: fall back to last committed checkpoint or fail clean.
Cancel: checked at every step boundary. Phase 4 cancel purges snapshots
and does NOT write partial output.

### 6.9 Optimizer

Signals written passively. signal_validity: valid/partial/invalid.
Three notification types:
  suggest_model_swap: 10 valid runs, 15% quality gap
  suggest_tier_upgrade: 5 runs, 30% floor breach rate
  compaction_applied: 5 runs, 50% compaction rate

### 6.10 Resource Arbitration

MAX_CONCURRENT_INFERENCE=1. MAX_CONCURRENT_FOCUSES from env (default 3).
Interactive preempts background at step boundaries.
GPU memory: graceful downgrade with plain-language offer to user.

---

## 7. API Reference

The Rust/Tauri backend exposes 33 typed IPC commands and 4 push events.
This replaces the Python Flask route layer retired June 21, 2026.
Full IPC surface definition: HANDOFF_IPC_SURFACE.md

### 7.1 Core Types

GenerateRequest: stream: Option<bool> — None resolved by OllamaClient.
GenerateResponse: completion_status replaces done field.
ChatMessage: role — "system" | "user" | "assistant".
ProviderHealth: available_models: Vec<String>.
ContextWindowStatus: status "ok" | "warn" | "exceeded".
  recommended_action: "compact_then_escalate".

### 7.2 Ollama Client

Health check: GET /api/tags, 5s timeout, never raises.
generate(): overlay applied, latency logged. stream=None (false) Release 1.
chat(): latency tracked — NOT hardcoded 0.
apply_modelfile(): validates NDJSON response body for success status.
  Logs to diagnostic view (not silent — Modelfile changes inference).
estimate_token_count(): heuristic ~4 chars/token. May underestimate
for code, JSON, non-English. Over-estimation is safe.
Release 2 streaming: Tier 1 progressive, Tier 2 buffer-and-release
  (intentional — PG_GATE_2 needs complete response before display).

OllamaSource enum: SystemOllama | Sidecar | Unavailable.
Detection at startup: GET /api/tags on 127.0.0.1:11434, 2s timeout.
If not found: start bundled sidecar (D6-353). get_health() reports source.

### 7.3 Tier 2 Provider Interface

Tier2Provider trait: generate(), health_check(), estimate_cost().
API keys from integration_keys.db only. Never from env.
GroqProvider: stateless, instantiated at call sites (D6-349).
CLIPBOARD_MAX_SENSITIVITY_SEVERITY=2 (personal and below).
Medical/financial: manual copy UI, never system clipboard.

### 7.4 Validation Provider Interface

ValidationLinkGenerator: clipboard_blocked flag for sensitivity gate.
ValidationReturnHandler: runs as Phase 6 FEEDBACK (not Phase 4).
  summarize_diff uses task_type="summarization" (not structured_output).
MultiSourceSynthesis: synthesis_truncated flag. Full content in outputs.db.

### 7.5 Privacy Guardian Hooks

Four typed request/response pairs. ValidationContentResponse includes
content_sensitivity_severity for clipboard safety decision.
All gate implementations in src-tauri/src/conductor/privacy/.
Golden vector test suite: 201 vectors, 0 failures (commit 267cdc9).

### 7.6 Error Taxonomy

All errors extend ConductorError (thiserror enum). Mapped to F1-F10.
Provider errors: ProviderError enum in providers/errors.rs.
From<ProviderError> for ConductorError implemented.

### 7.7 Evaluation Harness Level 1

Score = (latency_score × 0.40) + (format_compliance × 0.60).
Results written to model_hardware_scores.

### 7.8 Progress Indicator

Release 1: IPC push event run_status_update fires at each step boundary.
Fields: status, step_display_name, step_index, step_total.
Frontend polls or listens via tauri event subscription.
Release 2: no change needed — push events already replace polling.

### 7.9 IPC Push Events

run_status_update — fires at each step boundary (Phase 4 EXECUTE).
consent_request — fires at PG_GATE_3 cross-tier promotion.
floor_consent_request — fires when floor clamping detected.
notification_available — fires when Optimizer threshold crossed.

All push events emit from within FocusRun actor via AppHandle (D6-351).
Frontend must detach Tauri event listeners on SPA view unmount.

---

## 8. Auth and Multi-User Reference

The auth layer is Layer 8 — partially stubbed for Release 1.
Key derivation math (PBKDF2, HKDF, BIP39), threat model, and session
lifecycle design are unchanged from the original specification.
Rust implementation in progress. Python/Flask auth stack retired June 21 2026.

### 8.1 Overview

Single-user (default, auth_enabled=0): install keychain key, no login.
Multi-user (opt-in, auth_enabled=1): PBKDF2 password-derived keys.
auth_enabled=1 ONLY if ALL databases migrate successfully.
Partial failure: rollback migrated databases, keep auth disabled, retry.

### 8.2 Key Architecture

Key derivation (Rust — implementation pending Layer 8):
```rust
// PBKDF2 — 600,000 iterations
fn derive_user_master_key(password: &str, salt: &[u8]) -> Vec<u8> {
    pbkdf2_hmac_sha256(password.as_bytes(), salt, 600_000, 32)
}

// HKDF for field and snapshot keys
fn derive_field_key(master_key: &[u8], sensitivity_label: &str) -> Vec<u8> {
    hkdf_derive(master_key, format!("qr-field-key-{}", sensitivity_label).as_bytes())
}

fn derive_snapshot_key(master_key: &[u8], focus_run_id: &str) -> Vec<u8> {
    hkdf_derive(master_key, format!("qr-snapshot-{}", focus_run_id).as_bytes())
}
```

**Key management — Release 1 (partial stub):**
Master key held in Tauri managed state (tauri::State<Mutex<Option<Vec<u8>>>>).
Never persisted to disk. Process restart clears the key.
Full key registry and session management: Layer 8 post-Release 1.

**Install keychain (single-user):** System keychain via platform APIs.
Supported: Secret Service (Linux), macOS Keychain, Windows Credential Manager.
Insecure plaintext backends rejected at startup.

### 8.3 Session Lifecycle

```
Idle timeout:          30 minutes (configurable)
Absolute lifetime:     30 days
Concurrent sessions:   permitted
Key eviction triggers:
  - Explicit logout: clear key from managed state
  - Password change: re-derive, re-encrypt all databases
  - Process termination: volatile managed state cleared automatically

Single-user mode:
  Master key retrieved from keychain on each access attempt.
  No timeout — keychain is always available if unlocked.
```

### 8.4 Login and Logout

Auth commands (IPC Group 11): login(password), logout(), get_recovery_key_display().
login(): PBKDF2 derives key → store in tauri::State → session established.
logout(): clear key from managed state.
Auto-login (single-user): keychain key loaded at startup.

### 8.5 Recovery Key — Formal Specification and Invariants

**Implementation — Option B (mnemonic IS the master key):**

```rust
// 32 bytes → 256-bit entropy → 24-word BIP39 mnemonic
fn generate_recovery_key(master_key: &[u8]) -> String {
    bip39::Mnemonic::from_entropy(master_key).unwrap().to_string()
    // Shown ONCE at account creation. Never stored.
}

fn recover_with_key(recovery_mnemonic: &str, new_password: &str, user_id: &str) {
    let mnemonic = bip39::Mnemonic::parse(recovery_mnemonic).unwrap();
    let original_master_key = mnemonic.to_entropy();
    // Exact original 32 bytes. No wrapping. No random suffix.
    // Re-encrypts all databases with new password-derived key.
}
```

**Recovery key invariants — non-negotiable:**

```
QR never stores the recovery mnemonic.
QR never stores entropy or any material that could reconstruct the mnemonic.
QR never transmits the mnemonic or entropy to any external service.
No cloud escrow. No server-side backup. Escrowless zero-knowledge recovery.

Display requirements:
  Mnemonic shown in a dedicated screen. No other UI elements compete.
  User must tap "I've saved it somewhere safe" to continue.
  Skip option must state consequence explicitly: "If you lose both your
  password and this key, your data cannot be recovered."

If both password and recovery key are lost:
  Data is cryptographically unrecoverable. By design.
```

### 8.6 Database Re-encryption

SQLCipher PRAGMA rekey + reopen verification (Rust):
```rust
// Step 1: apply rekey
sqlx::query(&format!("PRAGMA rekey = \"x'{}'\"" , new_key_hex))
    .execute(&mut conn).await?;
// Step 2: reopen with new key to verify
let mut conn2 = connect_encrypted(&db_path, new_key_hex).await?;
sqlx::query("SELECT count(*) FROM sqlite_master")
    .execute(&mut conn2).await?;
```

**Migration state machine:**
pending → in_progress → verifying → committed | rolled_back | failed

All databases must reach committed before auth_enabled=1.
Any rolled_back or failed: overall migration failed, auth_enabled stays 0.

### 8.7 Auth Session Tables

auth_sessions, auth_failures, auth_lockouts in shared.db.
Release 1: schema present, NOT enforced.
auth_lockout_enabled flag independent of role_enforcement flag.

### 8.8 Threat Model

What Quiet Rabbit protects against:

```
Local disk theft or physical access:
  All per-user databases encrypted with SQLCipher. Without the master
  key, data is inaccessible.

Offline database extraction:
  Same protection as disk theft. Field-level encryption adds second layer.

Unauthorized local account access (multi-user mode):
  Per-user database isolation. Master key never on disk.

Network interception of external calls:
  Tier 2/3 calls over HTTPS. Content Privacy Guardian approved before
  transmission. Clipboard sensitivity-gated before leaving local control.
```

What Quiet Rabbit does NOT protect against:

```
Compromised running host — attacker with shell access can read
  managed state. Encrypted databases open while session active.

Root-level malware or kernel compromise — OS-level access can
  read process memory including managed state keys.

Active surveillance by the machine's owner — if the machine is
  hostile, there is no protection. QR is for owner-operated hardware.

Social engineering — user tricked into sharing password or recovery
  key loses all protection.
```

**Design scope:** Personal self-hosted use case — trusted machine, private network.
Protect data at rest from physical access, data in transit from interception.

### 8.9 Tier 2 Provider Choice — Install Interview

No provider prescribed. Install interview presents both with honest
trade-off framing. Stored in users.tier2_provider_preference.

```
Mistral (Europe): GDPR-native, paid, privacy-first users.
Groq (US):        Free tier, US jurisdiction, cost-first users.
Decide later:     Local only until configured.
```

---

## 9. Development Environment Setup

QR is a Tauri native desktop application. No Docker. No LXC. No container.
See CLAUDE.md at the repo root for the current dev workflow.

### 9.1 Current Development Environment

```
Garuda Linux desktop (Ryzen 5 7600X3D / RX 6600 8GB)
  LAN: 192.168.88.26 (eth) · 192.168.88.81 (wifi)
  Tailscale: 100.113.187.192
  Ollama: http://192.168.88.26:11434
  Workspace: /home/kulaga/QuietRabbit/
  Repo: /home/kulaga/QuietRabbit/06_GitRepos/quietrabbit-core/ (main branch)
```

### 9.2 Rust Toolchain

```bash
# Install rustup + stable toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable

# Install Tauri CLI
cargo install tauri-cli

# Build and test
cd /home/kulaga/QuietRabbit/06_GitRepos/quietrabbit-core/src-tauri
cargo build
cargo test
```

### 9.3 SQLCipher

libsqlcipher.so confirmed at /usr/lib/ on Garuda.
Linkage: libsqlite3-sys = { features = ["sqlcipher"] } in Cargo.toml.
PRAGMA key MUST precede PRAGMA journal_mode on every connection (D6-346).
Wrong-key error: SQLITE_NOTADB (code 26).

### 9.4 Ollama

```bash
ollama pull llama3.1:8b
ollama pull llama3.2:3b
ollama pull qwen2.5:7b
curl http://192.168.88.26:11434/api/tags
```

QR detects Ollama at 127.0.0.1:11434 on startup. If not found,
starts bundled sidecar (D6-353). Dev: Ollama running natively on Garuda.

### 9.5 Observability

QR_ENV=development: full prompt logging, personal field value logging.
NEVER run QR_ENV=development in production — logs decrypted personal
field values, full assembled prompts, routing decisions, snapshot state.

### 9.6 Backup

Back up: users/, instance/, integration_keys.db, config/.
Exclude: sessions/ (ephemeral), models/scores.db (auto-rebuilds).

---

## 10. Phase 1 Build Sequence

The Rust/Tauri migration is complete (D6-339, June 21 2026, commit 40ab2a3 on main).
All Python layer-by-layer build details in sections 10.1–10.13 are superseded.
The layer concepts below remain valid as a conceptual build map for Release 1.
See ROADMAP.md for current Release 1 status.

Release 1 scope: everything except community library (D6-359).
Release 2: community library only (browsing, discovery, signing, verification,
community scoring, server infrastructure).

### 10.1 Layer Concepts (conceptual — implementation is Rust/Tauri)

| Layer | Domain | Status |
|---|---|---|
| 0 | Project foundation: toolchain, DB init, taxonomy, health endpoint | ✅ Complete |
| 1 | Ollama client and evaluation harness | ✅ Complete |
| 2 | Conductor skeleton: lifecycle phases, context tracks, failure taxonomy | ✅ Complete |
| 3 | First Focus (Quick Ask): full end-to-end run, output in outputs.db | ✅ Complete |
| 4 | Privacy Guardian: all four gates operational, golden vectors passing | ✅ Complete (201 vectors) |
| 5 | Personal context: fields stored, retrieved, injected | ✅ Complete |
| 6 | Writing Assistant: first Tier 2 call with full Privacy Guardian protection | ✅ Complete |
| 7 | Remaining Focuses — see FOCUS_ROADMAP.md for full list and build order | ⬜ 10 design sessions remaining |
| 8 | Multi-user auth: PBKDF2, key management, session lifecycle, lockout enforcement | ⬜ Partially stubbed |
| 9 | Optimizer: quality signals, drift detection, notifications | ⬜ Pending |
| — | Daily Brief Conductor capability | ⬜ Pending |
| — | Quick Launch Dock population logic | ⬜ Pending |
| — | Chapter card interactive sub-view | ⬜ Pending |
| — | Frontend SPA | ⬜ Pending (unblocks after Active Board design locked) |
| — | Focus Builder | ⬜ Pending |

### 10.2 Rust Repository Structure

```
quietrabbit-core/
├── CLAUDE.md                    ← dev workflow reference
├── README.md
├── .env / .env.example
├── archive/                     ← retired Python files (reference only)
│   ├── Dockerfile · docker-compose.yml
│   ├── interview.py · apply_modelfiles.py
│   ├── extract_golden_vectors.py
│   └── taxonomy/ (5 YAML files)
├── app/
│   └── core_artifacts/
│       ├── focuses/             ← .focus files (YAML, Rust Conductor loads these)
│       ├── guides/              ← .guide files
│       └── operators/           ← .operator files
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── binaries/                ← Ollama sidecar binary (stub → real at build time)
│   └── src/
│       ├── main.rs
│       ├── lib.rs
│       ├── ollama_sidecar.rs
│       ├── conductor/           ← lifecycle, executor, privacy, memory_broker, etc.
│       ├── persistence/         ← migrations, 7 stores
│       ├── providers/           ← ollama_client, groq, evaluation, etc.
│       └── commands/            ← 12 IPC command submodules
└── tests/
    └── golden_vectors.rs        ← 204 tests, 201 gate vectors
```

### 10.3 Release 1 Completion Criteria

**Functional:**
All 8 Focuses end-to-end without errors.

**Privacy and security:**
```
✅ No personal field above max_permitted_tier sent without PG gate approval
✅ No Tier 2/3 call without disclosure_log entry
✅ SQLCipher linkage via libsqlite3-sys sqlcipher feature (D6-346)
✅ PRAGMA key before PRAGMA journal_mode on every connection
✅ Master key never on disk — held in tauri::State (partial Release 1)
✅ Recovery key exact reconstruction — BIP39 Option B
⬜ auth_enabled=1 only after all-database migration success (Layer 8)
⬜ Auth session enforcement + lockout wired (D6-359)
✅ Medical/financial content blocked from system clipboard
✅ Tier 2+ terminal boundary working — consent gate before send
✅ Interrupted runs detected on startup, marked paused
⬜ Ollama binary stubs replaced with real binaries (build pipeline pass)
```

**Quality signals:**
model_hardware_scores, Level 1 scores above 0.55, Optimizer
thresholds operational, disclosure_log complete.

**Operational:**
```
✅ Startup integrity check passing
✅ Migration runner operational (migration_lock, SAVEPOINT atomicity)
✅ 204 tests passing, 0 failures
⬜ QR_ENV=development NOT in any production config
⬜ TOPOLOGY A SMOKE TEST: clean single-machine install, no config,
  app launches, all 8 Focuses run end-to-end.
  This is the most important verification before shipping.
```

### 10.4 What Release 1 Intentionally Excludes (community library only)

Release 2 (community library):
- Community Focus library browsing and discovery UI
- Artifact signing and verification pipeline
- Community scoring system
- Server infrastructure for hosting community Focuses
- Community contributions system

Internal complexity deferred indefinitely (no user impact):
- Context group management UX (schema exists, no UI)
- Hot reload for .focus files (dev convenience)
- Revocation registry (no user-facing manifestation)
- Security Checker component split (implementation detail)

---

## CHAT-PM HANDOFF — Updated June 28, 2026

**Current status:** Release 1 design phase active. 13 Focuses confirmed across
4 R1 Personas. 10 Focus design sessions remaining. Tech Support is next.
Backend complete on main (218 tests, commit d65d83a).
See ROADMAP.md for full build sequence. See FOCUS_ROADMAP.md for Focus list.

**Architecture decisions locked since June 21 2026:**
- Entity model: entities + entity_facts, 15 invariants (D6-371–D6-375)
- Hierarchical fact model: scope tree + knowledge graph (D6-459)
- Source of truth framework: D6-460
- Persona invariants: Medical (two exits + export gate), Wellness (post-R1),
  Work (intentionally minimal) — June 28 2026
- Cooking design: D6-461–D6-464
- Travel design: D6-399–D6-458
- Writing Assistant design: D6-376–D6-398

--- END OF CANONICAL ARCHITECTURE ---
