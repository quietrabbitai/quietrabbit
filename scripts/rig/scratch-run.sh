#!/usr/bin/env bash
# Run the debug app in an isolated scratch tree: its own XDG data/cache/config dirs, QR_* and
# OLLAMA_* unset, a net log written beside them. Optionally inside a private mount namespace whose
# whole data/cache/config tree is a size-limited tmpfs.
#
#   scratch-run.sh [--ns SIZE] [--secs N] SCRATCH_DIR [-- APP_ARGS...]
#
#   SCRATCH_DIR  must not exist or must be empty; it is created. Nothing is ever deleted by this script.
#   --ns SIZE    run under `unshare -Urm` with a tmpfs of SIZE (e.g. 64m) as the data/cache/config tree
#   --secs N     stop the app after N seconds (SIGTERM to the main process, SIGKILL to its group after 20 s);
#                without it the script waits until the app exits
#   APP_ARGS     passed to the app, e.g. --remote-debugging-port=9333
#
# Environment: RIG_APP_BIN (default <repo>/src-tauri/target/debug/quietrabbit), RIG_APP_CWD (default
# the binary's src-tauri directory, as `cargo tauri dev` uses), RIG_RUST_LOG (default warn).
#
# Writes into SCRATCH_DIR: app.log, net.json (net log), app.pid, and in --ns mode df.txt (tmpfs usage
# every 5 s). It sets XDG_DATA_HOME, XDG_CACHE_HOME and XDG_CONFIG_HOME itself and refuses to run if
# it cannot prove they are inside SCRATCH_DIR. QR_DATA_ROOT does NOT isolate the data directory.
# XDG_RUNTIME_DIR is left alone (the GUI session needs it); the app's CEF profile lives under it in a
# per-pid directory that the app deletes at exit.
set -u

die() { echo "scratch-run: $*" >&2; exit "${2:-1}"; }

self=$(realpath -- "$0")
orig_args=("$@")
ns=""
secs=0
while [ $# -gt 0 ]; do
  case $1 in
    --ns) ns=${2:-}; [ -n "$ns" ] || die "--ns needs a size" 64; shift 2 ;;
    --secs) secs=${2:-}; shift 2 ;;
    --) break ;;
    -*) die "unknown option $1" 64 ;;
    *) break ;;
  esac
