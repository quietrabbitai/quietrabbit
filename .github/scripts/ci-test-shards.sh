#!/usr/bin/env bash
# items.id=844: run the quietrabbit_lib unit-test binary as 3 parallel shards in
# CI, plus every other target plain `cargo test` would run, exactly once.
#
#   ci-test-shards.sh check    build, then only verify the shard partition
#   ci-test-shards.sh lib      build, verify, run the 3 lib shards
#   ci-test-shards.sh others   build, verify the target list, run the other targets
#   ci-test-shards.sh all      lib, then others; every failure is reported
#
# Plain `cargo test` stays the local verification standard (decisions.id=776);
# only CI shards. No retry of any kind: a red or crashed shard fails the job.
#
# WHY THESE SHARDS: the suite is serialized inside one process by
# test_support::ENV_MUTEX (QR_DATA_ROOT is process-global), so more threads in
# one process buy nothing; separate processes each have their own mutex.
#   A  ollama_ownership:: ollama_sidecar::                            1 thread
#   B  auth::key_rotation:: commands::key_rotation:: auth::writer_gate::  1 thread
#   C  everything else (skips every A and B filter)                   2 threads
# 1/1/2 threads is the 4-vCPU budget, and it keeps the starvation-prone
# idle_timeout pool-size assertion (items.id=846 note) off an oversubscribed box.
# The trailing :: on each filter is mandatory: "auth::key_rotation" also matches
# 20 tests of other modules. Filters are libtest substring matches, and A, B and
# the C skips use the same strings, so the partition is consistent by
# construction; the COUNT CHECK below proves it anyway.
#
# COUNT CHECK (fails the job): the sorted test-name lists of A, B and C must be
# pairwise disjoint and their union must equal the sorted full `--list`, and
# each shard must be non-empty. A new test module lands in C automatically
# because C is "everything not matched by A or B". After the run, each shard's
# passed+failed+ignored must equal its listed count.
#
# OTHER TARGETS: bins and every [[test]] / tests/*.rs target (taken from
# `cargo metadata`, so a new integration file is picked up) run with one
# `cargo test --no-fail-fast --bins --test ...`, then doc tests with
# `cargo test --doc` (cargo refuses to mix --doc with other selectors). The
# executables of --lib plus those of the explicit selection must equal the set
# plain `cargo test --no-run` builds, or the job fails: nothing dropped.
#
# ISOLATION: each shard gets its own TMPDIR and XDG_{DATA,CONFIG,CACHE,STATE}_HOME
# so shards cannot collide on files. /dev/shm is shared but only
# auth::key_rotation_io's tmpfs test (shard B alone, 1 thread) touches it.
# Ollama sidecar tests bind ephemeral ports, never 21434-21443.
#
# Run from anywhere; it cd's to src-tauri. Output lines are prefixed [A] [B] [C].

set -u
set -o pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT/src-tauri" || exit 2

# --- shard definitions (the single place; see the header) --------------------
SHARD_NAMES=(A B C)
SHARD_A_FILTERS=(ollama_ownership:: ollama_sidecar::)
SHARD_B_FILTERS=(auth::key_rotation:: commands::key_rotation:: auth::writer_gate::)
SHARD_A_THREADS=1
SHARD_B_THREADS=1
SHARD_C_THREADS=2
# -----------------------------------------------------------------------------

WORK="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ci-test-shards.XXXXXX")" || exit 2
trap 'rm -rf "$WORK"' EXIT

fail=0
err() { echo "::error::$*"; fail=1; }

# Names of the lib tests selected by the given extra libtest args, sorted.
list_lib() {
  local out
  if ! out=$(cargo test --lib -- --list "$@" 2>"$WORK/list.err"); then
    cat "$WORK/list.err"
    return 1
  fi
  grep ': test$' <<<"$out" | sed 's/: test$//' | LC_ALL=C sort
}

skip_args() {
  local f
  for f in "${SHARD_A_FILTERS[@]}" "${SHARD_B_FILTERS[@]}"; do
    printf '%s\n' --skip "$f"
  done
}

shard_filters() { # name -> filter / skip args, one per line
  case "$1" in
    A) printf '%s\n' "${SHARD_A_FILTERS[@]}" ;;
    B) printf '%s\n' "${SHARD_B_FILTERS[@]}" ;;
    C) skip_args ;;
  esac
}

shard_threads() {
  case "$1" in A) echo "$SHARD_A_THREADS" ;; B) echo "$SHARD_B_THREADS" ;; C) echo "$SHARD_C_THREADS" ;; esac
}

build() {
  echo "::group::cargo test --no-run (build every test target once)"
  if ! cargo test --no-run; then
    echo "::endgroup::"
    err "build failed"
    return 1
  fi
  echo "::endgroup::"
}

verify_partition() {
  local s mapfile_args
  list_lib >"$WORK/full.txt" || { err "could not list the lib tests"; return 1; }
  local total sum=0
  total=$(wc -l <"$WORK/full.txt")
  for s in "${SHARD_NAMES[@]}"; do
    mapfile -t mapfile_args < <(shard_filters "$s")
    list_lib "${mapfile_args[@]}" >"$WORK/$s.list" || { err "could not list shard $s"; return 1; }
    local n
    n=$(wc -l <"$WORK/$s.list")
    echo "shard $s: $n tests"
    [ "$n" -gt 0 ] || err "shard $s selects no tests (a filter no longer matches anything)"
    sum=$((sum + n))
  done
  echo "shard sum: $sum, full lib list: $total"
  local dups
  dups=$(cat "$WORK"/A.list "$WORK"/B.list "$WORK"/C.list | LC_ALL=C sort | uniq -d)
  [ -z "$dups" ] || err "tests selected by more than one shard:"$'\n'"$dups"
  cat "$WORK"/A.list "$WORK"/B.list "$WORK"/C.list | LC_ALL=C sort >"$WORK/union.txt"
  if ! diff "$WORK/full.txt" "$WORK/union.txt" >"$WORK/partition.diff"; then
    err "shard union differs from the full list ('<' dropped, '>' extra):"$'\n'"$(cat "$WORK/partition.diff")"
  fi
  [ "$sum" -eq "$total" ] || err "shard counts sum to $sum, full list has $total"
  [ "$fail" -eq 0 ] && echo "count check OK: $sum = $total, disjoint, union equals the full list"
  return "$fail"
}

