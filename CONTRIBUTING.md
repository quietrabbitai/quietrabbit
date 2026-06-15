# Contributing to Quiet Rabbit

### Your personal AI. Built to grow, always yours.

Thank you for your interest in contributing.

## CLA Required
All contributors must sign the Quiet Rabbit CLA before first PR is merged.
Handled automatically via CLA Assistant at cla-assistant.io.

## What We're Looking For
- Bug fixes
- Community Focuses (.focus files) and Guides (.guide files)
- Documentation improvements
- Hardware testing (especially non-NVIDIA GPU setups)
- New built-in Focuses
- UI improvements

## Development Setup

Requirements: Docker Desktop (Windows, Mac, or Linux)

```bash
git clone https://github.com/quietrabbitai/quietrabbit.git
cd quietrabbit
cp .env.example .env  # add your API keys
docker compose up
```

Open http://localhost:5000 (Phase 1 UI).

## Terminology

Use the correct terms in all contributions:

| Use | Never say |
|---|---|
| **Persona** | Life, Space, Workspace, Profile |
| **Focus** | Path, Workflow, Pipeline |
| **Topic** | Plan, Project |
| **Action** | Task, Step |
| **Guide** | Specialist, Agent, Assistant |
| **Quick Ask** | Quick Draft |
| **Library** | Output store, Asset store |
| **Optimizer** | Path Optimizer |
| **.focus file** | .path file |
| **.guide file** | .specialist file |
| **.operator file** | .specialist file (system operators) |

## Community Conduct
Be direct, be kind, assume good intent.
GitHub Discussions: https://github.com/quietrabbitai/quietrabbit/discussions
Contact: hello@quietrabbit.ai
