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

Quiet Rabbit is a Tauri/Rust desktop app. No Docker or server is involved.

Requirements:

- Rust 1.97.1, pinned by `rust-toolchain.toml` (rustup selects it automatically), and the Tauri CLI (`cargo install tauri-cli`)
- Node.js and npm (the frontend dev server starts automatically)
- Linux system libraries: see the "Install Linux dependencies" step in `.github/workflows/tauri-ci.yml`
- SQLCipher built with FTS5 (`sqlcipher` on Arch; Ubuntu's apt package lacks FTS5, so CI builds v4.14.0 from source)

Quiet Rabbit works on Linux only for now, and Linux is the verified development platform (CI runs on Ubuntu). Windows and macOS are planned for the first release (items.id=608 to 611; their CI jobs are items.id=700).

```bash
git clone https://github.com/quietrabbitai/quietrabbit.git
cd quietrabbit/frontend && npm install
cd ../src-tauri && cargo tauri dev
```

Before opening a pull request, all of these must pass from `src-tauri/`:
`cargo build`, `cargo fmt --check`, `cargo clippy --tests -- -D warnings`, `cargo deny check`, and `cargo test` (plain, not `--lib`).
Frontend changes also need, from `frontend/`: `npm ci`, `npx tsc -b`, `npm test` and `npm run lint`.

## Terminology

Use the correct terms in all contributions:

| Use | Never say |
|---|---|
| **Persona** | Life, Space, Workspace, Profile |
| **Focus** | Path, Workflow, Pipeline |
| **Topic** | Plan, Project |
| **Action** | Task, Step |
| **Guide** | Specialist, Agent, Assistant |
| **Cloud Chat** | Tier 2 chat, Tier 3 chat |
| **Quick Ask** | Quick Draft |
| **Library** | Output store, Asset store |
| **Optimizer** | Path Optimizer |
| **.focus file** | .path file |
| **.guide file** | .specialist file |
| **.operator file** | .specialist file (system operators) |

Code identifiers `step_id` and `StepDefinition` predate the Task to Action rename, which is
tracked as items.id=552. Until it lands, match the existing identifiers when editing code, and
use Action in docs and UI text.

## Community Conduct
Be direct, be kind, assume good intent.
GitHub Discussions: https://github.com/quietrabbitai/quietrabbit/discussions
Contact: hello@quietrabbit.ai
