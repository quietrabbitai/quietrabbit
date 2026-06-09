# Quiet Rabbit — Claude Code Context

## Session Discipline
- Respond with code only. No preamble, no recap, no explanation unless asked.
- Run /compact at natural phase boundaries: end of each file written, before
  integration review, before returning to Chat-PM. Do not wait for auto-compaction.
- When compacting, preserve: open files, current task, architectural decisions,
  unresolved errors, and the next action.
- One task at a time. Draft → Jason approves → write file. Never skip approval.

## Project
Self-hosted privacy-first AI platform. Engine: Conductor. Version: 0.1.
Tagline: "Your personal AI. Simple to start, built to grow, always yours."

## Architecture Reference
/docs/QUIET_RABBIT_ARCHITECTURE.md — authoritative for all decisions.
Read the relevant section before writing code.
This file wins over all other sources on conflicts.
When in doubt: stop and ask rather than invent.

## Core Tenets (non-negotiable)
- Privacy-first: no data leaves local without explicit user consent
- Self-hosted: runs entirely on user hardware
- No telemetry: QR never sends usage data anywhere
- sqlcipher3 (Charles Leifer) required: standard sqlite3 ignores PRAGMA key/rekey silently.
  pysqlcipher3 is unmaintained and incompatible with Python 3.13+.
  Import: `from sqlcipher3 import dbapi2 as sqlite3`
  Dockerfile: `sqlcipher3-binary` in requirements.txt
  Verified: SQLCipher 4.12.0 community
- Master key never persisted to session store: keys live only in
  InMemoryKeyRegistry keyed by session_id. Flask session contains
  session_id only. Never master_key_hex.
- Tier 2 = user choice: Mistral (EU/GDPR, paid) or Groq (US, free tier).
  Honest trade-off framing. No prescribed default.
- Silent operator: personal context informs output, never narrated.
  Never "Since you mentioned..." or "Based on your preference..."
- Human in the loop: all auto-improvements require explicit approval.
  No silent changes.

## Dev Environment
Repo root (Claude Code runs here, on Garuda):
  /mnt/NAS/QuietRabbitMirror/06_GitRepos/quietrabbit-core/

NAS paths as seen from Garuda:
  Project vault:  /mnt/NAS/QuietRabbitMirror/
  QR data:        /mnt/NAS/QuietRabbitData/  <- config, outputs, open-webui
  NAS on Proxmox: /mnt/pve/NAS/QuietRabbitMirror/  (different mount, same NAS)

Services:
  Ollama API:  http://192.168.88.26:11434  (Garuda, ethernet)
  QR App LXC:  http://192.168.88.95:3000
  Portainer:   https://192.168.88.95:9443

Docker volume mapping (in docker-compose.yml):
  QR_DATA_ROOT inside container = /data/quietrabbit
  Maps to NAS path via QR_LOCAL_DATA env var in .env

Environment flags:
  QR_NETWORK_STORAGE=true  (NAS mount — rollback journal, not WAL)
  QR_ENV=development       <- NEVER in production (logs personal field values)

