# Quiet Rabbit — Architecture Reference
# QUIET_RABBIT_ARCHITECTURE.md

**Engine:** Conductor
**Version:** 0.1
**Status:** Release 1 active development
**Last updated:** May 30, 2026 (Phase D start — committed to repo)
**Companion document:** QUIET_RABBIT_DESIGN.md

---

## DOCUMENT STATUS

| Section | Status | Notes |
|---|---|---|
| 1 — System Overview | validated | |
| 2 — Deployment Topologies | validated | |
| 3 — Data Model Reference | validated | |
| 4 — File Format Specifications | validated | |
| 5 — Taxonomy Files Reference | validated | |
| 6 — Conductor Execution Reference | validated | |
| 7 — API Reference | validated | |
| 8 — Auth and Multi-User Reference | validated | Final hardening pass May 29 |
| 9 — Development Environment Setup | validated | |
| 10 — Phase 1 Build Sequence | validated | |

This completes the Release 1 architecture specification.
All sections validated through multi-AI external review (ChatGPT + Gemini).
Final hardening pass applied May 29: threat model, session lifecycle,
recovery invariants, silent operator principle, EOF marker.
This file is the living document. NAS copy at
03_ProjectDocs/Architecture/QUIET_RABBIT_ARCHITECTURE.md is the archive.

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

---

## 1. System Overview

Quiet Rabbit is a self-hosted personal AI platform. The Conductor is the
execution engine that orchestrates paths — structured task sequences run
through teams of specialists. Everything runs on the user's own hardware.
Nothing is sent externally without explicit user consent at every boundary.

### 1.1 Core Concepts

Space — privacy default tier, active specialists, integration permissions.
Specialist — role definition only; personal data lives in encrypted store.
Path — curated specialist sequence; personal data never included.
Personal Specialist — persistent context, three ownership scopes.
Conductor — cohesive Python service, modular components.
Your Library — personal document store (outputs.db).

### 1.2 Three-Tier Model Routing

Tier 1: Local Ollama. Private, offline, always available.
Tier 2: User-configurable external API. Mistral (EU/GDPR, paid) or
        Groq (US, free tier). User choice at install. No prescribed default.
Tier 3: Validation providers (Claude, ChatGPT, Gemini). Always optional,
        always explicit, never automatic.

```python
# Hard security ceiling — cannot be exceeded under any circumstance
effective_tier = min(
    space.max_permitted_tier,
    path.max_routing_tier,
    step.routing_tier
)
# User preference — default, may be elevated with consent up to ceiling
preferred_tier = min(space.privacy_default_tier, effective_tier)
```

**External call consent invariant:** No content crosses a tier boundary
without a corresponding disclosure_log entry and explicit user
acknowledgment at the appropriate Privacy Guardian gate. No silent
fallback to higher tiers. No automatic retry to cloud provider.

### 1.3 Specialist Architecture

Layer 1 — System specialists (always active, never declared in .path files):
  Privacy Guardian, Security Checker, Path Optimizer, Support Specialist,
  Librarian, Path Builder, Personal Specialist.
Layer 2 — Domain specialists (calibrated over time, shareable).
Layer 3 — Path specialists (targeted, purpose-built, shareable).

### 1.4 The Silent Operator Principle

When the Conductor injects personal context into a path step, the output
reflects that context without narrating it. The system uses personal data
to inform the answer — it does not reference the data source.

Correct: output naturally reflects user's location, tone, situation.
Wrong: "Since you mentioned you live in Avon..." or "Based on your
preference for direct communication..."

This applies to all specialist prompts and all output types. The personal
context is ambient, not cited. Violation of this principle is a
prompt engineering bug, not a user-facing feature.

### 1.5 Privacy Enforcement Model

Field-level sensitivity: general / personal / medical / financial.
HKDF name-based key derivation — new levels: append to yaml, zero migration.
Instance-scope restricted to general and personal sensitivity only.
Privacy Guardian invoked at four gates before any external call.
No telemetry. No usage data sent anywhere.

### 1.6 Phase 1 Paths (8 paths — confirmed May 29, 2026)

