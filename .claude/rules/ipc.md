---
paths:
  - "src-tauri/src/commands/**"
  - "src-tauri/src/ipc.rs"
  - "frontend/src/bindings.ts"
---

## Rust/Tauri Architecture

### Tauri IPC command conventions
IPC surface defined in HANDOFF_IPC_SURFACE.md — read before building any IPC layer.
Command count grows regularly (20 groups as of 2026-08-29) -- do not rely on a
stated number here; count `commands::` entries in src-tauri/src/ipc.rs's
specta_builder() for the current live surface.
All command structs derive Serialize, Deserialize, specta::Type.
TypeScript types via tauri-specta (2.0.0-rc.25).
Run type export after any command struct change.
Raw PersonalTrack values never cross into IPC response layer.
Tauri event listeners must be explicitly detached on SPA view unmount.
