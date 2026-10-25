# Quiet Rabbit

### Your personal AI. Built to grow, always yours.

Quiet Rabbit is a self-hosted personal AI platform that keeps every part of your life
exactly where it belongs — organized into separate Personas, each with its own Focuses,
memory, privacy settings, and team of AI Guides. Each Persona stays completely separate.

It runs on your own hardware and will install like any other desktop app, with no Docker
and no server setup. Gets smarter the more you use it.
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

**Chat** is where you talk to Quiet Rabbit. Each chat belongs to one Persona and is filed
into History once its first reply is saved; starting a new chat never deletes the old one.
**Cloud Chat** is a split-screen view for taking a question to an outside service such as
Duck.ai, Claude, ChatGPT or Gemini; Quiet Rabbit asks before anything leaves your device.

---

## Getting Started

Quiet Rabbit is a desktop application — download, install, and the interactive
onboarding handles the rest. No Docker, no server setup, no technical expertise required.

Quiet Rabbit runs its own bundled Ollama on a dedicated local port and never connects to
an Ollama you installed yourself, so the two do not conflict. Models you already pulled
are not shared.

*The installer is not yet available. Phase 1 is in active development — watch
this repo or https://quietrabbit.ai for updates.*

Quiet Rabbit works on Linux only for now. Windows and macOS are planned for the first
release but do not work yet.

---

## Built-in Focuses

Quiet Rabbit is designed to ship with a small core set of Focuses, writing first, with more you can switch on. Each one runs structured, context-aware workflows that improve the more you use them.

The full Focus list will be published when the first release is ready.

---

## Privacy Model

- **Local inference by default** — Ollama runs on your hardware, bundled in the install
- **Your choice at every step** — on-device processing is the default; faster hosted
  inference and Cloud Chat (a split-screen view for outside services) are opt-in, never
  automatic
- **You set the ceiling for each Focus** — set a Focus to "Local only" and Quiet Rabbit runs its AI steps on your own hardware
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
Focus library.

Not ready for public use yet. Watch this repo for updates.

---

## Links

- Website: https://quietrabbit.ai
- GitHub: https://github.com/quietrabbitai/quietrabbit
- Community: https://github.com/quietrabbitai/community *(coming Phase 2)*
- Contact: hello@quietrabbit.ai
