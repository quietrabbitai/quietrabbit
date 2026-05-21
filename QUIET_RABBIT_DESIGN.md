# Quiet Rabbit — Product Design Specification

**Tagline:** Your personal AI. Simple to start, built to grow, always yours.
**Engine:** Conductor | **Version:** 0.1 | **Status:** Active development
**Updated:** May 20, 2026 — Palette v3 locked. Session 2 idea decisions incorporated.

---

## Origin Story (Use in README and marketing)

Two real experiences drove this project:

1. **Context bleed (Gemini):** After fixing a computer issue, Gemini referenced an upcoming mother-daughter trip and offered to build a reading list for it. A privacy violation hiding in helpfulness.

2. **Context imprisonment (Claude projects):** Business writing, college planning, yard sign design, and technical support all crammed into one project with no separation. The tool that should help creates its own chaos.

Quiet Rabbit solves both: your contexts stay separated, and nothing crosses between them without your explicit permission.

---

## Core Tenets (Non-Negotiable)

1. **Privacy by default** — local inference is the baseline, not the premium tier
2. **Simplicity first** — detect, recommend, confirm. Never burden the user with research
3. **Transparent by design** — no silent actions, no hidden agents, no surprises
4. **Human in the loop** — all improvements require explicit approval
5. **Self-building / self-healing / self-evolving** — the system helps itself improve, always with user approval

---

## Development Philosophy — Detect, Recommend, Confirm

Applied everywhere, not just install:
- The system detects what it can (hardware, usage patterns, quality drift)
- Presents a clear recommendation in plain language
- Asks for confirmation before acting
- Technical users can always ask "what are my limiting factors?" for a deeper conversation
- Hardware context is silent infrastructure — surfaces only on request or when a boundary is hit

---

## Vocabulary — Human Terms Only

| Term | Definition | Never Say | Status |
|---|---|---|---|
| **Space** | A distinct context for a part of your life | Workspace, profile, persona | ✅ LOCKED — replaces Persona |
| **Persona** | RETIRED | — | ❌ Do not use |
| **Path** | A specific task run through a team of specialists | Mission, workflow, pipeline | ✅ Locked |
| **Specialist** | An AI team member with a specific role | Agent, LLM | ✅ Locked |
| **Personal Specialist** | Holds persistent context about the user within a space | Memory, knowledge base, RAG | ✅ Replaces Memory |
| **Memory** | RETIRED — replaced by Personal Specialist | — | ❌ Do not use |
| **Insights** | Quality tracking and improvement suggestions | Tracker, metrics | ✅ Locked |
| **Community** | Shared path and specialist library | Repository, registry | ✅ Locked |
| **Review** | Final validation with Claude, ChatGPT, or Gemini | Validator | ✅ Locked |
| **Conductor** | The engine that runs paths | Pipeline, orchestrator | ✅ Locked |
| **Quiet Rabbit** | The product | AI Conductor, Prism | ✅ Locked |

---

## Key Design Decisions (Locked)

- Name: Quiet Rabbit | Engine: Conductor
- Tagline: "Your personal AI. Simple to start, built to grow, always yours."
- Logo: Geometric rabbit ear mark — two upright ears, amber inner ears, circular head, amber nose
- License: BSL 1.1 (free personal ≤5 users, paid commercial)
- Domain: quietrabbit.ai (registered May 18 2026, Cloudflare)
- Contact: quietrabbit.ai@gmail.com
- Distribution: self-hosted, Docker-first, GitHub first
- Install: docker compose up — single command, cross-platform
- Ollama: core component, packaged in compose stack — NOT optional
- Groq: built-in for development — Tier 2 provider is user-configurable at launch
- Three-tier model routing: Local Ollama → Tier 2 (user-configurable) → Cloud review (all load-bearing)
- Model selection: always invisible to user — system decides based on task and space
- Primary audience: frustrated single-AI user — not homelab enthusiast
- Human approval required for ALL auto-improvements
- Solo operator scope lock: Phase 1 is single-user only
- Self-building / self-healing / self-evolving as core philosophy (not a feature)
- Computer-first UI, phone as companion (responsive web, no app required)
- Mobile remote access via Tailscale — no App Store required
- Space is the confirmed term (not Persona — locked)
- Semantic versioning and automated changelog from day one
- Architecture must never foreclose: future premium API tier, user-supplied API keys, app integrations, async communication, hardware appliance