Writing Assistant, Research & Buy, Job Match, Tech Support,
Travel/Vacation Planning, Cooking, Personal Finance, Quick Draft.

Layer 7 build order: Travel/Vacation Planning after Job Match and Research & Buy.
Quick Draft: confirmed as a full path (Layer 3). Output type: quick_draft.
suggest_in_paths: [writing-assistant, job-match, research-and-buy] —
Librarian surfaces prior draft as starting point when those paths open.
Life Transitions → Phase 2 (milestone paths: wedding, pregnancy, career, retirement).

### 1.7 Technical Stack

Python 3.11+ · Flask + Flask-Login + Flask-Session
sqlcipher3 (standard sqlite3 ignores PRAGMA key/rekey silently)
Custom Conductor engine (direct provider API calls — no CrewAI or litellm)
bcrypt 12 rounds · PBKDF2 600k iterations · HKDF · BIP39 recovery
Ollama: llama3.2:3b / llama3.1:8b / qwen2.5:7b · BSL 1.1

---

## 2. Deployment Topologies

### 2.1 Environment Variables

**Required:**
```bash
OLLAMA_HOST=ollama  OLLAMA_PORT=11434  QR_DATA_ROOT=/data/quietrabbit
QR_ENV=production   QR_NETWORK_STORAGE=false
```

**Optional tuning:**
```bash
QR_STARTUP_INTEGRITY_CHECK=true   QR_FALLBACK_ON_INTEGRITY_FAIL=true
QR_LOOP_DETECTION_THRESHOLD=3     QR_CONTEXT_WARNING_THRESHOLD=0.75
QR_QUALITY_FLOOR=0.55             QR_INTERRUPT_THRESHOLD_MINUTES=5
QR_MAX_CONCURRENT_PATHS=3         QR_CHECKPOINT_EVERY_N_STEPS=3
```

**Security / debug (change carefully):**
```bash
QR_ALLOW_HTTP=true    # LAN only — set false when HTTPS introduced
# QR_ENV=development  # NEVER in production — logs personal field values
```

### 2.2 Topologies

A: Single machine — no .env needed.
B: Separate inference machine — set OLLAMA_HOST={ip}.
C: Separate storage — QR_NETWORK_STORAGE=true for network mounts.
   WAL mode unreliable on NFS/SMB/9P (locking semantics vary by
   implementation) — rollback journal used instead.
D: Laptop + Tailscale remote access.

### 2.3 Data Root Structure

```
{QR_DATA_ROOT}/
├── instance/shared.db
├── users/{user_id}/spaces/{space_id}/personal.db + outputs.db
├── users/{user_id}/integration_keys.db
├── models/scores.db
├── cache/last_known_good/ · config/ · sessions/ · linked/
└── community_artifacts/paths/ specialists/ integrations/
```

### 2.4 Startup Sequence

Steps 1-5 halt on failure. Steps 6-9 degrade. 11 steps total.
Headless boot (multi-user): instance databases accessible, per-user
databases locked until login, queued notifications at first login.

### 2.5 Interaction Design Constraints

Plain Language Rule: non-technical, action-focused, one clear action,
reassure-then-guide.
Severity: INFO (notification center) / SUGGEST (contextual) /
          REQUIRE (blocking) / STOP (hard block).
Passive acquisition order before any prompt:
  current run context → Personal Specialist fields → space context →
  library outputs → user uploads → explicit prompt (last resort).

---

## 3. Data Model Reference

### 3.1 Overview

pysqlcipher3 throughout. Additive schema evolution. migration_lock in
every database. WAL locally, rollback journal on network storage.

Field encryption: HKDF(master_key, info=f"qr-field-key-{label}").

### 3.2 instance/shared.db

spaces — canonical source of truth. max_permitted_tier (hard ceiling)
  and privacy_default_tier (preference) are distinct.
users — no password_hash on User object; returned separately in
  credential lookup.
user_salts, user_spaces — ON DELETE CASCADE.
instance_context — CHECK(sensitivity IN ('general','personal')) only.
  Medical and financial never instance-scoped.
context_groups, context_group_members — Release 1 schema, Release 2 UX.
artifact_versions — in shared.db (not per-space). All file types tracked.

