# Quiet Rabbit — Setup Guide

**Your personal AI. Simple to start, built to grow, always yours.**

---

## Status

Phase 1 is in active development. The standard Docker-based install is coming soon.

This guide will be updated with full installation instructions when Phase 1 is ready.
Watch this repo or check https://quietrabbit.ai for updates.

---

## What to Expect

Installation will begin with a single command:

```bash
docker compose up
```

From there, Quiet Rabbit guides you through everything with an interactive conversation.
No technical expertise required. No manual configuration.

Quiet Rabbit will:
- Detect your hardware automatically
- Recommend a model configuration based on what it finds
- Guide you through first-time setup interactively
- Run fully on your own hardware — ino data leaves your machine by default

**Requirements (coming Phase 1):**
- Docker Desktop (Windows, Mac, or Linux)
- 8GB RAM minimum (16GB recommended)
- GPU optional — NVIDIA yNVIDIA and AMD supported, CPU fallback always available
- ~10GB disk space for models and data

---

## Privacy Model

Quiet Rabbit uses a three-tier routing system:

- **Tier 1 — Local Ollama:** runs on your hardware, fully private, default for sensitive spaces
- **Tier 2 — Configurable API:** faster inference for general paths, user-selectable provider
  (Mistral recommended for privacy-conscious users; Groq available for development)
- **Tier 3 — Cloud review:** Claude, ChatGPT, or Gemini for final validation — always optional, always explicit

Sensitive spaces (Medical, Legal, Finance) never leave Tier 1.
Every external service interaction asks before acting.

---

## Questions or Issues

- GitHub: https://github.com/quietrabbitai/quietrabbit
- Website: https://quietrabbit.ai **(coming soon)**
- Contact: quietrabbit.ai@gmail.com