run_shard() { # name; runs in a background subshell, writes $WORK/<name>.status
  local name=$1 threads dir
  threads=$(shard_threads "$name")
  dir="$WORK/env-$name"
  mkdir -p "$dir"/{tmp,data,config,cache,state}
  local args
  mapfile -t args < <(shard_filters "$name")
  local start=$SECONDS rc
  (
    export TMPDIR="$dir/tmp" XDG_DATA_HOME="$dir/data" XDG_CONFIG_HOME="$dir/config" \
      XDG_CACHE_HOME="$dir/cache" XDG_STATE_HOME="$dir/state"
    echo "shard $name: --test-threads=$threads, args: ${args[*]}"
    cargo test --lib -- --test-threads="$threads" "${args[@]}" 2>&1 | tee "$WORK/$name.log" | sed -u "s/^/[$name] /"
    exit "${PIPESTATUS[0]}"
  )
  rc=$?
  echo "$rc $((SECONDS - start))" >"$WORK/$name.status"
}

run_lib() {
  local s
  for s in "${SHARD_NAMES[@]}"; do run_shard "$s" & done
  wait
  for s in "${SHARD_NAMES[@]}"; do
    local rc secs
    read -r rc secs <"$WORK/$s.status" 2>/dev/null || { rc=missing; secs=0; }
    local want got
    want=$(wc -l <"$WORK/$s.list")
    got=$(sed -n 's/^test result: [A-Za-z]*\. \([0-9]*\) passed; \([0-9]*\) failed; \([0-9]*\) ignored.*/\1 \2 \3/p' "$WORK/$s.log" |
      awk '{t += $1 + $2 + $3} END {print t + 0}')
    echo "shard $s: exit $rc, ${secs}s, ran passed+failed+ignored=$got of $want listed"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
      [ -s "$WORK/summary.md" ] || printf '| shard | exit | seconds | ran / listed |\n|---|---|---|---|\n' >"$WORK/summary.md"
      printf '| %s | %s | %s | %s / %s |\n' "$s" "$rc" "$secs" "$got" "$want" >>"$WORK/summary.md"
    fi
    [ "$rc" = 0 ] || err "lib shard $s FAILED (exit $rc)"
    grep -q 'signal: [0-9]' "$WORK/$s.log" && err "lib shard $s: a test process died on a signal (see the Core-dump backtrace step); do not retry, investigate"
    [ "$got" -eq "$want" ] || err "lib shard $s accounted for $got tests but $want were listed"
  done
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ] && [ -s "$WORK/summary.md" ]; then
    { printf '### Lib test shards\n\n'; cat "$WORK/summary.md"; } >>"$GITHUB_STEP_SUMMARY"
  fi
}

# Executables plain `cargo test` would build, as "name|path" sorted.
test_executables() { # extra cargo args
  cargo test --no-run --message-format=json "$@" 2>/dev/null |
    jq -r 'select(.reason=="compiler-artifact" and .profile.test==true and .executable != null) | "\(.target.name)|\(.executable)"' |
    LC_ALL=C sort -u
}

run_others() {
  local -a sel=(--bins)
  local t
  while IFS= read -r t; do sel+=(--test "$t"); done < <(
    cargo metadata --no-deps --format-version 1 |
      jq -r '.packages[].targets[] | select(.kind | index("test")) | .name' | LC_ALL=C sort)

  # lib + the explicit selection must equal everything plain cargo test builds.
  test_executables >"$WORK/all.exes"
  { test_executables --lib; test_executables "${sel[@]}"; } | LC_ALL=C sort -u >"$WORK/split.exes"
  if ! diff "$WORK/all.exes" "$WORK/split.exes" >"$WORK/exes.diff"; then
    err "test executables of plain cargo test differ from lib + explicit selection:"$'\n'"$(cat "$WORK/exes.diff")"
    return 1
  fi
  echo "target check OK: lib + [${sel[*]}] + --doc = plain cargo test ($(wc -l <"$WORK/all.exes") executables)"

  echo "::group::other targets: cargo test --no-fail-fast ${sel[*]}"
  cargo test --no-fail-fast "${sel[@]}" || err "other test targets FAILED"
  echo "::endgroup::"
  echo "::group::doc tests: cargo test --doc"
  cargo test --doc || err "doc tests FAILED"
  echo "::endgroup::"
}

cmd=${1:-all}
case "$cmd" in
  check) build && verify_partition ;;
  lib) build && verify_partition && run_lib ;;
  others) build && run_others ;;
  all)
    if build; then
      if verify_partition; then run_lib; fi
      run_others
    fi
    ;;
  *) echo "usage: $0 check|lib|others|all" >&2; exit 2 ;;
esac
if [ "$fail" -ne 0 ]; then
  echo "::error::sharded test run FAILED (see the errors above)"
  exit 1
fi
echo "sharded test run OK"
