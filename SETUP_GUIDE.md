# Quiet Rabbit — Setup Guide

**Your personal AI. Simple to start, built to grow, always yours.**

---

## Status

Phase 1 is in active development. The installer is not yet available.

Quiet Rabbit works on Linux only for now. Windows and macOS are planned for the first
release but do not work yet.

This guide will be updated with full installation instructions when Phase 1 is ready.
Watch this repo or check https://quietrabbit.ai for updates.

---

## What to Expect

Quiet Rabbit is a desktop application — download, install, and the interactive
onboarding guides you through everything. No technical expertise required.
No manual configuration.

Quiet Rabbit runs its own bundled Ollama on a dedicated local port and never connects to
an Ollama you installed yourself, so the two do not conflict. Models you already pulled
are not shared.

Quiet Rabbit will:
- Detect your hardware and available models automatically
- Recommend a model configuration based on what it finds
- Guide you through first-time setup interactively
- Run fully on your own hardware — no data leaves your machine by default

**Requirements (coming Phase 1):**
- Linux (x86_64) for now; Windows and macOS are planned for the first release but do not work yet
- 8GB RAM minimum (16GB recommended)
- GPU optional — NVIDIA and AMD GPUs are detected and used only where Ollama itself supports
  the card (some AMD cards are not); otherwise Quiet Rabbit runs on the CPU
- ~10GB disk space for models and data

---

## Privacy Model

Quiet Rabbit routes work by how far it leaves your device, and you choose at every step:

- **Tier 1 — Local Ollama:** runs on your hardware, fully private; set a Focus's ceiling to "Local only" to keep it here
- **Tier 1.5 — Faster hosted inference (opt-in):** the same class of open-weight model as
  Tier 1, run on a hosted provider's faster hardware when your own is the bottleneck. You pick
  the provider from a curated list that discloses what each one keeps and trains on. Requires an
  account, so it's not anonymous — never automatic, never default, always your explicit choice.
- **Cloud Chat, anonymous (split-screen):** an alternative that needs no sign-in (Duck.ai
  today). Quiet Rabbit prepares your context; you paste it in and paste the response back.
  Stronger models than Tier 1.5, no account needed.
- **Cloud Chat, with your own account (split-screen):** Claude, ChatGPT, or Gemini for final
  validation. Quiet Rabbit generates a chat starter from your history, and you paste it in
  through a split-screen view — always optional, always explicit. A direct in-app connection to
  a hosted provider you choose (API Chat) is planned for the first release and will also be
  opt-in.

For a sensitive Focus, set its ceiling (Maximum permitted tier) to "Local only" and Quiet Rabbit runs its AI steps for it on your machine.
Every external service interaction asks before acting.

---

## Questions or Issues

- GitHub: https://github.com/quietrabbitai/quietrabbit
- Website: https://quietrabbit.ai
- Contact: hello@quietrabbit.ai