done
case $secs in '' | *[!0-9]*) die "--secs needs whole seconds" 64 ;; esac
[ $# -ge 1 ] || { sed -n '2,22p' "$self" | sed 's/^# \{0,1\}//' >&2; exit 64; }
scratch=$1
shift
[ "${1:-}" = "--" ] && shift

[ -n "${HOME:-}" ] || die "HOME is not set" 64
scratch=$(realpath -m -- "$scratch")
home=$(realpath -m -- "$HOME")

# Refuse any scratch dir that is, contains or sits inside a real user data/cache/config location.
for real in "$home" "$home/.local/share" "$home/.cache" "$home/.config" \
  "${XDG_DATA_HOME:-$home/.local/share}" "${XDG_CACHE_HOME:-$home/.cache}" "${XDG_CONFIG_HOME:-$home/.config}"; do
  real=$(realpath -m -- "$real")
  case "$real/" in
    "$scratch"/* ) die "refusing: $scratch contains $real" ;;
  esac
  case "$scratch/" in
    "$real"/* ) die "refusing: $scratch is inside $real" ;;
  esac
done
[ "$scratch" != "/" ] || die "refusing: scratch dir is /"

if [ -e "$scratch" ]; then
  [ -d "$scratch" ] || die "$scratch exists and is not a directory"
  left=$(ls -A -- "$scratch")
  if [ -n "$left" ] && ! { [ -n "${RIG_IN_NS:-}" ] && [ "$left" = "mnt" ]; }; then
    die "$scratch is not empty; pass a new or empty directory"
  fi
fi

bin=${RIG_APP_BIN:-$(dirname "$self")/../../src-tauri/target/debug/quietrabbit}
[ -x "$bin" ] || die "app binary not found or not executable: $bin (set RIG_APP_BIN)" 66
bin=$(realpath -- "$bin")
cwd=${RIG_APP_CWD:-$(dirname "$bin")/../..}
[ -d "$cwd" ] || die "RIG_APP_CWD is not a directory: $cwd" 66
cwd=$(realpath -- "$cwd")

mkdir -p -- "$scratch" || die "cannot create $scratch"
chmod 700 -- "$scratch"

if [ -n "$ns" ] && [ -z "${RIG_IN_NS:-}" ]; then
  command -v unshare >/dev/null || die "unshare not found" 69
  mkdir -p -- "$scratch/mnt"
  exec env RIG_IN_NS=1 RIG_NS_SIZE="$ns" unshare -Urm -- "$self" "${orig_args[@]}"
fi

if [ -n "${RIG_IN_NS:-}" ]; then
  mount -t tmpfs -o "size=${RIG_NS_SIZE},mode=0700" tmpfs "$scratch/mnt" || die "tmpfs mount failed"
  tree=$scratch/mnt
else
  tree=$scratch
fi
mkdir -p -- "$tree/data" "$tree/cache" "$tree/config"

XDG_DATA_HOME=$tree/data
XDG_CACHE_HOME=$tree/cache
XDG_CONFIG_HOME=$tree/config
for d in "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME"; do
  case "$(realpath -- "$d")/" in
    "$scratch"/*) ;;
    *) die "refusing: $d is not inside $scratch" ;;
  esac
done
export XDG_DATA_HOME XDG_CACHE_HOME XDG_CONFIG_HOME
for v in $(compgen -e); do
  case $v in QR_* | OLLAMA_*) unset "$v" ;; esac
done

cd -- "$cwd" || die "cannot cd to $cwd"
LD_LIBRARY_PATH=$(dirname "$bin")${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH} \
  RUST_LOG=${RIG_RUST_LOG:-warn} \
  setsid "$bin" --log-net-log="$scratch/net.json" --net-log-capture-mode=Default "$@" >"$scratch/app.log" 2>&1 &
pid=$!
echo "$pid" >"$scratch/app.pid"
echo "scratch-run: app pid $pid, scratch dir $scratch${ns:+, tmpfs $ns}"

# SIGTERM goes to the main process only: it runs the app's own shutdown (browser helpers, sidecar,
# net log flush). Signalling the whole group at once makes the browser process crash on exit.
stop_app() {
  kill -TERM "$pid" 2>/dev/null
  for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 1
  done
  echo "scratch-run: app ignored SIGTERM for 20 s, sending SIGKILL to its process group" >&2
  kill -KILL -- "-$pid" 2>/dev/null
}
trap 'stop_app; exit 130' INT TERM

if [ -n "${RIG_IN_NS:-}" ]; then
  (
    while kill -0 "$pid" 2>/dev/null; do
      echo "$(date +%T) $(df -k "$scratch/mnt" | tail -1 | awk '{print $3, $4}')" >>"$scratch/df.txt"
      sleep 5
    done
  ) &
fi

if [ "$secs" -gt 0 ]; then
  end=$((SECONDS + secs))
  while [ "$SECONDS" -lt "$end" ] && kill -0 "$pid" 2>/dev/null; do sleep 1; done
  kill -0 "$pid" 2>/dev/null && stop_app
else
  wait "$pid"
fi
wait 2>/dev/null

# What is left after the app exits: any other process of ours whose environment points into the
# scratch tree (a stray helper or sidecar). Nothing is killed here.
left=""
for e in /proc/[0-9]*/environ; do
  p=${e#/proc/}
  p=${p%/environ}
  [ "$p" = "$$" ] && continue
  [ -r "$e" ] || continue
  if { tr '\0' '\n' <"$e" | grep -qF -- "=$scratch"; } 2>/dev/null; then left="$left $p"; fi
done
[ -z "$left" ] || echo "scratch-run: processes still tied to the scratch dir:$left" >&2
echo "scratch-run: done. net log $scratch/net.json, app log $scratch/app.log"