Dev venv: ~/.venvs/quietrabbit/ (Garuda local — NAS has noexec, .so files won't run)
  Activate: source ~/.venvs/quietrabbit/bin/activate.fish
Verify sqlcipher3: python -c "from sqlcipher3 import dbapi2 as sqlite3; print('OK')"
Claude Code: always launch from /mnt/NAS/QuietRabbitMirror/06_GitRepos/quietrabbit-core/

## Naming Architecture (Phase A rename — D6-224, D6-225)
All legacy terms retired. Use only the new terms below.

| Old term         | New term         | Notes                              |
|------------------|------------------|------------------------------------|
| Space            | Life             | DB table: lives                    |
| space_id         | life_id          | Column and parameter name          |
| Path             | Focus            | File extension: .focus             |
| path_id          | focus_id         | Column and parameter name          |
| path_run         | focus_run        | DB table: focus_runs               |
| path_run_id      | focus_run_id     | Column and parameter name          |
| path_run_snapshot| focus_run_snap   | DB table: focus_run_snapshots      |
| Specialist       | Guide / Operator | .guide (community), .operator (sys)|
| specialist_id    | source_id        | personal_fields, voice_profiles    |
| quick_draft      | quick_ask        | output_type and focus_id           |
| paths/ dir       | focuses/ dir     | core_artifacts/focuses/            |
| specialists/ dir | guides/ + ops/   | core_artifacts/guides/ + operators/|
| PathRun class    | FocusRun class   | conductor/lifecycle.py             |
| PathDefinition   | FocusDefinition  | conductor/lifecycle.py             |
| path_context     | focus_context    | SYSTEM_TOKENS in tokens.py         |
| space_context    | life_context     | SYSTEM_TOKENS in tokens.py         |

Dev constants (routes.py, interview.py):
  _DEV_SPACE_ID = "dev-space"  →  _DEV_LIFE_ID = "dev-life"
  QR_INTERVIEW_SPACE_ID        →  QR_INTERVIEW_LIFE_ID
  SOURCE_ID = "personal-specialist"  (was SPECIALIST_ID)

## Phase 1 Focuses (8 confirmed)
1. Writing Assistant  2. Research & Buy  3. Job Match  4. Tech Support
5. Travel/Vacation Planning  6. Cooking  7. Personal Finance  8. Quick Ask

Quick Ask: full focus (Layer 3), output_type=quick_ask,
  suggest_in_focuses: [writing-assistant, job-match, research-and-buy]
Layer 7 build order: Travel after Job Match and Research & Buy.

## Key Architectural Decisions
- execution_tier = min(life_max_permitted_tier, focus_max_routing_tier, step.routing_tier)
  raw_abstraction = min(life_privacy_default_tier, execution_tier)
  abstraction_tier = max(2, raw_abstraction) if execution_tier > 1 else raw_abstraction
  life_max_permitted_tier is the hard ceiling. life_privacy_default_tier is preference.
  Never conflate them — they are distinct concepts.
- PersonalTrack NEVER serialized to focus_run_snapshots — re-fetched fresh on resume
- Step 6: PG_GATE_1 writes approved/abstracted fields to step_disclosure_buffer
  Step 8: reads from step_disclosure_buffer for Tier 2+ (NEVER from PersonalTrack)
  Raw personal values never appear in external prompt strings.
- Tier 3 is a terminal boundary — execution loop breaks, status=awaiting_user,
  thread released. Never a synchronous inline call.
- auth_enabled=1 ONLY if ALL databases migrate successfully. Partial = rollback.
- focus_run created as status=initializing, promoted to running after Phase 3 only
- Recovery key: BIP39 mnemonic of full 32 bytes. mnemo.to_entropy() gives exact
  original key. No wrapping, no random suffix, no stored blob.
- Release 1/Release 2 = product roadmap. Phase 1-7 = Conductor lifecycle phases.
- SYSTEM_TOKENS frozenset defined in conductor/tokens.py
- open_db() wrapper in providers/utils.py — explicit close. Use everywhere.
  sqlite3 context manager handles transactions only, not connection lifecycle.
- ContextCompactor makes direct local model call. No Librarian, no routing.
  Avoids Conductor<->Librarian circular dependency.
- is_fast_lane boolean on focus_runs — Phase 1 capture only.
  Phase 2 uses for "Promote to focus" Focus Builder pre-population.
- Guide versions: _load_guide_versions() — artifact_type='guide', per focus steps.
  Operator versions: _load_operator_versions() — artifact_type='operator', always loaded.
  Two separate queries by design — guides and operators need different handling in future.

## Layer Build Order
0 Foundation -> 1 Evaluation -> 2 Skeleton -> 3 Quick Ask -> 4 Privacy Guardian
-> 5 Personal Specialist -> 6 Writing Assistant -> 7 Remaining focuses -> 8 Auth
-> 9 Focus Optimizer

## Validation Workflow (required before locking any layer)
This chat generates the artifact AND reconciles external review findings.
Gemini and ChatGPT are adversarial reviewers only — no project context shared.
Classify every finding: Accepted / Rejected / Deferred / Requires empirical validation.
Reviewer agreement != correctness. Shared training data = shared blind spots.
Save validation records to /mnt/NAS/QuietRabbitMirror/05_AI_Validation/

## Critical Bug Patterns (found in integration review — do not repeat)
- PRAGMA key must be set BEFORE journal_mode in encrypted openers. open_db() is for unencrypted only.
- Never use executescript() in migrations — implicit COMMIT breaks SAVEPOINT atomicity. Use individual execute() calls.
- Always use open_db() wrapper — never raw sqlite3. Raw sqlite3 sets wrong journal mode on NAS.
- TaskStep.content = model output ONLY. Never prompt-expanded personal context. StepExecutor must enforce this. (D5-040)
- _bootstrap_lock_table() before acquire_lock() in run_migrations(). Wrapped in SAVEPOINT. (D5-056, D5-062)
- Fail fast at module load if QR_ENV=development and QR_DEV_KEY_HEX not set. (D5-063)
- Voice profile VALUES must be validated at write time — reject + warn if PII detected. (D5-151)
  ALLOWED_VOICE_ATTRIBUTES protects keys. Values are NOT protected — validate both.
  Plain-language warning required at point of input, not just a log entry.
- Floor consent preference must be scoped: abstraction_tier + consent_timestamp. (D5-152)
  Never store as life-wide blanket consent. Future focuses/providers require fresh consent.
- not_permitted enforces at Tier 2+ only. Raw values permitted at Tier 1. (D5-093, ADR-012 Amendment 1)
  ARCHITECTURE.md not_permitted section is stale — ADR-012 is authoritative on this.

## Schema Authoring Rule
Do not use semicolons inside string literals in .sql files (e.g. DEFAULT 'foo;bar').
_parse_statements() is not a general-purpose SQL parser — semicolons in string literals
will be split incorrectly and cause migration failures.

## Prompt Authoring Rule
_render_prompt() uses str.replace() for {token} substitution. Prompt templates must
not use {token_name} syntax for any string not intended as a Conductor token.
JSON examples or code blocks with literal braces matching a token name will be mangled.

## NotebookLM Bugs (fix before any layer ships)
- Clipboard leakage: sensitivity > 2 -> Manual Copy UI, never system clipboard
- Token estimation: 20% safety buffer when task_type is 'code' or 'research'
- PG_GATE_2 latency: full response classified before any display to user
- Ollama NDJSON stream: parse final line for {"status":"success"} before confirming