---

## Architecture — Three-Tier Model Routing (All Tiers Load-Bearing)

Tier 1: Local Ollama — private, offline, always available. Default for sensitive spaces.
         Docker compose packages Ollama + Quiet Rabbit together.
         Install interview detects hardware, recommends model configuration automatically.
         Models scale with hardware — user confirms, doesn't research.

Tier 2: User-configurable API — faster, general paths, middle-ground privacy.
         Groq used during development. Mistral recommended default at public launch —
         European jurisdiction, GDPR-native, stronger privacy narrative for target audience.
         Grok (xAI) — NOT recommended, trust liability for privacy-conscious users.
         Monitor: Cerebras (fast, newer), Together AI (review privacy policies before recommending).
         Provider must be user-configurable from day one — NEVER hard-coded.

Tier 3: Cloud review — Claude/ChatGPT/Gemini, manual paste, optional final validation.
         Model-aware: cover letter review → ChatGPT, strategy → Claude, research → Gemini.
         User preference is the default routing; system uses validated model affinities.

Future (not Phase 1): direct in-app API access as premium tier.
Users may also supply own API keys. Architecture must not foreclose either.

---

## Install Philosophy — "Click to Free the Rabbit"

The installer opens with a single button: "Click to free the rabbit."
Everything from there is an interactive conversation — no technical wall at first run.

- Lightweight bootstrap model conducts the install conversation before full models are pulled
- Docker Compose backend handles the actual install
- Button launches browser to local UI once container is running
- Same graceful failure principles apply — plain language, specific, clear next step
- Do not over-engineer before the core product exists — revisit as part of development

Platforms: Windows, Mac, Linux via Docker Desktop
GPU: NVIDIA via nvidia-container-toolkit, AMD/ROCm supported, CPU fallback always available

---

## Space System (confirmed — "Space" is the locked term)

Start screen: "Which space are you in today?"
Inside a space: paths are things you do, specialists are invisible infrastructure.

### Space Color Assignments — Palette v3 — LOCKED May 2026

Color conflicts resolved. Amber is the mark accent only — no longer assigned to any space.
Teal (Forest) is the Medical space only — not a tier indicator color.
Coral (Ember) is warnings only — not assigned to any space.
Tier routing uses icons only — no tier-specific colors.

| Space | Color Name | Hex | Privacy Default |
|---|---|---|---|
| Work | Dusk blue | #2E5068 | Tier 2 (configurable) |
| Medical / Health | Forest | #386850 | Tier 1 (Local only) |
| Personal | Slate blue | #5A6E7F | Tier 2 (configurable) |
| Cooking | Forest | #386850 | Tier 2 (configurable) |
| Nature & Travel | Forest | #386850 | Tier 2 (configurable) |
| Homelab / Technical | Dusk blue | #2E5068 | Tier 2 (configurable) |
| Legal / Finance | Warm umber | #56482E | Tier 1 (Local only) |

Note: Multiple spaces may share a color. Color distinguishes space type/sensitivity,
not individual space identity. User-created spaces select from the palette.

---

## Brand Identity — Palette v3 — LOCKED May 2026

### Logo
Geometric rabbit ear mark — two upright ears with amber inner ears, circular head, amber nose.
- Light backgrounds: Ink #242C2A ears/head, Amber #C48B1A inner ear, Amber #D4A030 nose
- Dark backgrounds: Sage mist #E6E9E2 ears/head, Amber #C48B1A inner ear, Amber #D4A030 nose
- Wordmark: "Quiet Rabbit" — DM Serif Display, two words, no abbreviation

### Color Palette (8 colors — locked)

| Name | Hex | Role |
|---|---|---|
| Sage mist | #E6E9E2 | Background |
| Ink | #242C2A | Body text, mark fill (light bg) |
| Amber | #C48B1A | Mark inner ear — LOCKED, mark use only |
| Amber light | #D4A030 | Mark nose — LOCKED, mark use only |
| Ember | #B04030 | Warnings and alerts ONLY — never space color |
| Dusk blue | #2E5068 | Nav background + Work + Homelab spaces |
| Forest | #386850 | Medical + Cooking + Nature spaces |
| Slate blue | #5A6E7F | Personal space |
| Warm umber | #56482E | Legal + Finance spaces |