### 3.3 personal.db

personal_fields — sensitivity_severity GENERATED ALWAYS (1-4, unknown=99).
personal_field_groups — MOVED HERE for local FK enforcement. group_id
  is cross-db reference resolved at application layer.
voice_profiles — precedence: model baseline → specialist defaults →
  global → space → writing context overrides.
disclosure_log — NEVER deleted. override_declined + declined_at columns.
staleness_check_state — UNIQUE(user_id, space_id), one row per space.

### 3.4 outputs.db

outputs — sensitivity_severity GENERATED. Purge tracking columns.
  Deletion sequence: zero content → COALESCE FTS5 update → set deleted.
path_runs — status includes 'initializing'. Created as initializing,
  promoted to running after Phase 3 success only.
  is_fast_lane BOOLEAN DEFAULT 0 — Phase 1 capture only.
path_run_snapshots — PersonalTrack never serialized. Two-phase commit.
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
/app/core_artifacts/ (immutable) vs {QR_DATA_ROOT}/community_artifacts/.
Prompt template: {variable} for injection, {{ }} for literal braces.
output_var required on every step producing output consumed downstream.
Layer 1 specialists never declared in .path files.

artifact_versions in shared.db (not per-space).
space_affinity: [legal-finance] (matches actual space id, not 'financial').
Security Checker: 5a whitelist → 5b structural (DAG validation) →
  5c semantic (provider registry) → 5d pattern heuristics.
Validation pipeline: 8 steps, atomic activation.
Multi-source validation: max 2 providers, SYNTHESIS_CONTENT_LIMIT=2000
  (synthesis summary only — full content always in outputs.db).

---

## 5. Taxonomy Files Reference

Five files in memory at startup. SHA-256 manifest verification.
Bundled files hash-verified; user config schema-validated only.
validation_mode: development=warn, production=fail_fast.

Key decisions:
- long_context threshold 0.75: surface Tier 2 option, never auto-route
- not_applicable: registered fourth retroactive_extraction enum value
- space_affinity [legal-finance] for financial output types
- Mistral + Groq both first-class with honest trade-off framing
- url_overflow: clipboard_and_base_url for all Tier 3 providers
- Explicit per-task-type fallback chains in routing_table

Travel/Vacation Planning output types (add to output_types.yaml):
  itinerary, attraction_research, packing_list, trip_summary
  space_affinity: [nature-travel], sensitivity: general

---

## 6. Conductor Execution Reference

### 6.1 Seven-Phase Lifecycle

Phases 1-5 and 7 mandatory. Phase 6 async and optional.

Phase 1 LOAD:       load .path, validate, check artifact_versions
Phase 2 AUTHORIZE:  tier checks, create path_run status=initializing
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
  {user_input, space_context, voice_profile, previous_output, path_context}

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
F2: Quality below floor → fast model, offer Tier 2 if confidence >= 0.55
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

ContextCompactor: direct local model call. No specialist routing.
No PG gates. No path orchestration. Avoids Conductor-Librarian
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

### 6.9 Path Optimizer

Signals written passively. signal_validity: valid/partial/invalid.
Three notification types:
  suggest_model_swap: 10 valid runs, 15% quality gap
  suggest_tier_upgrade: 5 runs, 30% floor breach rate
  compaction_applied: 5 runs, 50% compaction rate

### 6.10 Resource Arbitration

MAX_CONCURRENT_INFERENCE=1. MAX_CONCURRENT_PATHS from env (default 3).
Interactive preempts background at step boundaries.
GPU memory: graceful downgrade with plain-language offer to user.

---

## 7. API Reference

### 7.1 Core Types

GenerateRequest: stream: bool | None = None (StepExecutor resolves).
GenerateResponse: completion_status replaces done field.
ChatMessage: role: Literal['system', 'user', 'assistant'].
ProviderHealth: available_models: list[str] = field(default_factory=list).
ContextWindowStatus: status 'ok' | 'warn' | 'exceeded'.
  recommended_action: 'compact_then_escalate'.

### 7.2 Ollama Client

