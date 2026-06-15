# Quiet Rabbit

### Your personal AI. Built to grow, always yours.

Quiet Rabbit is a self-hosted personal AI platform that keeps every part of your life
exactly where it belongs — organized into separate Personas, each with its own Focuses,
memory, privacy settings, and team of AI Guides. Each Persona stays completely separate.

It runs on your own hardware. Installs with one command. Gets smarter the more you use it.
And it never needs you to become an AI expert to get value from it.

---

## Why Quiet Rabbit Exists

Two things that actually happened:

After fixing a computer issue, Gemini casually referenced an upcoming mother-daughter trip
and offered to build a reading list for the journey. Helpful, maybe. Deeply unsettling, definitely.
That's context bleed — your AI knowing too much across domains you never meant to connect.

At the same time: business writing, college planning, yard sign design, and technical support
all crammed into one AI project with no separation, no organization, and no way to keep them
apart. That's context imprisonment — your AI knowing too little within the domains that matter.

Quiet Rabbit solves both. Your contexts stay separated. Nothing crosses between them without
your explicit permission. And within each Focus, your AI knows exactly what it needs to.

---

## How It Works

**Personas** are isolated contexts for different parts of your life — Work, Personal, Medical,
Legal. Each has its own Focuses, Guides, personal context, and privacy settings.
Nothing crosses between them without your permission.

**Focuses** are structured tasks run by a team of AI Guides. Job hunting, product research,
tech support, writing — each Focus assembles exactly the right Guides for the job and routes
them through the right models automatically.

**Guides** are AI team members with specific roles and expertise — assembled for each Focus,
invisible when not needed. Your cooking Guide knows your dietary preferences. Your writing
Guide knows your voice.

**What QR knows about you** is the personal context Quiet Rabbit holds within each Persona —
injected automatically so you never have to repeat yourself. Stays on your device. Never
shared across Personas. Never exported without your permission.

**Quick Ask** is an ephemeral single-session interaction — no memory, no tracking, just an
answer. Start a Quick Ask, or create a Topic to track it.

---

## Getting Started

Guided setup gets everything running — Quiet Rabbit detects your hardware, recommends a
model configuration, and walks you through first-time setup interactively. No manual
configuration required.

```bash
docker compose up
```

Requires: Docker Desktop (Windows, Mac, Linux)
GPU acceleration: automatic if available (NVIDIA or AMD)
No GPU: runs on CPU — slower but fully private and functional

---

## Built-in Focuses

| Focus | What it does |
|---|---|
| ✍️ Writing Assistant | Business writing, personal correspondence, any format |
| 🛍️ Research & Buy | Requirements → research → pricing and where to buy |
| 💼 Job Match | Analyze a posting, match your resume, draft a cover letter |
| 🖥️ Tech Support | Computer and homelab troubleshooting, step-by-step |
| 🍳 Cooking | Recipe research, meal planning, nutrition |
| ✈️ Travel & Vacation | Trip research, itinerary, packing guidance |
| 💰 Personal Finance | Budgeting, spending analysis — local only, always private |
| ⚡ Quick Ask | Fast single-stage output — no friction, no tracking |

---

## Privacy Model

- **Local inference by default** — Ollama runs on your hardware, included in the install
- **Three-tier routing** — Local Ollama → configurable API (Mistral/Groq) → Cloud review (your choice)
- **Sensitive Personas stay local** — Medical, Legal, Finance never leave your device
- **No telemetry** — Quiet Rabbit never sends usage data anywhere
- **Transparent always** — every action that touches external services asks first

---

## Philosophy

Quiet Rabbit is built around one idea: your AI should fit your life, not the other way around.

- Simple to start — one command, guided setup, no expertise required
- Built to grow — Focuses, Guides, and Personas expand at your pace
- Always yours — your hardware, your data, your control
- Self-improving — surfaces suggestions, never acts without your approval

---

## License

Business Source License 1.1 — free for personal use (≤5 household users).
Commercial use requires a license. Contact: hello@quietrabbit.ai

After four years each version converts to Apache 2.0.

See LICENSE for full terms.

---

## Status

Phase 1 in active development — Docker install, web UI, full Focus library.

Not ready for public use yet. Watch this repo for updates.

---

## Links

- Website: https://quietrabbit.ai
- GitHub: https://github.com/quietrabbitai/quietrabbit
- Community: https://github.com/quietrabbitai/community *(coming Phase 2)*
- Contact: hello@quietrabbit.ai
