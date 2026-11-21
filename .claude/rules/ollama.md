---
paths:
  - "src-tauri/src/ollama_*.rs"
  - "src-tauri/src/main.rs"
  - "src-tauri/binaries/**"
  - "src-tauri/deny.toml"
---

## Dev Environment

Services:
  Ollama: QR starts its own sidecar on 127.0.0.1:21434 (see "Ollama" below).
  Never point QR at another Ollama. Use localhost, never a LAN address
  (DHCP-assigned, changes). Manual experiments against some other Ollama are
  run by hand and are never configured into QR.

## Ollama (decisions.id=840, 863)
QR always runs its own bundled Ollama sidecar and never uses a user-installed
Ollama (decisions.id=840). No detect-and-reuse: a user's own Ollama is invisible
to QR's model registry, Focus eligibility and Cloud Chat, and the sidecar uses
its own models directory (QR_DATA_ROOT/ollama_models). Port 11434 is probed
only to log a GPU/RAM contention warning; it never gates or redirects the
sidecar.

Built today (ollama_sidecar.rs, ollama_ownership.rs; items.id=668):
- Port: the sidecar prefers 127.0.0.1:21434 and falls back through 21435-21443
  in order (PORT_CANDIDATES), no manual port setting. Each attempt spawns with
  OLLAMA_HOST=127.0.0.1:{port} and polls /api/tags only while its own child is
  alive; a child that exits means the port is not ours, so the next is tried,
  and a 2xx that arrives after the child died is rejected. Only an EXIT moves
  to the next port: a child still alive after the 5 s window without answering
  (a slow start, e.g. GPU discovery) is stopped (provable members only), no
  other port is tried for that binary, and the next candidate binary is tried.
  A child that is alive but not provably QR's is never waited on indefinitely:
  it is left alone and its record is kept. All ten ports failing gives
  Unavailable and an error in the log naming the ports tried; nothing is killed
  and no notice is shown (a user-facing notice is a Chat-BRAND design item).
  The winning port is ACTIVE_PORT, published (Release) before the trust flag and
  read through ollama_sidecar::port(). The port list is a field on
  OllamaSidecar so tests inject their own; no test binds 21434-21443.
- Name: the bundled binary is qr-ollama (externalBin binaries/qr-ollama, stub
  src-tauri/binaries/qr-ollama-<triple>). The debug-only PATH fallback still
  launches the system `ollama` (items.id=583 retires it).
- Ownership proof: QR signals a process only if it is a live, non-zombie member
  of a process group QR recorded itself AND its exe file name is qr-ollama or
  llama-server (plus `ollama` in debug builds only); an unreadable exe() is not
  provable and is never signalled. Nothing matched by command-line argument or
  environment alone is ever signalled, and the broad sweep over all processes is
  gone. The group is recorded at spawn, before readiness, in
  QR_DATA_ROOT/ollama_sidecar.pid, a JSON array of {pid, start_time, pgid,
  owner_pid, owner_start_time} (the old single-object format is ignored). Reclaim
  skips a group whose owner QR is alive, drops a record whose leader pid was
  reused (start_time differs), and for a live leader also requires the exe name,
  OLLAMA_MODELS equal to models_dir (env is an extra check, never sufficient) and
  is_orphaned; a headless group (leader gone, runner left by an abrupt stop) is
  reclaimed by member name. stop() and liveness_tick() use the same
  kill_process_group and keep the record if a provable member survives. The
  residual pid-reuse risk is documented in ollama_ownership.rs.
- Startup reclaim (items.id=586) therefore touches only recorded groups.
- No cloud (items.id=848): every launch sets OLLAMA_NO_CLOUD=1 (sidecar_env() in
  ollama_sidecar.rs, with OLLAMA_HOST and OLLAMA_MODELS). Without it Ollama 0.40.13
  fetches its model-recommendations list from ollama.com at startup and every 4 h,
  which left a TCP connection to ollama.com open (about 90 s per refresh in the
  captures; items.id=848 first saw it last a whole 13 min run), with no user
  action, and read the user's ~/.ollama/id_ed25519 while doing so. With it:
  no non-loopback connection and no key read in idle, model load or chat. The
  variable is not on the inherited allowlist, so a user's own value cannot reach or
  override it; a user's ~/.ollama/server.json can only add to it (HOME is inherited).
  Verified on 0.40.13 only; the packaged binary's version is unknown, so
  items.id=583/619 must assert the variable and the capture on it.
OllamaClient::base_url() is always http://127.0.0.1:{ollama_sidecar::port()}
and never reads OLLAMA_HOST or OLLAMA_PORT (items.id=616); take the port from
ollama_sidecar::port(), never a literal. The sidecar does not inherit QR's
environment: ollama_spawn_thread.rs clears it and passes only the INHERITED_ENV
allowlist (PATH, HOME, locale/XDG, LD_LIBRARY_PATH, the CUDA/ROCm/HIP/HSA/Vulkan
device-selection variables, HTTPS_PROXY/NO_PROXY/SSL_CERT_*) plus QR's own
OLLAMA_HOST and OLLAMA_MODELS. No user OLLAMA_* variable reaches the sidecar in
R1, so a user's OLLAMA_VULKAN or OLLAMA_IGPU_ENABLE has no effect on it until
QR has its own setting. wait_for_ready counts a 2xx only if the sidecar child
is still running when it arrives. Tests aim OllamaClient at a mock listener with
ollama_sidecar::force_port_for_test / ForcedPortGuard, not the environment.

Single instance (decisions.id=863, items.id=667), built in main.rs: a second
launch raises the first window and exits. tauri-plugin-single-instance is pinned
=2.4.5 (2.5.x needs tauri 2.12) and registered first in the Tauri builder,
release builds only (cfg(not(debug_assertions)), no override in a shipped
build), after doc_parse::dispatch_parse_mode and dispatch_cef_subprocess; a
test in main.rs pins that order. It initialises inside `.build()`, so the
second process starts no CEF, data-directory work, sidecar, reclaim or pidfile
record. Callback: raise_main_window shows the main window, unminimizes it only if
minimized, focuses it, and logs and returns if the window does not exist yet.
Limits, accepted: the lock is a name on the D-Bus session bus (Linux), derived
from the identifier, so it is per login session, ignores HOME/XDG, and does
not cover two OS users or a separate bus; with no usable session bus it fails
open and a second instance starts, silently; a malformed
DBUS_SESSION_BUS_ADDRESS panics plugin 2.4.5 at launch. Wayland may only flag
the window as demanding attention. Windows and macOS are unverified
(items.id=700, 701). deny.toml ignores RUSTSEC-2026-0370/0371/0372 (zbus 5 pin).

Related and still open: the bundled binary is a stub and the runner layout of a
packaged build is unverified, so the member name list in ollama_ownership.rs
must be revisited there (items.id=583, ready). Where the guard does not apply
(debug builds, no session bus, another OS user or bus) a second QR instance on
the same data directory starts a second sidecar on the next free port and its
record is kept apart from the first's (neither is signalled).
