#!/usr/bin/env bash
# items.id=821: keep the Linux dependency .debs in an actions/cache entry so a
# normal CI run does not depend on the Ubuntu mirror for them.
#
#   ci-apt-cache.sh seed  <cache-dir> <package>...
#       (root, after `apt-get update`) work out every .deb the install needs
#       and its SHA-256 from the freshly updated, signed index, then copy each
#       cached .deb whose size AND SHA-256 match into /var/cache/apt/archives.
#   ci-apt-cache.sh stage <cache-dir> <output-file> [matched-key]
#       (root, after a SUCCESSFUL install) copy every needed .deb that is in
#       /var/cache/apt/archives and passes the same check into <cache-dir>,
#       delete everything else from it, and append apt_key=<new cache key> to
#       <output-file> (GITHUB_OUTPUT) when the set is complete and differs from
#       [matched-key], the key of the entry that was restored. The key is a
#       digest of the needed .deb set, so a new entry is written exactly when
#       that set changed (image update, package update, package list edit).
#
# apt itself does NOT hash-check a .deb already sitting in the archives
# directory: it reuses any file whose size matches (tested on Ubuntu 24.04), so
# nothing reaches the archives directory without passing the check above.
# Every failure here degrades to "cache miss": the caller's bounded apt download
# still fetches whatever is missing, and both subcommands exit 0.

set -u

ARCHIVES=/var/cache/apt/archives
WANTED="${APT_WANTED:-/tmp/ci-apt-wanted.txt}"

# Writes "<archive file name> <size> <sha256>" per needed .deb to $WANTED, once.
# Retries see a shorter `--print-uris` list (completed files are skipped), so
# the first, complete list is kept.
build_wanted() {
  [ -s "$WANTED" ] && return 0
  local uris specs
  uris=$(apt-get install -y --print-uris "$@" 2>/dev/null | grep "^'") || return 1
  specs=$(awk '{ n = split($2, p, "_"); if (n == 3) { v = p[2]; gsub(/%3a/, ":", v); print p[1] "=" v } }' <<<"$uris")
  [ -n "$specs" ] || return 1
  # shellcheck disable=SC2086 # one word per package=version
  apt-cache show $specs 2>/dev/null | awk '
    BEGIN { RS = ""; FS = "\n" }
    { pk = ""; ve = ""; ar = ""; sz = ""; sh = ""
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^Package: /) pk = substr($i, 10)
        else if ($i ~ /^Version: /) ve = substr($i, 10)
        else if ($i ~ /^Architecture: /) ar = substr($i, 15)
        else if ($i ~ /^Size: /) sz = substr($i, 7)
        else if ($i ~ /^SHA256: /) sh = substr($i, 9)
      }
      gsub(/:/, "%3a", ve)
      if (pk != "" && ve != "" && ar != "" && sz != "" && sh != "") print pk "_" ve "_" ar ".deb", sz, sh }' | sort -u >"$WANTED.index" || return 1
  # Keep only what the install needs, named exactly as apt names it in the archives.
  awk 'NR == FNR { idx[$1] = $2 " " $3; next } $2 in idx { print $2, idx[$2] }' \
    "$WANTED.index" - <<<"$uris" >"$WANTED.tmp" || return 1
  [ -s "$WANTED.tmp" ] || return 1
  mv "$WANTED.tmp" "$WANTED"
}

# ok_file <path> <size> <sha256>
ok_file() {
  [ -f "$1" ] && [ "$(stat -c %s "$1")" = "$2" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$3" ]
}

seed() {
  local dir=$1; shift
  if ! build_wanted "$@"; then
    echo "::warning::apt cache: could not work out the needed .debs, ignoring the cache"
    return 0
  fi
  local total=0 seeded=0 rejected=0 name size sha
  while read -r name size sha; do
    total=$((total + 1))
    [ -e "$dir/$name" ] || continue
    if ok_file "$dir/$name" "$size" "$sha"; then
      cp -p "$dir/$name" "$ARCHIVES/$name" && seeded=$((seeded + 1))
    else
      rejected=$((rejected + 1))
      echo "::warning::apt cache: $name does not match the signed index, not used"
    fi
  done <"$WANTED"
  echo "apt cache: $seeded of $total needed .debs taken from the cache, $rejected rejected"
}

stage() {
  local dir=$1 out=$2 matched=${3:-}
  [ -s "$WANTED" ] || { echo "apt cache: nothing to stage"; return 0; }
  mkdir -p "$dir" || return 0
  local total=0 staged=0 name size sha f
  declare -A keep=()
  while read -r name size sha; do
    total=$((total + 1))
    if [ -e "$dir/$name" ] && ok_file "$dir/$name" "$size" "$sha"; then
      keep[$name]=1; staged=$((staged + 1)); continue
    fi
    if ok_file "$ARCHIVES/$name" "$size" "$sha"; then
      cp -p "$ARCHIVES/$name" "$dir/$name" && keep[$name]=1 && staged=$((staged + 1))
    fi
  done <"$WANTED"
  for f in "$dir"/*; do
    [ -e "$f" ] || continue
    [ -n "${keep[$(basename "$f")]:-}" ] || rm -f -- "$f"
  done
  [ -n "${SUDO_UID:-}" ] && chown -R "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$dir"
  echo "apt cache: $staged of $total needed .debs staged"
  [ "$staged" -eq "$total" ] || { echo "::warning::apt cache: incomplete set, not saving"; return 0; }
  local key="apt-debs-v1-$(sha256sum "$WANTED" | cut -c1-16)"
  if [ "$key" = "$matched" ]; then
    echo "apt cache: restored entry is current, not saving"
  else
    echo "apt_key=$key" >>"$out"
  fi
}

case "${1:-}" in
  seed)  shift; dir=$1; shift; seed "$dir" "$@" ;;
  stage) shift; stage "$@" ;;
  *) echo "usage: $0 seed <dir> <pkg>... | stage <dir> <out> [matched-key]" >&2 ;;
esac
exit 0
