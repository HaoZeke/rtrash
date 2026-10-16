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
# grep -q stops reading at the first match, and rtrash then exits 141 on
# the closed pipe, which pipefail counts as a failure; read it all instead.
has() { grep "$@" >/dev/null; }

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
    "$rt" list | has -F -- "$1" && return 0
    sleep 0.5
  done
  return 1
}

echo payload > "$f"
"$rt" put "$f" || fail "plain put"
[[ ! -e $f ]] || fail "plain put left the file"
listed "$f" || fail "plain list does not show the Put Back path"
"$rt" list --json | has -F '"original"' || fail "list --json"
"$rt" restore "$f" || fail "plain restore"
[[ $(cat "$f") == payload ]] || fail "restore content"
echo "ok   plain put, list and restore"

"$rt" put "$f" || fail "second put"
listed "$f" || fail "second list"
# awk reads to the end: an early exit would close the pipe, and the
# 141 rtrash then exits with fails the step under pipefail.
name=$("$rt" finder list | awk -F' -> ' -v p="$f" '$2 == p && n == "" { n = $1 } END { print n }')
[[ -n $name ]] || fail "finder list does not show the item"
sleep 1  # the age cut counts whole seconds
"$rt" empty --plain --older-than=0 || fail "plain empty"
[[ ! -e $HOME/.Trash/$name ]] || fail "empty left '$name' in ~/.Trash"
echo "ok   plain empty"

g="$work/via finder.txt"
echo finder > "$g"
"$rt" finder put "$g" || fail "finder put"
for _ in $(seq 20); do
  "$rt" finder list | has -F -- "$g" && break
  sleep 0.5
done
"$rt" finder restore "$g" || fail "finder restore"
[[ $(cat "$g") == finder ]] || fail "finder restore content"
echo "ok   finder put, list and restore"

i="$work/undo me.txt"
echo undo > "$i"
"$rt" put "$i" || fail "put for undo"
"$rt" status --json | has '"finder":true' || fail "status on the Finder Trash"
"$rt" trash-stats --json | has -F '.Trash' || fail "trash-stats on the Finder Trash"
"$rt" trash-stats --history --json | has -F '"event":"put"' || fail "trash-stats --history on the Finder Trash"
"$rt" trash-stats --total --json | has -F '.Trash' || fail "trash-stats --total on the Finder Trash"
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
"$rt" list | has -F -- "$i" && fail "rm left the item listed"
echo "ok   rm"

# A name with a newline through the real Finder. Finder may refuse it
# or store it its own way, so this step reports what happened on
# "finder-newline:" lines and fails only when the file is lost: gone
# from disk without a Finder Trash entry to restore.
n="$work/line one
line two.txt"
want=$n
echo newline > "$n"
if out=$("$rt" put "$n" 2>&1); then
  echo "finder-newline: put exited 0"
  [[ ! -e $n ]] || echo "finder-newline: the file is still in place"
  if "$rt" list --format='{path}\0' |
    python3 -c 'import sys; w = sys.argv[1].encode(); sys.exit(0 if any(r.strip(b"\n") == w for r in sys.stdin.buffer.read().split(b"\0")) else 1)' "$n"; then
    echo "finder-newline: list shows the Put Back path unchanged"
  else
    echo "finder-newline: list does not show the path unchanged"
    "$rt" finder list 2>&1 | grep -F "line one" | sed 's/^/finder-newline:   /' || true
  fi
  if rout=$("$rt" restore "$n" 2>&1); then
    echo "finder-newline: restore exited 0"
  else
    echo "finder-newline: restore failed: $rout"
  fi
  if [[ -e $n ]]; then
    [[ $(cat "$n") == newline ]] || fail "newline name restored with other content"
    echo "finder-newline: restored in place"
  else
    found=$(find "$HOME/.Trash" -maxdepth 1 -name 'line one*' -print -quit 2>/dev/null || true)
    [[ -n $found ]] || fail "newline name lost: not on disk and not in ~/.Trash"
    echo "finder-newline: still in ~/.Trash as '$found'"
    mv "$found" "$n" || fail "cannot move the newline item back by hand"
  fi
else
  echo "finder-newline: put refused: $out"
  [[ -e $n ]] || fail "newline name lost after a refused put"
fi
unset want
echo "ok   newline name through Finder (see finder-newline lines)"

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
  "$rt" list | has -F -- "$big" && fail "empty --until-free left the item"
  echo "ok   reap --purge-trash and empty --until-free on the volume"
fi

# The FreeDesktop trash stays reachable.
h="$work/fdo.txt"
echo fdo > "$h"
RTRASH_MACOS_TRASH=freedesktop "$rt" put "$h" || fail "freedesktop put"
RTRASH_MACOS_TRASH=freedesktop "$rt" list | has -F -- "$h" || fail "freedesktop list"
RTRASH_MACOS_TRASH=freedesktop "$rt" restore "$h" || fail "freedesktop restore"
echo "ok   RTRASH_MACOS_TRASH=freedesktop"

cd /
RTRASH_MACOS_TRASH=freedesktop "$rt" put -r "$work" >/dev/null
