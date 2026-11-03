# Verification rig

Small tools for checking, on a **scratch run of the debug app**, what the app and its embedded
browser store on disk and what they send over the network. Built for items.id=622 (Cloud Chat
profile and site data) and reused by 637, 619 and 848 (recipes at the end).

Everything here is for scratch runs only. Nothing in it reads or writes the real data directory.

## Requirements

- Node.js 22 or newer (the scripts use the built-in `WebSocket` and `fetch`; no npm install).
- Linux with `ss` (iproute2), `pgrep`, `du`, `df`, and `unshare` (only for `--ns`).
- A built debug app: `src-tauri/target/debug/quietrabbit` (or set `RIG_APP_BIN`).
- For the CDP scripts, the app launched with `--remote-debugging-port=N`; for the net-log parser, with
  `--log-net-log=PATH`. `scratch-run.sh` always passes the net-log flag and passes any other app
  arguments through, so `scratch-run.sh DIR -- --remote-debugging-port=9333` gives you both.

## Safety rules

1. **Scratch only.** `scratch-run.sh` is the only way to start the app from here. It sets
   `XDG_DATA_HOME`, `XDG_CACHE_HOME` and `XDG_CONFIG_HOME` to directories inside the scratch dir, unsets
   every `QR_*` and `OLLAMA_*` variable, and refuses to run if the scratch dir is, contains or sits
   inside your home directory, `~/.local/share`, `~/.cache` or `~/.config`, or if it is not new or empty.
   `QR_DATA_ROOT` does **not** isolate the data directory (a dev run once hit the real one); the XDG
   variables do. Do not launch the app any other way for these checks.
2. **XDG_RUNTIME_DIR is not isolated.** The GUI session needs it, so the app's CEF profile lives in
   `$XDG_RUNTIME_DIR/ai.quietrabbit.app/cef-<pid>-<random>` (a per-run directory the app deletes at a
   clean exit; one left behind by a crash can be removed once its pid is gone).
3. **Names and counts only.** The CDP scripts print cookie names, storage entry counts and origins;
   never values, paths or query strings. The canary data is synthetic (`qrrig_canary*`). The net-log
   parser strips query strings unless you pass `--full`.
4. **Delete the scratch dir afterwards.** A profile or net log can hold session cookies or URLs from
   whatever you did in the app. The scripts never delete anything; you do, with `rm -rf` on the
   scratch dir you created (check the path first).
5. Another QR instance may be running. A scratch run starts its own Ollama sidecar on the next free
   port (21434-21443). In a build with the Privacy Filter library linked, **logging in** inside a scratch
   run starts a 1.6 GB model download into the scratch tree (see CLAUDE.md, "Privacy model").
6. Stop the app with SIGTERM to the main process (what `--secs` does), never SIGKILL, so it can
   clean up its sidecar and its profile directory. Do not leave a DevTools-created page open
   at exit: both crashed runs during this rig's development (SIGTRAP at shutdown) had one.

## Scripts

All CDP scripts take `--port N` (else `RIG_CDP_PORT`, else 9222) and print names and counts only.

### scratch-run.sh: isolated scratch run

```
scripts/rig/scratch-run.sh [--ns SIZE] [--secs N] SCRATCH_DIR [-- APP_ARGS...]

scripts/rig/scratch-run.sh --secs 60 "$TMPDIR/qr-rig" -- --remote-debugging-port=9333
```

Writes `app.log`, `net.json` (net log), `app.pid` into `SCRATCH_DIR`, and the app's data, cache and
config under `SCRATCH_DIR/{data,cache,config}`. `--secs N` stops the app after N seconds; without it
the script waits until the app exits (Ctrl-C stops it). `--ns SIZE` runs under `unshare -Urm` with the
data/cache/config tree on a tmpfs of that size (for disk-footprint and full-disk experiments) and
samples tmpfs usage into `df.txt` every 5 s. Environment: `RIG_APP_BIN`, `RIG_APP_CWD` (default
`src-tauri`, as `cargo tauri dev` uses), `RIG_RUST_LOG` (default `warn`). It reports any process still
tied to the scratch dir after the app exits.

### disk.sh: directory listing by name and size

```
scripts/rig/disk.sh <dir> [depth]

scripts/rig/disk.sh "$TMPDIR/qr-rig/data" 2
scripts/rig/disk.sh "$XDG_RUNTIME_DIR/ai.quietrabbit.app" 1     # the CEF profile, while the app runs
```

Prints the total size, the filesystem's free space, and `du` per entry. Never prints file contents.

### netlog.mjs: net-log parser

```
node scripts/rig/netlog.mjs <net.json> [--host-regex RE] [--full] [--strict]

node scripts/rig/netlog.mjs "$TMPDIR/qr-rig/net.json"
```

