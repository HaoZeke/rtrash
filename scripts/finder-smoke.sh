#!/usr/bin/env bash
# Plain put, list, restore and empty against the real Finder Trash, and
# the `rtrash finder` commands, on a Mac.
#
#   scripts/finder-smoke.sh RTRASH
#
# Each step works on a scratch folder under $HOME and on items it trashed
# itself. Finder may write its Put Back records a moment after the move,
# so the list checks retry for a few seconds. Exits non-zero at the first
# failing step, naming it.
set -euo pipefail

# Absolute, so the steps after `cd /` still find it.
rt=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
unset RTRASH_MACOS_TRASH
work=$(mktemp -d "$HOME/rtrash-finder.XXXXXX")
f="$work/a b %é.txt"
# On failure, show what rtrash and the Finder Trash hold, so a CI log
# says whether the item went missing or only its Put Back path did
# (Finder may store the name decomposed, NFD, unlike the NFC we passed).
fail() {
  echo "FAIL $*"
  echo "--- rtrash list"
  "$rt" list 2>&1 | tail -n 20 || true
  echo "--- ~/.Trash"
  ls -la "$HOME/.Trash" 2>&1 | tail -n 20 || true
  echo "--- wanted (hex)"
  if [[ -n ${RTRASH_SMOKE_VOLUME:-} ]]; then
    echo "--- $RTRASH_SMOKE_VOLUME/.Trashes/$(id -u)"
    ls -la "$RTRASH_SMOKE_VOLUME/.Trashes/$(id -u)" 2>&1 | tail -n 20 || true
  fi
  echo "--- wanted: ${want:-$f}"
  printf '%s' "${want:-$f}" | od -An -tx1 | tail -n 3
  exit 1
}
listed() { # PATH: wait until list shows PATH
  for _ in $(seq 20); do
    "$rt" list | grep -qF -- "$1" && return 0
    sleep 0.5
  done
  return 1
}

echo payload > "$f"
"$rt" put "$f" || fail "plain put"
[[ ! -e $f ]] || fail "plain put left the file"
listed "$f" || fail "plain list does not show the Put Back path"
"$rt" list --json | grep -qF '"original"' || fail "list --json"
"$rt" restore "$f" || fail "plain restore"
[[ $(cat "$f") == payload ]] || fail "restore content"
echo "ok   plain put, list and restore"

"$rt" put "$f" || fail "second put"
listed "$f" || fail "second list"
name=$("$rt" finder list | awk -F' -> ' -v p="$f" '$2 == p { print $1; exit }')
[[ -n $name ]] || fail "finder list does not show the item"
sleep 1  # the age cut counts whole seconds
"$rt" empty --plain --older-than=0 || fail "plain empty"
[[ ! -e $HOME/.Trash/$name ]] || fail "empty left '$name' in ~/.Trash"
echo "ok   plain empty"

g="$work/via finder.txt"
echo finder > "$g"
"$rt" finder put "$g" || fail "finder put"
for _ in $(seq 20); do
  "$rt" finder list | grep -qF -- "$g" && break
  sleep 0.5
done
"$rt" finder restore "$g" || fail "finder restore"
[[ $(cat "$g") == finder ]] || fail "finder restore content"
echo "ok   finder put, list and restore"

i="$work/undo me.txt"
echo undo > "$i"
"$rt" put "$i" || fail "put for undo"
"$rt" status --json | grep -q '"finder":true' || fail "status on the Finder Trash"
"$rt" trash-stats --json | grep -qF '.Trash' || fail "trash-stats on the Finder Trash"
"$rt" trash-stats --history --json | grep -qF '"event":"put"' || fail "trash-stats --history on the Finder Trash"
"$rt" trash-stats --total --json | grep -qF '.Trash' || fail "trash-stats --total on the Finder Trash"
# The carried home total against a full walk; a bytes difference is
# logged (Finder may still be writing), an items one fails.
sleep 2
carried=$("$rt" trash-stats --total --json)
walked=$("$rt" trash-stats --json --top=0)
python3 - "$carried" "$walked" "$HOME/.Trash" <<'PY' || fail "carried Finder total"
import json, sys
c, w, home = json.loads(sys.argv[1]), json.loads(sys.argv[2]), sys.argv[3]
pick = lambda rows: next(r for r in rows if r["path"] == home)
ct = pick(c["trashes"])
wt = pick(w["trashes"] if isinstance(w, dict) else w)
print(f"carried: {ct['bytes']} bytes, {ct['items']} items, stale={ct['stale']}; walked: {wt.get('bytes')} bytes, {wt.get('items')} items")
if not ct["stale"] and ct["items"] != wt.get("items"):
    sys.exit(1)
