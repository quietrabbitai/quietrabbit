#!/usr/bin/env bash
# List a directory by entry name and size (names and sizes only, never contents), plus the
# free space of its filesystem.
#
#   disk.sh <dir> [depth]
#
# depth: how many levels below <dir> to list (default 2). The Cloud Chat browser profile of a
# running app is under $XDG_RUNTIME_DIR/ai.quietrabbit.app/cef-<pid>-<random> (deleted at exit);
# a scratch run's own data tree is <scratch>/data, <scratch>/cache and <scratch>/config.
set -u
dir=${1:-}
depth=${2:-2}
if [ -z "$dir" ]; then
  sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 64
fi
[ -d "$dir" ] || { echo "not a directory: $dir" >&2; exit 66; }
case $depth in '' | *[!0-9]*) echo "depth must be a number" >&2; exit 64 ;; esac

echo "== $dir"
du -sh -- "$dir" 2>/dev/null | cut -f1 | sed 's/^/total: /'
df -h --output=fstype,size,used,avail -- "$dir" 2>/dev/null | tail -1 | awk '{print "filesystem: " $1 ", size " $2 ", used " $3 ", avail " $4}'
du -h --max-depth="$depth" -- "$dir" 2>/dev/null | sort -k2 | awk -v root="$dir" '{ n=$2; sub(root, "", n); if (n == "") n = "."; print $1 "\t" n }'
