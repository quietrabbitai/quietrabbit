#!/usr/bin/env bash
# CEF currency check (items.id=872, decisions.id=935): warn when the pinned cef
# crate is older than 8 weeks. Run by .github/workflows/cef-currency.yml, weekly.
#
# Never fails the job: a stale pin, a crates.io outage, a changed response shape
# and an unreadable lock file each print a ::warning:: and exit 0. A warning
# notifies nobody; the report is also printed to the log and, in Actions, to
# the step summary, so whoever owns the cadence (decisions.id=935) has to look.
#
# Pinned version: the `cef` package in src-tauri/Cargo.lock (the resolved pin,
# not the Cargo.toml requirement). Release date: `created_at` of that version
# in crates.io's `cef` listing (one request; the CEF builds index has no dates).
#
# Local test hooks, read only when set (nothing in CI sets them):
#   CEF_CURRENCY_LOCK     path to a Cargo.lock (default src-tauri/Cargo.lock)
#   CEF_CURRENCY_FIXTURE  a JSON file used in place of the crates.io response
#   CEF_CURRENCY_NOW      a date `date -d` understands, used in place of now
set -u

MAX_DAYS=56
lock=${CEF_CURRENCY_LOCK:-src-tauri/Cargo.lock}
url=https://crates.io/api/v1/crates/cef

warn() { echo "::warning title=CEF currency::$1"; }
say() {
  echo "$1"
  [ -z "${GITHUB_STEP_SUMMARY:-}" ] || echo "$1" >>"$GITHUB_STEP_SUMMARY"
}
cannot_check() {
  warn "the currency check could not run: $1"
  say "CEF currency check could not run: $1"
  exit 0
}

[ -r "$lock" ] || cannot_check "cannot read $lock"
pinned=$(awk '/^name = "cef"$/ { f = 1; next } f && /^version = / { gsub(/"/, "", $3); print $3; exit }' "$lock")
[ -n "$pinned" ] || cannot_check "no cef package found in $lock"

listing=$(mktemp) || cannot_check "mktemp failed"
trap 'rm -f "$listing"' EXIT
if [ -n "${CEF_CURRENCY_FIXTURE:-}" ]; then
  cp -- "$CEF_CURRENCY_FIXTURE" "$listing" || cannot_check "cannot read the fixture"
else
  curl -fsS --max-time 30 --retry 2 --retry-delay 5 \
    -A "quietrabbit-cef-currency (${GITHUB_REPOSITORY:-local})" \
    -o "$listing" "$url" || cannot_check "crates.io request failed ($url)"
fi

published=$(jq -r --arg v "$pinned" '[.versions[]? | select(.num == $v) | .created_at][0] // empty' "$listing" 2>/dev/null)
[ -n "$published" ] || cannot_check "cef $pinned not found in the crates.io listing (or the response changed shape)"
newest=$(jq -r '[.versions[]? | select(.yanked | not)] | max_by(.created_at) | "\(.num) published \(.created_at[0:10])"' "$listing" 2>/dev/null)

now=$(date -u -d "${CEF_CURRENCY_NOW:-now}" +%s 2>/dev/null) || cannot_check "bad CEF_CURRENCY_NOW"
then=$(date -u -d "$published" +%s 2>/dev/null) || cannot_check "bad date from crates.io: $published"
days=$(((now - then) / 86400))

say "### CEF currency"
say "- pinned: cef $pinned, published ${published:0:10}, ${days} days ago (limit ${MAX_DAYS})"
say "- newest published crate: ${newest:-unknown}"
if [ "$days" -gt "$MAX_DAYS" ]; then
  warn "pinned cef $pinned is $days days old (limit $MAX_DAYS). Newest published crate: ${newest:-unknown}. Bump per decisions.id=935."
  say "- STALE: bump to the current CEF stable (decisions.id=935)"
else
  say "- ok"
fi
exit 0