### Tier Badge System
Tiers use icons only — Neutral badge, no tier-specific color.
- Tier 1 Local: shield-lock icon
- Tier 2 (configurable): bolt icon
- Tier 3 Cloud: cloud icon
- Warning/Review: alert-triangle icon, Ember #B04030 text

### Typography
- DM Serif Display — wordmark, display headings, pull quotes, tagline (italic)
- DM Sans 300/400/500 — body text, UI, labels, navigation

⚠️ Screen design ON HOLD pending Chat-DEV organizational structure decisions.
Logo: approved — revisit fine details before any public sharing.

---

## Phase 1 Paths (Locked — 6 paths)

| Path | Description | Primary Model Affinity |
|---|---|---|
| Writing Assistant | Business, personal, correspondence | Claude |
| Research & Buy | Product research, requirements, pricing | Gemini |
| Job Match | Posting analysis, salary research, cover letter | Claude (analysis) + ChatGPT (letters) |
| Tech Support | Computer and homelab troubleshooting | Gemini |
| Quick Draft | Fast single-stage, no friction, no review | Local 3b |
| Life Transitions | College, moves, major life planning | Tier 2 70B |

---

## Personal Specialist (replaces Memory System)

Protected system specialist holding persistent context about the user within a space.
Always injected at path start. User controls all entries — add, edit, remove.
Never shared across spaces. Never included in any export.
Staleness tracking with configurable reminders.

---

## Core Protected Specialists

| Specialist | Role | Visible to user |
|---|---|---|
| Path Builder | Interview-led path creation — no code required | ✅ Yes |
| Librarian | Find, import, organize community paths/specialists | ✅ Yes |
| Privacy Guardian | Plain-language alerts before any data leaves local | ✅ Always |
| Security Checker | Scans imports for prompt injection / unsafe instructions | ✅ Yes |
| Path Optimizer | Suggests improvements, shows what/why, requires approval | ✅ Yes |
| Support Specialist | Diagnoses issues, explains errors, guides troubleshooting | ✅ Yes |
| Personal Specialist | Holds persistent user context per space | ✅ Yes |

All core specialists are visible advisors — never silent background processes.

---

## Specialist Fidelity Tiers

| Tier | Name | Description |
|---|---|---|
| 1 | Prompt-defined | Well-known domains, common expertise. No external data. |
| 2 | Artifact-augmented | Requires user-supplied documents or records. |
| 3 | Expert-contributed | Requires real-world domain expert. Community library territory. |

## Specialist Creation Gate

Must pass before any specialist is created:
✅ Create when: distinct consistent expertise lens, domain-specific rules, reused across paths, user needs to trust who's working
❌ Don't create when: single capability any model handles (summarize, fix grammar), no persona context needed, won't be reused

---

## Async Communication — Phase 1 Architecture Constraint

Conversation is the primary surface. Everything else is ambient and user-initiated.
Phase 2 feature to build — but Phase 1 architecture must accommodate all three layers.

Three layers:
1. **Notification center** — path optimizer findings, scheduled path completions, model/infrastructure updates
2. **Breadcrumbs / conversation starters** — contextual, proactive but never intrusive
3. **Scheduled output inbox** — where timed path outputs land

Nothing pushes aggressively. Architecture must support all three from day one.

---

## App Integration — Phase 1 Architecture Constraint

Phase 2+ to build — Phase 1 architecture must not foreclose.

Read vs. write distinction is critical:
- **Read:** granted once, low risk — calendar, Notion, task lists
- **Write:** confirmed per action or per session, high risk — every write goes through
  Privacy Guardian and Security Checker with explicit plain-language confirmation before committing

Notion Quick Thought pull is strong first dogfooding candidate.
Note: At start of future Chat-IDEAS sessions, suggest pulling not-started Notion Quick Thoughts.

---

## Update and Release Process

GitHub releases as source of truth.
Notification center surfaces all update types inside the product.

Three update types:
- **Model updates:** Path Optimizer surfaces these
- **Docker / infrastructure:** GitHub releases + notification center
- **Community communications:** Librarian + notification center

Semantic versioning and automated changelog from day one.
Minimal manual intervention — self-building philosophy applies.

---

## Privacy Rules — Plain-Language Guidance

Every data-leaving event gets a plain-language, conversational alert.
"This step would share X with Groq. Confirm?" — not legal boilerplate.
Minimal friction on low-risk, hard stop on high-risk.
Privacy Guardian owns and enforces all rules.