PY
"$rt" undo || fail "undo"
[[ $(cat "$i") == undo ]] || fail "undo content"
echo "ok   undo, status and trash-stats (with --total and --history)"

"$rt" put "$i" || fail "put for rm"
"$rt" rm "undo me.txt" || fail "rm on the Finder Trash"
"$rt" list | grep -qF -- "$i" && fail "rm left the item listed"
echo "ok   rm"

# An item on another volume goes to that volume's .Trashes/UID.
if [[ -n ${RTRASH_SMOKE_VOLUME:-} ]]; then
  v="$RTRASH_SMOKE_VOLUME/rtrash smoke.txt"
  want=$v
  echo vol > "$v"
  "$rt" put "$v" || fail "put on the volume"
  [[ -d $RTRASH_SMOKE_VOLUME/.Trashes/$(id -u) ]] || fail "no volume trash"
  listed "$v" || fail "list does not show the volume item"
  "$rt" restore "$v" || fail "restore on the volume"
  [[ $(cat "$v") == vol ]] || fail "volume restore content"
  "$rt" put "$v" && "$rt" rm "rtrash smoke.txt" || fail "rm on the volume"
  echo "ok   external volume"
fi

# A short volume: ask for 2 MiB more free than it has, so reap
# --purge-trash and empty --until-free must purge the 4 MiB item there.
if [[ -n ${RTRASH_SMOKE_VOLUME:-} ]]; then
  big="$RTRASH_SMOKE_VOLUME/rtrash purge.bin"
  scratch="$RTRASH_SMOKE_VOLUME/rtrash scratch"
  mkdir -p "$scratch"
  dd if=/dev/zero of="$big" bs=1048576 count=4 2>/dev/null
  "$rt" put "$big" || fail "put the purge item"
  avail=$(( $(df -k "$RTRASH_SMOKE_VOLUME" | awk 'NR==2 {print $4}') * 1024 ))
  want=$(( avail + 2 * 1048576 ))
  out=$("$rt" reap -n --when-below="$want" --purge-trash --older-than=1h "$scratch" 2>&1) ||
    fail "reap --purge-trash dry run: $out"
  grep -q "would empty 1 item" <<<"$out" || fail "reap --purge-trash saw no Finder item: $out"
  listed "$big" || fail "the dry run purged the item"
  out=$("$rt" empty --until-free="$want" 2>&1)
  grep -q "emptied 1 item" <<<"$out" || fail "empty --until-free: $out"
  "$rt" list | grep -qF -- "$big" && fail "empty --until-free left the item"
  echo "ok   reap --purge-trash and empty --until-free on the volume"
fi

# The FreeDesktop trash stays reachable.
h="$work/fdo.txt"
echo fdo > "$h"
RTRASH_MACOS_TRASH=freedesktop "$rt" put "$h" || fail "freedesktop put"
RTRASH_MACOS_TRASH=freedesktop "$rt" list | grep -qF -- "$h" || fail "freedesktop list"
RTRASH_MACOS_TRASH=freedesktop "$rt" restore "$h" || fail "freedesktop restore"
echo "ok   RTRASH_MACOS_TRASH=freedesktop"

cd /
RTRASH_MACOS_TRASH=freedesktop "$rt" put -r "$work" >/dev/null
