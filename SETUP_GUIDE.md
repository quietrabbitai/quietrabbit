# Quiet Rabbit — Setup Guide

**Your personal AI. Simple to start, built to grow, always yours.**

---

## Status

Phase 1 is in active development. The installer is not yet available.

This guide will be updated with full installation instructions when Phase 1 is ready.
Watch this repo or check https://quietrabbit.ai for updates.

---

## What to Expect

Quiet Rabbit is a desktop application — download, install, and the interactive
onboarding guides you through everything. No technical expertise required.
No manual configuration.

**Ollama is bundled.** If you already have Ollama installed and running, Quiet Rabbit
detects it automatically and uses it — no duplicate model downloads, no conflicts.
If you don't have Ollama, Quiet Rabbit starts its own sidecar automatically.
Either way, you don't need to touch Ollama yourself.

Quiet Rabbit will:
- Detect your hardware and available models automatically
- Recommend a model configuration based on what it finds
- Guide you through first-time setup interactively
- Run fully on your own hardware — no data leaves your machine by default

**Requirements (coming Phase 1):**
- Windows, macOS, or Linux (x86_64)
- 8GB RAM minimum (16GB recommended)
- GPU optional — NVIDIA and AMD supported, CPU fallback always available
- ~10GB disk space for models and data

---

## Privacy Model

Quiet Rabbit uses a tiered routing system:

- **Tier 1 — Local Ollama:** runs on your hardware, fully private, default for sensitive Personas
- **Tier 1.5 — Faster hosted inference (opt-in):** same open-source model class as Tier 1, run on
  faster hosted hardware (e.g. Groq) when your own hardware is the bottleneck. Requires an
  account, so it's not anonymous — never automatic, never default, always your explicit choice
  per task.
- **Tier 2 — Private cloud review (split-screen):** an anonymous, no-retention alternative to
  Tier 3. Quiet Rabbit prepares your context; you paste it into a provider that doesn't require
  sign-in (e.g. Duck.ai, Brave Leo) and paste the response back. Stronger models than Tier 1.5,
  no account needed.
- **Tier 3 — Full cloud service:** Claude, ChatGPT, or Gemini for final validation. Quiet Rabbit
  generates a chat starter from your history, and you paste it in through a split-screen view —
  always optional, always explicit. A direct in-app API round-trip (skipping the copy/paste step)
  was evaluated and found technically workable, but doesn't yet have a clear case for this
  release — it's parked as a possible future paid option, not part of Phase 1.

Sensitive Personas (Medical, Legal, Finance) never leave Tier 1.
Every external service interaction asks before acting.

---

## Questions or Issues

- GitHub: https://github.com/quietrabbitai/quietrabbit
- Website: https://quietrabbit.ai
- Contact: hello@quietrabbit.ai
