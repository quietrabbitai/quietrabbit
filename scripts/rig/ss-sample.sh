#!/usr/bin/env bash
# Sample `ss -tunp` for a process tree on an interval, appending to a file.
#
#   ss-sample.sh <outfile> <interval-secs> <pid|process-name> [duration-secs]
#
# <pid|process-name>: a numeric pid, or an exact process name (pgrep -x). A name that matches more
# than one process is refused (a real instance running beside a scratch one would pollute the
# result); pass the pid instead. The tree is the root and all its descendants, re-read each sample.
# duration-secs 0 or omitted: run until killed. Output lines:
#   "# HH:MM:SS root=<pid> tree=<pid,pid,...>"   or   "# HH:MM:SS root=none"
#   "<proto> <state> <local> <peer> <users:(...)>" one per socket owned by the tree
# Report with: node ss-report.mjs <outfile>
set -u

if [ $# -lt 3 ]; then
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 64
fi
out=$1
iv=$2
target=$3
dur=${4:-0}

case $iv in '' | *[!0-9.]*) echo "interval must be a number" >&2; exit 64 ;; esac
case $dur in '' | *[!0-9]*) echo "duration must be whole seconds" >&2; exit 64 ;; esac
command -v ss >/dev/null || { echo "ss not found" >&2; exit 69; }

if [ "$target" -eq "$target" ] 2>/dev/null; then
  mode=pid
else
  mode=name
  n=$(pgrep -x -- "$target" | wc -l)
  if [ "$n" -gt 1 ]; then
    echo "$n processes named '$target'; pass a pid instead" >&2
    exit 2
  fi
fi

descendants() {
  local c
  for c in $(pgrep -P "$1" 2>/dev/null); do
    echo "$c"
    descendants "$c"
  done
}

end=$((SECONDS + dur))
while :; do
  if [ "$mode" = pid ]; then
    root=$target
    kill -0 "$root" 2>/dev/null || root=
  else
    root=$(pgrep -x -- "$target" | head -1)
  fi
  now=$(date +%T)
  if [ -n "$root" ]; then
    pids=$( (echo "$root"; descendants "$root") | sort -un | paste -sd, -)
    echo "# $now root=$root tree=$pids" >>"$out"
    re=$(echo "$pids" | sed 's/,/|/g')
    ss -tunpH 2>/dev/null | grep -E "pid=($re)[,)]" | awk '{print $1, $2, $5, $6, $7}' >>"$out"
  else
    echo "# $now root=none" >>"$out"
  fi
  if [ "$dur" -gt 0 ] && [ "$SECONDS" -ge "$end" ]; then break; fi
  sleep "$iv"
done
