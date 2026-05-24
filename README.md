# Quiet Rabbit

### Your personal AI. Simple to start, built to grow, always yours.

Quiet Rabbit is a self-hosted personal AI platform that keeps every part of your life
exactly where it belongs — organized into separate spaces, each with its own context,
privacy settings, and team of AI specialists. Each space stays completely separate.

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
your explicit permission. And within each space, your AI knows exactly what it needs to.

---

## How It Works

**Spaces** are isolated contexts for different parts of your life — Work, Medical, Personal,
Homelab, Legal. Each has its own paths, specialists, personal context, and privacy settings.
Nothing crosses between them.

**Paths** are structured tasks run by a team of AI specialists. Job hunting, product research,
tech support, writing — each path assembles exactly the right specialists for the job and routes
them through the right models automatically.

**Specialists** are AI team members with specific roles and expertise — assembled for each path,
invisible when not needed.

**Personal Specialist** holds what Quiet Rabbit knows about you in each space — injected
automatically so you never have to repeat yourself. Stays on your device. Never shared across
spaces. Never exported without your permission.

**Insights** tracks quality over time and surfaces improvement suggestions — always with your
approval before anything changes.

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

## Built-in Paths

| Path | What it does |
|---|---|
| ✍️ Writing Assistant | Business writing, personal correspondence, any format |
| 🛍️ Research & Buy | Requirements → research → pricing and where to buy |
| 💼 Job Match | Analyze a posting, match your resume, draft a cover letter |
| 🖥️ Tech Support | Computer and homelab troubleshooting, step-by-step |
| ⚡ Quick Draft | Fast single-stage output — no specialists, no friction |
| 🎓 Life Transitions | College, moves, and major life planning |

---

## Privacy Model

- **Local inference by default** — Ollama runs on your hardware, included in the install
- **Three-tier routing** — Local Ollama → configurable API (Mistral/Groq) → Cloud review (your choice)
- **Sensitive spaces stay local** — Medical, Legal, Finance never leave your device
- **No telemetry** — Quiet Rabbit never sends usage data anywhere
- **Transparent always** — every action that touches external services asks first

---

## Philosophy

Quiet Rabbit is built around one idea: your AI should fit your life, not the other way around.

- Simple to start — one command, guided setup, no expertise required
- Built to grow — paths, specialists, and spaces expand at your pace
- Always yours — your hardware, your data, your control
- Self-improving — surfaces suggestions, never acts without your approval

---

## License

Business Source License 1.1 — free for personal use (≤5 household users).
Commercial use requires a license. Contact: quietrabbit.ai@gmail.com

After four years each version converts to Apache 2.0.

See LICENSE for full terms.

---

## Status

Phase 0 complete — core pipeline running, local models working.
Phase 1 in active development — Docker install, web UI, full path library.

Not ready for public use yet. Watch this repo for updates.

---

## Links

- Website: https://quietrabbit.ai
- GitHub: https://github.com/quietrabbitai/quietrabbit
- Community: https://github.com/quietrabbitai/community *(coming Phase 2)*
- Contact: quietrabbit.ai@gmail.com
