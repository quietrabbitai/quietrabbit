# Contributing to Quiet Rabbit

### Your personal AI. Simple to start, built to grow, always yours.

Thank you for your interest in contributing.

## CLA Required
All contributors must sign the Quiet Rabbit CLA before first PR is merged.
Handled automatically via CLA Assistant at cla-assistant.io.

## What We're Looking For
- Bug fixes
- Community paths (.path files) and specialists (.specialist files)
- Documentation improvements
- Hardware testing (especially non-NVIDIA GPU setups)
- New built-in paths
- UI improvements

## Development Setup

Requirements: Docker Desktop (Windows, Mac, or Linux)

```bash
git clone https://github.com/quietrabbitai/quietrabbit.git
cd quietrabbit
cp .env.example .env  # add your API keys
docker compose up
```

Open http://localhost:5000 (Phase 1 UI) or http://localhost:3000 (Open WebUI).

For the Conductor engine directly (advanced):
```bash
python3 -m venv venv && source venv/bin/activate
pip install crewai crewai-tools litellm openai python-dotenv
python3 conductor/conductor.py --profile quick-draft --no-browser
```

## Terminology

Use the correct terms in all contributions:
- **Space** (not Persona, not Workspace)
- **Path** (not Mission, not Workflow)
- **Specialist** (not Agent, not LLM)
- **Personal Specialist** (not Memory, not Knowledge base)
- **Insights** (not Tracker, not Metrics)

## Community Conduct
Be direct, be kind, assume good intent.
GitHub Discussions: https://github.com/quietrabbitai/quietrabbit/discussions
Contact: quietrabbit.ai@gmail.com