---

## File Format — Three Separate Types

| Extension | Contains | Shareable |
|---|---|---|
| `.path` | Path definition — specialists, routing, steps | ✅ Yes |
| `.specialist` | Specialist definition — role, prompt, tier, constraints | ✅ Yes |
| `.space` | Space config — privacy settings, Personal Specialist structure | ⚠️ Private by default |

Personal Specialist data NEVER included in any export.
Security Checker runs on all imports before activation.

---

## Sharing and Community

Ring 1: direct file sharing — Phase 1
Ring 2: GitHub community repo github.com/quietrabbit/community — Phase 2
Ring 3: verified creators — Phase 3

---

## Multi-User

Phase 1: single user only — solo operator scope lock enforced by Chat-PM.
Phase 2: Flask-Login auth, multiple spaces per user, full user isolation.

---

## Mobile / Access

Computer-first workflow. Phone as companion for lightweight interactions.
Hub-and-spoke: Quiet Rabbit runs on home server, access via any browser.
Responsive web UI — phone-accessible without app.
Tailscale for remote access — already installed on Garuda.

---

## Website / Landing Page

Cloudflare Pages recommended — already managing DNS there, strong CDN, scales without pain.
Configure DNS baseline before any public sharing.
Static marketing page and docs can live in the same GitHub repo.
⚠️ Deploy landing page before any public sharing — Chat-DOCS task.

---

## Hardware Appliance — Phase 4+ Vision (Parked)

Three tiers: portable personal unit (most compelling — holds Personal Specialist locally,
connects to PC/phone, no cloud), home/prosumer unit, enterprise version.
Local-first architecture keeps hardware option open naturally.
Never foreclose with architectural decisions.

---

## Insights (Self-Improvement)

All suggestions require explicit user approval before any change.

| Threshold | Condition | Action |
|---|---|---|
| prompt_review_runs | 10 runs unchanged | Suggest path review |
| level2_rated_runs | 5 rated runs | Suggest saving examples |
| level4_finals | 20 finals saved | Suggest RAG setup |
| level5_high_rated | 60 high-rated runs | Suggest fine-tuning |
| quality_drift_pct | 35% avg diff | Suggest improvement |
| model_upgrade_pct | 40% avg diff | Suggest model tier upgrade |

---

## Build Roadmap

Phase 0 ✅: Core pipeline, Ollama/ROCm, Conductor, Open WebUI, NAS storage
Phase 1 (next): Single-user. Docker-first. 6 paths. Scope locked in Chat-DEV.
Phase 2: Multi-user, spaces, community library Ring 2, data integrations, app hooks, async communication
Phase 3: Packaged installer, Ring 3 community, premium API tier
Phase 4+: Hardware appliance (parked vision)

---

## Technical Stack

Python 3.11+ · CrewAI · litellm · Flask · Ollama (Docker)
Groq API (dev, built-in) · Mistral (recommended Tier 2 at launch) · BSL 1.1
Ollama models: llama3.2:3b (fast/UI), llama3.1:8b (primary), qwen2.5:7b (code)
⚠️ OLLAMA_NUM_CTX=2048 global — replace with per-model Modelfiles in Chat-DEV

---

## Privacy Commitments (Non-Negotiable)

1. No telemetry — Quiet Rabbit never sends usage data anywhere
2. Local-only spaces (Medical, Legal, Finance) — nothing leaves the device
3. Personal Specialist data never included in any export
4. Self-hosted — your hardware, your data, your control
5. Human approval required for all auto-improvements — no silent changes

---

## Chat Registry

| Code ID | Status |
|---|---|
| Chat-PM | Active — project manager |
| Chat-BRAND | Active — screens ON HOLD pending Chat-DEV |
| Chat-IP | Active — trademark deferred, .com monitoring |
| Chat-SETUP | ✅ Complete |
| Chat-IDEAS | ✅ Complete — Session 2 decisions incorporated |
| Chat-LEGAL | Active |
| Chat-GITHUB | 🔴 Open next |
| Chat-DEV | 🔴 Open after GitHub |
| Chat-STRATEGY | Planned |
| Chat-MARKETING | Planned |
| Chat-DOCS | Planned |
| Chat-TESTING | Planned |
| History-PRISM-TM | Archive |
| History-PRISM-UOGO | Archive |
| History-SETUP-DIP | Archive |
