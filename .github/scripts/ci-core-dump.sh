#!/usr/bin/env bash
# items.id=800: make a SIGSEGV in a CI test binary (items.id=566) diagnosable.
#
#   ci-core-dump.sh enable   point kernel.core_pattern at a plain file under
#                            /tmp/cores before the Test step
#   ci-core-dump.sh report   after a failed job, print kernel segfault lines and
#                            a symbolized all-thread backtrace of any core found
#
# Diagnostics must never fail the job: every command that can fail is guarded,
# and both subcommands exit 0. No core file is ever uploaded -- the repo is
# public, and a raw core is the whole process memory. gdb is run with
# frame-arguments none and without `bt full`, registers or memory dumps, so the
# log shows function names, file paths and library offsets only.

set -u

CORE_DIR="${CORE_DIR:-/tmp/cores}"
MAX_LINES=600

enable() {
  echo "core_pattern before: $(cat /proc/sys/kernel/core_pattern 2>&1)"
  mkdir -p "$CORE_DIR" && chmod 1777 "$CORE_DIR" ||
    echo "::warning::could not create $CORE_DIR"
  sudo -n sysctl -w "kernel.core_pattern=$CORE_DIR/core.%e.%p" ||
    echo "::warning::could not set kernel.core_pattern; no core file will be written"
  echo "core_pattern after:  $(cat /proc/sys/kernel/core_pattern 2>&1)"
  echo "ulimit -c (soft/hard): $(ulimit -Sc 2>&1) / $(ulimit -Hc 2>&1)"
}

report() {
  echo "== kernel segfault lines =="
  sudo -n dmesg 2>/dev/null | grep -i -E 'segfault|general protection' | tail -n 20 ||
    echo "(none, or dmesg unavailable)"

  shopt -s nullglob
  local cores=("$CORE_DIR"/core.*)
  if [ "${#cores[@]}" -eq 0 ]; then
    echo "== no core files in $CORE_DIR: this failure was not a crash, or no core was written =="
    return 0
  fi

  if ! command -v gdb >/dev/null 2>&1; then
    sudo -n apt-get install -y gdb >/dev/null 2>&1 ||
      { echo "::warning::could not install gdb; kernel lines above are the only record"; return 0; }
  fi

  local core exe
  for core in "${cores[@]}"; do
    echo "== $core =="
    # `file` reports the full path of the executable as execfn='...'.
    exe=$(file "$core" | sed -n "s/.*execfn: '\([^']*\)'.*/\1/p")
    if [ -z "$exe" ] || [ ! -x "$exe" ]; then
      echo "::warning::could not locate the executable for $core (execfn='$exe')"
      exe=""
    fi
    echo "executable: ${exe:-unknown}"
    DEBUGINFOD_URLS="${DEBUGINFOD_URLS:-https://debuginfod.ubuntu.com}" \
      timeout 240 gdb -batch -nx \
        -iex "set debuginfod enabled on" \
        -ex "set pagination off" \
        -ex "set print frame-arguments none" \
        -ex "set print entry-values no" \
        -ex "info sharedlibrary" \
        -ex "info threads" \
        -ex "thread apply all bt 60" \
        ${exe:+"$exe"} --core "$core" 2>&1 | head -n "$MAX_LINES" ||
      echo "::warning::gdb failed on $core"
  done
}

case "${1:-}" in
  enable) enable ;;
  report) report ;;
  *) echo "usage: $0 enable|report" >&2; exit 2 ;;
esac
exit 0
