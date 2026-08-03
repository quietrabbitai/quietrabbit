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

Quiet Rabbit is a desktop application — download, install, and the interactive
onboarding handles the rest. No Docker, no server setup, no technical expertise required.

Ollama is included. If you already have Ollama running, Quiet Rabbit uses it automatically.
If not, it starts its own — no duplicate downloads, no conflicts.

*The installer is not yet available. Phase 1 is in active development — watch
this repo or https://quietrabbit.ai for updates.*

---

## Built-in Focuses

Quiet Rabbit ships with a set of built-in Focuses covering everyday pursuits — writing, research, cooking, travel, reading, and more. Each one runs structured, context-aware workflows that improve the more you use them.

The full Focus list will be published when the first release is ready.

---

## Privacy Model

- **Local inference by default** — Ollama runs on your hardware, bundled in the install
- **Tiered routing, your choice at every step** — local Ollama is the default; faster hosted
  inference and private split-screen cloud review are opt-in, never automatic; full cloud
  service is always optional and explicit
- **Sensitive Personas stay local** — Medical, Legal, Finance never leave your device
- **No telemetry** — Quiet Rabbit never sends usage data anywhere
- **Transparent always** — every action that touches external services asks first

---

## Philosophy

Quiet Rabbit is built around one idea: your AI should fit your life, not the other way around.

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

Phase 1 in active development — desktop app (Tauri/Rust), native Ollama integration,
full Focus library.

Not ready for public use yet. Watch this repo for updates.

---

## Links

- Website: https://quietrabbit.ai
- GitHub: https://github.com/quietrabbitai/quietrabbit
- Community: https://github.com/quietrabbitai/community *(coming Phase 2)*
- Contact: hello@quietrabbit.ai
