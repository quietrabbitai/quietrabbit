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

Quiet Rabbit uses a three-tier routing system:

- **Tier 1 — Local Ollama:** runs on your hardware, fully private, default for sensitive Personas
- **Tier 2 — Configurable API:** faster inference for general Focuses, user-selectable provider
  (Mistral recommended for privacy-conscious users; Groq available as a free-tier option)
- **Tier 3 — Cloud review:** Claude, ChatGPT, or Gemini for final validation — always optional, always explicit

Sensitive Personas (Medical, Legal, Finance) never leave Tier 1.
Every external service interaction asks before acting.

---

## Questions or Issues

- GitHub: https://github.com/quietrabbitai/quietrabbit
- Website: https://quietrabbit.ai
- Contact: hello@quietrabbit.ai