Health check: GET /api/tags, 5s timeout, never raises.
generate(): overlay applied, latency logged. stream=False Release 1.
chat(): latency tracked — NOT hardcoded 0.
apply_modelfile(): validates NDJSON response body for success status.
  Logs to diagnostic view (not silent — Modelfile changes inference).
estimate_token_count(): heuristic ~4 chars/token. May underestimate
  for code, JSON, non-English. Over-estimation is safe.
Release 2 streaming: Tier 1 progressive, Tier 2 buffer-and-release
  (intentional — PG_GATE_2 needs complete response before display).

### 7.3 Tier 2 Provider Interface

Tier2Provider ABC: generate(), health_check(), estimate_cost() with
  exact signatures (no *args/**kwargs).
API keys from integration_keys.db only. Never from env.
open_db() wrapper used throughout — explicit close.
MistralProvider, GroqProvider: _get_key() lazy-loads.
OpenAICompatibleProvider: _get_key() implemented.
CLIPBOARD_MAX_SENSITIVITY_SEVERITY=2 (personal and below).
Medical/financial: manual copy UI, never system clipboard.
Linux clipboard subprocess: timeout=2.
Pre-check content size before urllib.parse.quote().

### 7.4 Validation Provider Interface

ValidationLinkGenerator: clipboard_blocked flag for sensitivity gate.
ValidationReturnHandler: runs as Phase 6 FEEDBACK (not Phase 4).
  _summarize_diff uses task_type='summarization' (not structured_output).
MultiSourceSynthesis: synthesis_truncated flag. Full content in outputs.db.

### 7.5 Privacy Guardian Hooks

Four typed request/response pairs. ValidationContentResponse includes
content_sensitivity_severity for clipboard safety decision.

### 7.6 Error Taxonomy

All errors extend QRAPIError with plain_language. Mapped to F1-F10.

### 7.7 Evaluation Harness Level 1

Score = (latency_score x 0.40) + (format_compliance x 0.60).
Results written to model_hardware_scores.

### 7.8 Progress Indicator

Release 1: polling every 2s. Step display_name from .path file.
Release 2: server-sent events.

---

## 8. Auth and Multi-User Reference

### 8.1 Overview

Single-user (default, auth_enabled=0): install keychain key, no login.
Multi-user (opt-in, auth_enabled=1): PBKDF2 password-derived keys.
auth_enabled=1 ONLY if ALL databases migrate successfully.
Partial failure: rollback migrated databases, keep auth disabled, retry.

### 8.2 Key Architecture

```python
from pysqlcipher3 import dbapi2 as sqlite3   # required — standard is no-op

def derive_user_master_key(password: str, salt: bytes) -> bytearray:
    raw = hashlib.pbkdf2_hmac('sha256', password.encode(), salt, 600_000, 32)
    return bytearray(raw)   # bytearray: mutable, in-place zeroing possible
    # 600,000 iterations: current conservative implementation choice

def derive_field_key(master_key: bytearray, sensitivity_label: str) -> bytes:
    return HKDF(info=f"qr-field-key-{sensitivity_label}".encode()).derive(bytes(master_key))

def derive_snapshot_key(master_key: bytearray, path_run_id: str) -> bytes:
    return HKDF(info=f"qr-snapshot-{path_run_id}".encode()).derive(bytes(master_key))
```

**InMemoryKeyRegistry:** Master key NEVER in Flask session store.
Flask session contains session_id only. Keys in InMemoryKeyRegistry
keyed by session_id. Volatile — process restart clears all keys.
In-place zeroing is best-effort. Python offers no guaranteed OS-level
secure erasure — no interpreter copy guarantees, no mlock by default.
This is the correct approach within Python's runtime constraints.

**Install keychain (single-user):** keyring library with explicit backend
check. Insecure plaintext backends rejected at startup with plain-language
error. Supported: Secret Service (Linux), macOS Keychain, Windows
Credential Manager.

### 8.3 Flask Auth Stack

SESSION_COOKIE_SECURE = not QR_ALLOW_HTTP (defaults False for LAN HTTP).
Set QR_ALLOW_HTTP=false to enforce HTTPS when reverse proxy introduced.
Remember-me: disabled in multi-user mode.
Key availability middleware: enforce_key_availability() redirects to
/reauth if authenticated but no key in registry.

### 8.4 Session Lifecycle

```
Idle timeout:          30 minutes (configurable, default 30m)
Absolute lifetime:     30 days (PERMANENT_SESSION_LIFETIME)
Concurrent sessions:   permitted — all active sessions share registry
                       under same user_id, each with unique session_id
Key eviction triggers:
  - Explicit logout: InMemoryKeyRegistry.clear(session_id)
  - Password change: InMemoryKeyRegistry.clear_all_for_user(user_id)
  - Process termination: volatile memory cleared automatically
  - Idle timeout: session file expires, next request fails key lookup,
                  redirected to /reauth
  - Remember-me restore (multi-user): key not in registry after restart,
                  enforce_key_availability() redirects to /reauth

Single-user mode:
  Master key retrieved from keychain on each access attempt.
  No timeout — keychain is always available if unlocked.
  Headless boot: keychain unlocked at OS level (KWallet/Secret Service).

Re-authentication flow (/reauth):
  User re-enters password → PBKDF2 derives key → stored in registry
  under existing or new session_id → continues from where they were.
```

### 8.5 Login and Logout

User model: no password_hash attribute. Credential lookup returns
(User, password_hash) tuple. Hash used for verification only, discarded.
Auto-login (single-user): explicit False + error if primary user missing.
No silent redirect loop.

### 8.6 Recovery Key — Formal Specification and Invariants

**Implementation — Option B (mnemonic IS the master key):**

```python
def generate_recovery_key(master_key: bytearray) -> str:
    return mnemo.to_mnemonic(bytes(master_key))
    # 32 bytes → 256-bit entropy → 24-word BIP39 mnemonic
    # Shown ONCE at account creation. Never stored.

def recover_with_key(recovery_mnemonic: str, new_password: str, user_id: str):
    entropy_bytes = mnemo.to_entropy(recovery_mnemonic)
    original_master_key = bytearray(entropy_bytes)
    # Exact original 32 bytes. No wrapping. No random suffix.
    # Re-encrypts all databases with new password-derived key.
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

### 8.7 Database Re-encryption

```python
from pysqlcipher3 import dbapi2 as sqlite3   # standard sqlite3 is a no-op

def rekey_database(db_path, old_key, new_key):
    conn = sqlite3.connect(str(db_path))
    conn.execute(f"PRAGMA key = \"x'{old_key.hex()}'\"")
    conn.execute(f"PRAGMA rekey = \"x'{new_key.hex()}'\"")
    conn.commit(); conn.close()
    # Reopen with new key to verify — catches silent failures
    conn2 = sqlite3.connect(str(db_path))
    conn2.execute(f"PRAGMA key = \"x'{new_key.hex()}'\"")
    conn2.execute("SELECT count(*) FROM sqlite_master")
    conn2.close()
```

Migration states: pending → in_progress → verifying → committed | rolled_back | failed.
All databases must reach committed before auth_enabled=1.
Partial failure: rollback all migrated, keep auth_enabled=0, surface retry.

### 8.8 Auth Session Tables

auth_sessions, auth_failures, auth_lockouts in shared.db.
Release 1: schema present, NOT enforced.
auth_lockout_enabled flag independent of role_enforcement flag.
Timestamp comparison: datetime.fromisoformat() + timezone-aware.

### 8.9 Threat Model

Protects against: local disk theft, offline DB extraction, unauthorized
local account access (multi-user), network interception of external calls.

Does NOT protect against: compromised running host, root-level malware,
memory scraping of active sessions, hostile machine owner, social
engineering, vulnerable dependencies.

Design scope: personal self-hosted use on trusted owner-operated hardware.

### 8.10 Tier 2 Provider Choice — Install Interview

No provider prescribed. Install interview presents both with honest
trade-off framing. Stored in users.tier2_provider_preference.

```
Mistral (Europe): GDPR-native, paid, privacy-first users.
Groq (US):        Free tier, US jurisdiction, cost-first users.
Decide later:     Local only until configured.
```

---

## 9. Development Environment Setup

### 9.1 Current Development Topology

```
Proxmox Host (Ryzen 7 5700X / 32GB)
  CT 111 LXC: 192.168.88.95
    QR: http://192.168.88.95:3000
    Portainer: https://192.168.88.95:9443
  NAS mount on Proxmox host: /mnt/pve/NAS/QuietRabbit/

Garuda Desktop (Ryzen 5 7600X3D / RX 6600 8GB)
  LAN (ethernet/eno1): 192.168.88.26
  WiFi: 192.168.88.81
  Tailscale: 100.113.187.192
  Ollama: http://192.168.88.26:11434
  NAS mount on Garuda: /mnt/NAS/QuietRabbit/

Repo: /mnt/NAS/QuietRabbit/06_GitRepos/quietrabbit-core/
  (Claude Code initialized from this directory — picks up CLAUDE.md)

QR data (config, outputs, open-webui): /mnt/NAS/QuietRabbitData/
  Not inside the vault. Separate from project files.
```

### 9.2 Development .env

```bash
OLLAMA_HOST=192.168.88.26
OLLAMA_PORT=11434
QR_LOCAL_DATA=/mnt/NAS/QuietRabbitData
QR_ENV=development
QR_NETWORK_STORAGE=true
QR_ALLOW_HTTP=true
```

Docker volume in compose maps QR_LOCAL_DATA → /data/quietrabbit in container.

### 9.3 pysqlcipher3 Setup

```bash
# Arch/Garuda
sudo pacman -S sqlcipher && pip install pysqlcipher3
python -c "from pysqlcipher3 import dbapi2; print('OK')"
# Then verify SQLCipher is actually linked (not standard sqlite3):
python -c "
from pysqlcipher3 import dbapi2 as sqlite3
conn = sqlite3.connect(':memory:')
conn.execute(\"PRAGMA key='test'\")
print(conn.execute('PRAGMA cipher_version').fetchone()[0])
"
```

### 9.4 Model Setup

```bash
ollama pull llama3.1:8b && ollama pull llama3.2:3b && ollama pull qwen2.5:7b
curl http://192.168.88.26:11434/api/tags
# Ollama must listen on 0.0.0.0 — restrict port 11434 at firewall/Tailscale
```

### 9.5 Observability

Log rotation: json-file driver, max-size 50m, max-file 5.
NEVER run QR_ENV=development in production — logs decrypted personal
field values, full assembled prompts, routing decisions, snapshot state.
Privacy-safe export: scripts/export_diagnostics.py --strip-personal

### 9.6 Backup

Back up: users/, instance/, integration_keys.db, config/.
Exclude: sessions/ (ephemeral), models/scores.db (auto-rebuilds).

---

## 10. Phase 1 Build Sequence

Implementation tool: Claude Code on Garuda, initialized from repo root
/mnt/NAS/QuietRabbit/06_GitRepos/quietrabbit-core/ (picks up CLAUDE.md).
Architecture document at /docs/QUIET_RABBIT_ARCHITECTURE.md wins on all
conflicts. Stop and resolve before implementing anything not covered here.

Install interview: CLI bootstrap acceptable for Layer 5.
Full UI conversational interview follows later.

### 10.1 Repository Structure

```
quietrabbit-core/
├── CLAUDE.md                          <- Claude Code session context
├── README.md
├── docker-compose.yml                 <- QR Conductor (not Open WebUI)
├── .env.example                       <- committed, values empty
├── .gitignore
├── docs/
│   └── QUIET_RABBIT_ARCHITECTURE.md  <- this file (living document)
├── app/
│   ├── core_artifacts/paths/ specialists/ spaces/ integrations/
│   ├── taxonomy/ (5 yaml files + manifest.yaml)
│   └── modelfiles/ (llama3.1-8b, llama3.2-3b, qwen2.5-7b)
├── conductor/
│   ├── __init__.py  lifecycle.py  context.py  executor.py
│   ├── privacy.py  failure.py  feedback.py  compaction.py
│   ├── concurrency.py  tokens.py
├── providers/
│   ├── __init__.py  utils.py  errors.py  types.py
│   ├── ollama_client.py  tier2_base.py  mistral.py  groq.py
│   ├── validation.py  evaluation.py
├── auth/
│   ├── __init__.py  keys.py  key_registry.py  models.py
│   ├── credentials.py  passwords.py  recovery.py  migration.py
│   ├── routes.py  decorators.py
├── persistence/
│   ├── __init__.py
│   ├── schema/ (shared_001.sql personal_001.sql outputs_001.sql keys_001.sql)
│   ├── migrations.py  personal_store.py  output_store.py  space_store.py
├── taxonomy/
│   ├── __init__.py  loader.py
├── ui/
│   ├── __init__.py  routes.py
├── scripts/
│   ├── generate_manifest.py  export_diagnostics.py
│   ├── init_db.py  apply_modelfiles.py
├── tests/
│   ├── test_taxonomy.py  test_conductor.py
│   ├── test_privacy_guardian.py  test_auth.py
│   └── test_paths/ (test_quick_draft.py  test_writing_assistant.py)
├── Dockerfile
└── requirements.txt
```

### 10.2 Layer 0 — Project Foundation

Milestone: Docker starts, taxonomy verifies, databases initialize,
health endpoint responds. SQLCipher verified before proceeding.

```
☐ docker-compose.yml — QR Conductor (replaces Open WebUI version)
☐ .env.example — all variables with descriptions, values empty
☐ requirements.txt — pysqlcipher3 first in list
☐ Dockerfile
☐ providers/utils.py — now(), open_db() wrapper
☐ providers/errors.py — full QRAPIError hierarchy
☐ taxonomy/loader.py — load, verify manifest, write last_known_good
☐ scripts/generate_manifest.py
☐ persistence/schema/*.sql — all four schemas
☐ persistence/migrations.py — runner with migration_lock
☐ scripts/init_db.py
☐ auth/__init__.py, auth/models.py — stubs only (full auth in Layer 8)
☐ ui/routes.py — /health and /diagnostics JSON endpoints
☐ Startup Steps 1-5 passing
```

Verification — do not proceed until both pass:
```bash
# 1. Health endpoint
curl http://192.168.88.95:3000/health
# {"status":"ok","taxonomy":"verified"}

# 2. SQLCipher actually linked (not silent sqlite3 fallback)
docker exec qr-conductor python -c "
from pysqlcipher3 import dbapi2 as sqlite3
conn = sqlite3.connect(':memory:')
conn.execute(\"PRAGMA key='test'\")
print(conn.execute('PRAGMA cipher_version').fetchone()[0])
"
# Must print a version string, e.g. "4.5.5 community"
```

### 10.3 Layer 1 — Ollama Client and Evaluation Harness

```
☐ providers/types.py — all data types (stream: bool | None = None)
☐ providers/ollama_client.py — health, generate(), chat() with latency
☐ providers/evaluation.py — EvaluationHarness, Level 1
☐ Startup Steps 6-7 (Ollama connectivity, Modelfile versions)
☐ scripts/apply_modelfiles.py
☐ model_hardware_scores seeded for all 3 models x 8 task types
```

### 10.4 Layer 2 — Conductor Skeleton

```
☐ conductor/tokens.py — SYSTEM_TOKENS frozenset, StepDefinition
☐ conductor/context.py — PersonalTrack, TaskTrack, SharedStateTrack,
    PersonalContextManifest, update_sensitivity_ceiling
☐ conductor/lifecycle.py — PathRun, all 7 phases as stubs
    Phase 2: creates path_run status=initializing
    Phase 3: promotes to running after success
☐ conductor/failure.py — FailureHandler F1-F10
☐ persistence/personal_store.py — stubs
☐ persistence/space_store.py — full CRUD
```

### 10.5 Layer 3 — First Working Path: Quick Draft

Milestone: end-to-end run, output in outputs.db.
Single step, Tier 1 only, no personal fields, no PG gates.
Proves full Conductor pipeline before adding complexity.

```
☐ app/core_artifacts/paths/quick-draft.path
    output_type: quick_draft
    suggest_in_paths: [writing-assistant, job-match, research-and-buy]
☐ conductor/executor.py — StepExecutor, full 15-step sequence
☐ conductor/compaction.py — ContextCompactor (direct local call)
☐ conductor/concurrency.py — ConductorScheduler
☐ Phase 4 EXECUTE and Phase 5 OUTPUT wired
☐ outputs.db with COALESCE FTS5 triggers
☐ Minimal Flask UI — submit, step display_name progress, output display
```

Do not proceed until Quick Draft produces coherent output in outputs.db.

### 10.6 Layer 4 — Privacy Guardian

```
☐ conductor/privacy.py — all four gates
☐ Rules-based primary, pre-generated templates at LOAD
☐ Steps 6 and 11 wired in StepExecutor
☐ disclosure_log written on every invocation
```

Positive test: salary (range_only) → abstracted in buffer.
Negative test: financial field + Tier 2 space → not_permitted → withheld
  from disclosure buffer, step proceeds with permitted fields only.
  ADR-012 Amendment 1: not_permitted enforces at Tier 2+ only.
  Raw values are permitted at Tier 1 (no external call, no abstraction).

### 10.7 Layer 5 — Personal Specialist

```
☐ personal-specialist.specialist
☐ persistence/personal_store.py — full CRUD, field-level encryption
☐ CLI install interview (not UI yet — proves field storage first)
☐ Voice profile assembly — all five precedence levels
☐ PersonalTrack population at Phase 3
☐ Steps 5-8 wired for Tier 1 personal injection
```

### 10.8 Layer 6 — Writing Assistant (Tier 2)

```
☐ writing-assistant.path + writing-voice.specialist
☐ providers/tier2_base.py + mistral.py + groq.py
☐ Provider factory — user preference, no prescribed default
☐ Full 15-step for Tier 2 including buffer read at Step 8
☐ Clipboard sensitivity gate (medical/financial → manual copy UI)
☐ Tier 3 terminal boundary
☐ Phase 6 FEEDBACK — paste-back, diff, quality rating
```

### 10.9 Layer 7 — Remaining Five Paths

Build order: Research & Buy → Job Match → Tech Support → Cooking →
Travel/Vacation Planning → Personal Finance.

Travel output types: itinerary, attraction_research, packing_list, trip_summary.
Personal Finance: max_permitted_tier=1 enforcement throughout.

### 10.10 Layer 8 — Multi-User Auth

```
☐ auth/ package complete
☐ InMemoryKeyRegistry (volatile, threading.Lock)
☐ Recovery key: mnemo.to_mnemonic(bytes(32_byte_key))
    Recovery: bytearray(mnemo.to_entropy(mnemonic)) → exact original
☐ Migration state machine with rollback
☐ auth_enabled=1 only on all-database success
☐ Session lifecycle defaults (Section 8.4)
```

### 10.11 Layer 9 — Path Optimizer

```
☐ signal_validity classification
☐ model_quality_scores on valid/partial runs
☐ Three notification types at correct thresholds
☐ Notification center operational
```

### 10.12 Release 1 Completion Criteria

Functional: all 8 paths end-to-end.

Privacy and security:
```
☐ No personal field above max_permitted_tier sent without PG gate approval
☐ pysqlcipher3 confirmed via PRAGMA cipher_version
☐ Master key never on disk
☐ Recovery key exact reconstruction verified
☐ auth_enabled=1 only after all-database migration success
☐ Medical/financial content blocked from system clipboard
☐ Tier 3 terminal boundary — no synchronous wait
```

Operational:
```
☐ Startup integrity check passing
☐ Log rotation configured
☐ QR_ENV=development NOT in production config
☐ TOPOLOGY A SMOKE TEST — clean single-machine Docker install,
  no .env, docker compose up, all 8 paths run.
  Most important verification before shipping.
```

### 10.13 What Release 1 Intentionally Excludes

Release 2: group-scope UX, community library, direct Tier 3 API,
SSE streaming, role enforcement, auth lockout, Path Builder UI,
"Promote to path" fast lane feature, context group management,
hot reload, revocation registry, Security Checker component split.

Phase 3: community scoring, signature verification, contributions.

--- END OF CANONICAL ARCHITECTURE ---
