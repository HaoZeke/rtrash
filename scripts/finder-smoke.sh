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

rt=$1
unset RTRASH_MACOS_TRASH
work=$(mktemp -d "$HOME/rtrash-finder.XXXXXX")
f="$work/a b %é.txt"
fail() { echo "FAIL $*"; exit 1; }
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

# The FreeDesktop trash stays reachable.
h="$work/fdo.txt"
echo fdo > "$h"
RTRASH_MACOS_TRASH=freedesktop "$rt" put "$h" || fail "freedesktop put"
RTRASH_MACOS_TRASH=freedesktop "$rt" list | grep -qF -- "$h" || fail "freedesktop list"
RTRASH_MACOS_TRASH=freedesktop "$rt" restore "$h" || fail "freedesktop restore"
echo "ok   RTRASH_MACOS_TRASH=freedesktop"

cd /
RTRASH_MACOS_TRASH=freedesktop "$rt" put -r "$work" >/dev/null