Reports URL requests by origin and by method+URL with time offsets from the start of the capture, the
socket addresses of connect events (loopback, non-loopback TCP/other, non-loopback UDP), and DNS names
looked up. Reads a log from a killed app up to its last complete event (an empty one is reported as
such). Exit 0: nothing non-loopback; exit 3: a non-loopback URL host, DNS name or TCP/other socket
address. A non-loopback UDP `connect()` alone is Chromium's IPv6 reachability probe (a route lookup;
no packet is sent): it is printed on its own line and does not change the exit code unless `--strict`.

### ss-sample.sh and ss-report.mjs: socket sampling

```
scripts/rig/ss-sample.sh <outfile> <interval-secs> <pid|process-name> [duration-secs]
node scripts/rig/ss-report.mjs <outfile>

scripts/rig/ss-sample.sh "$TMPDIR/ss.txt" 2 "$(cat "$TMPDIR/qr-rig/app.pid")" 60
node scripts/rig/ss-report.mjs "$TMPDIR/ss.txt"
```

The sampler records `ss -tunp` for the process and all its descendants every interval (duration 0 or
omitted: until killed). Give a pid when more than one process could match a name; an ambiguous name is
refused. The report lists each distinct remote address with the owning process names, sample count
and first/last time, and flags non-loopback remotes (exit 3). Sampling sees only connections that are
open at a sample, so a short connection can fall between samples; use the net log for those.

### canary.mjs: set and read a synthetic canary set

```
node scripts/rig/canary.mjs set|read [--port N] [--host SUBSTR] [--all-pages] [--expect present|absent]

node scripts/rig/canary.mjs set --host duck.ai --expect present
```

On the first page target (or every page with `--all-pages`, or those whose host contains `--host`),
`set` writes a cookie, a localStorage key, a sessionStorage key, an IndexedDB database and a Cache
Storage cache, all named `qrrig_canary*`. Both modes print the browser-level cookie jar total and
canary count, and per page `canary/total` for each store. `--expect absent` exits 1 if any canary exists;
`--expect present` exits 1 if the cookie, localStorage, IndexedDB or Cache Storage canary is missing
(sessionStorage does not outlive a reload, so it is not required). Exit 2: no matching page target.
The app has no page target until a pane is open; a pane for a provider is opened from the app itself.

### clear-check.mjs: site-data clear check

```
node scripts/rig/clear-check.mjs [--port N] [--host SUBSTR] [--origin ORIGIN]
```

Sets the canary set, calls `Storage.clearDataForOrigin` (browser target first, then the page target)
and reports what is left, then `Network.clearBrowserCookies` if anything remains. Exit 0 only if every
canary is gone (sessionStorage is shown but not counted).

### jar.mjs: what the browser holds

```
node scripts/rig/jar.mjs [--port N]
```

Targets (type and origin), the cookie jar as domain to cookie names, and per page the storage counts.

## Recipes

### items.id=637: cross-user canary check

A provider pane must hold nothing of the previous account after sign-out and sign-in as another.

1. `scratch-run.sh DIR -- --remote-debugging-port=9333`, sign in as account A, open the provider pane.
2. `RIG_CDP_PORT=9333 node scripts/rig/canary.mjs set --host <provider host> --expect present`
3. Sign out, sign in as account B, open the same provider pane.
4. `RIG_CDP_PORT=9333 node scripts/rig/jar.mjs` and `... canary.mjs read --all-pages --expect absent`.
   Pass: exit 0, jar canary 0, every page `0/N` in each store, and the jar holds none of A's cookie names
   (compare the `jar.mjs` output from before and after).
5. Stop the app, delete the scratch dir.

### items.id=619: idle egress assertion

With no user action, nothing leaves the machine.

1. `scratch-run.sh --secs 120 DIR` (start it in the background).
2. `ss-sample.sh SS.txt 2 "$(cat DIR/app.pid)" 100` while it runs.
3. After it exits: `node scripts/rig/netlog.mjs DIR/net.json` and `node scripts/rig/ss-report.mjs SS.txt`.
   Pass: both exit 0. A UDP route probe line from the net log is expected and is not egress.
   `ss-report.mjs` reports the Ollama sidecar's own connections too, under its process name.

### items.id=848: sidecar capture

1. `scratch-run.sh DIR` in the background; take the pid from `DIR/app.pid`.
2. `ss-sample.sh SS.txt 1 <pid> 300`: the tree covers the sidecar and its runner processes.
3. `node scripts/rig/ss-report.mjs SS.txt`: remote addresses by owning process name
   (`qr-ollama`, `ollama` in a debug build, `llama-server`).
