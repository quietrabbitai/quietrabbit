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

- Rust (stable) and the Tauri CLI (`cargo install tauri-cli`)
- Node.js and npm (the frontend dev server starts automatically)
- Linux system libraries: see the "Install Linux dependencies" step in `.github/workflows/tauri-ci.yml`
- SQLCipher built with FTS5 (`sqlcipher` on Arch; Ubuntu's apt package lacks FTS5, so CI builds v4.14.0 from source)

Linux is the verified development platform (CI runs on Ubuntu).

```bash
git clone https://github.com/quietrabbitai/quietrabbit.git
cd quietrabbit/frontend && npm install
cd ../src-tauri && cargo tauri dev
```

Before opening a pull request, all of these must pass from `src-tauri/`:
`cargo build`, `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo deny check`, and `cargo test` (plain, not `--lib`).

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
