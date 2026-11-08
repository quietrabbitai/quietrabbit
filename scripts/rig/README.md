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
   The one exception to "new or empty" is `--reuse` (see below), which adds conditions and removes none:
   the home/XDG refusals are checked first and apply to a reused dir exactly as to a new one.
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
scripts/rig/scratch-run.sh [--ns SIZE] [--secs N] [--reuse] SCRATCH_DIR [-- APP_ARGS...]

scripts/rig/scratch-run.sh --secs 60 "$TMPDIR/qr-rig" -- --remote-debugging-port=9333
```

`--reuse` relaunches into a scratch dir this script created earlier, for a **two-phase run** (create the
data, stop the app, edit `shared.db`, relaunch). The script writes a `.rig-scratch` marker holding the
dir's own path when it creates a scratch dir; `--reuse` on a non-empty dir is accepted only if that marker
is there (a regular file) and names the same path, the pid in `app.pid` is no longer alive, and `--ns` is
not used (the tmpfs does not outlive its run, so there is nothing to reuse). Anything else is refused as
before. The previous `app.log` and `net.json` are renamed to `app.N.log` and `net.N.json`, never
overwritten. The marker is a guard against reusing a directory the script did not make, not a security
boundary: the home/XDG refusals are what keep a run off the real data, and they run before the marker is
looked at. Without `--reuse`, a non-empty dir is refused as always. Passing `--reuse` for a new or empty
dir is harmless (it is simply a first run). Editing a database under a running app is the alternative this
replaces; do not do that when two phases will do.

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
looked up. A final section says **why**: speculative DNS requests (an event carrying `is_speculative` and
a `host`) and preconnects (`is_preconnect` and a `url`), grouped by source type, event type and name,
with the count, every offset (the first 12) and the first and last time as an offset and as UTC wall
clock. Matching is on those parameters, not on event names. It prints the host (speculative DNS) or the
origin (preconnect) only, never a path or query string, not even with `--full`. Reads a log from a killed
app up to its last complete event (an empty one is reported as such). Exit 0: nothing non-loopback; exit
3: a non-loopback URL host, DNS name or TCP/other socket address. A non-loopback UDP `connect()` alone is
Chromium's IPv6 reachability probe (a route lookup; no packet is sent): it is printed on its own line and
does not change the exit code unless `--strict`. The speculative/preconnect section is likewise
informational (it adds a note to the RESULT line) unless `--strict`, which makes a non-loopback name in it
fail with exit 3. The section was checked against synthetic logs only, built from the event shapes in the
items.id=855 handoff; the first real capture with a pane open is the real test of the parameter names.

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
The app has no page target until a pane is open; open one with `--qr-rig-open-pane` (see "Opening a pane
in a scratch run" below).

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

## Notes on driving a scratch run

### Blank main window on a plain debug build

A plain debug build (`cargo build`, not `cargo tauri dev`) loads the main window from the configured
`devUrl`, `http://localhost:1420` (`src-tauri/tauri.conf.json`; the Vite port in `frontend/vite.config.ts`).
With nothing serving that address the main window is **blank**. That is expected, not a crash. Serve
something on port 1420 first (`cargo tauri dev` serves the real frontend there, but it is not a scratch
run; for a rig run a stub page is enough, and the page doubles as the main webview's allowed IPC origin
when you call commands by hand). Bind the stub to 127.0.0.1 so it adds no non-loopback traffic.

### Opening a pane in a scratch run

A Cloud Chat pane can be opened **logged out**: `open_cloud_chat_panes` (Tauri IPC command, argument
`providerIds`) treats "not logged in" as "no stored cookies" and opens the pane anyway, so no account is
needed (`src-tauri/src/commands/cloud_chat_pane.rs`). It needs a CEF build and the provider's
`launch_url` in the `providers` table of `shared.db`. A provider row is looked up by id, and the
frontend never passes a URL. To point a provider at a local test page, edit `launch_url` in the scratch
tree's `shared.db` (`SCRATCH_DIR/data/ai.quietrabbit.app/shared.db`; the data root is named after the Tauri
identifier) **between two phases** with `--reuse`, never in the real data directory:

1. `scratch-run.sh --secs 15 DIR`: the first launch creates `shared.db`.
2. `sqlite3 DIR/data/ai.quietrabbit.app/shared.db "update providers set launch_url='http://127.0.0.1:8181/a.html' where id='duckai'"`
3. `scratch-run.sh --reuse DIR -- --remote-debugging-port=9333 --qr-rig-open-pane=duckai`: the app opens
   the pane itself (next section); no GUI and no main-webview access are needed.

### Rig flags: open a pane and close its popup without the GUI (items.id=860)

Two **debug-build-only** app arguments (`src-tauri/src/rig_hooks.rs`), passed after `--` like any other
app argument. They are arguments, not environment variables, because `scratch-run.sh` unsets every `QR_*`
variable; that rule is unchanged.

- `--qr-rig-open-pane=<provider_id>`: after startup the app calls `open_cloud_chat_panes` (the real
  command function: logged out, no cookie restore, the `launch_url` from the scratch `shared.db`) and
  repeats it every 500 ms until a pane exists (the pane host drops an open that arrives before its
  GLArea is realized), for at most 30 s. With `RIG_RUST_LOG='warn,quietrabbit_lib::rig_hooks=info'` (the default `warn` hides it) `app.log` gets
  `rig_hooks: pane open for provider=<id>`.
- `--qr-rig-close-popup-after-ms=<N>`: after each `cloud-chat-popup-opened` event, wait N ms (so you can
  look at the popup over CDP first), then call `close_cloud_chat_popup`, the command behind the popup-close
  button and Ctrl+W. With the same `RIG_RUST_LOG`, `app.log` gets `rig_hooks: closing the popup of provider=<id>`.

Flags absent: nothing changes. Unknown arguments (CEF's `--remote-debugging-port=...` and so on) are
ignored, and a malformed rig flag is logged and ignored; neither stops the app. Logs carry provider ids
and counts only.

```
# phase 1 and 2 as above (launch_url pointed at a local page), then:
scripts/rig/scratch-run.sh --reuse --secs 60 DIR -- --remote-debugging-port=9333 \
  --qr-rig-open-pane=duckai --qr-rig-close-popup-after-ms=8000
# meanwhile: RIG_CDP_PORT=9333 node scripts/rig/jar.mjs shows the pane's page target; open a popup from
# the page with CDP Runtime.evaluate {expression: "window.open('/b.html')", userGesture: true}; jar.mjs
# shows the popup target, and about 8 s later it is gone.
```

What this does and does not cover. It exercises the command, the `ClosePopup` dispatch, the pane host's
close channel and drain, `force_close_popup`, the page-side effect (`close_browser(true)`, so the page sees
a closed window) and the `cloud-chat-popup-closed` event. It does **not** exercise the button's React
`onClick` or the Tauri IPC serialisation of the call: the button is in the real frontend, which sits behind
the login screen, and the rig does not log in. items.id=857 closed popups with `window.close()` through
CDP, which skips the app's own close path altogether; this does not.

It must not exist in a release build, three ways: `tests/rig_gate.rs` (the flag text and every use of
`rig_hooks` sit behind `#[cfg(debug_assertions)]`, checked over the source tree), the
`release_has_no_rig_hooks` test in `lib.rs` (the release test executable does not contain the flag text;
`cargo test --release --lib -- release_`, build with `QR_ALLOW_RELEASE_WITHOUT_PRIVACY_FILTER=1` if the
Privacy Filter library is not at hand), and `strings` on a release binary finding no `qr-rig`.
Do not copy a flag name into code outside `rig_hooks.rs`.

The earlier route to the main webview (the WebKitGTK inspector socket, `WEBKIT_INSPECTOR_HTTP_SERVER`,
used once in items.id=855) is no longer needed for opening a pane. It was never a tested recipe.

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
