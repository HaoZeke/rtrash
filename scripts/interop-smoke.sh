#!/usr/bin/env bash
# Cross-restore between rtrash and the other FreeDesktop trash tools.
#
#   scripts/interop-smoke.sh RTRASH
#
# Each tool on PATH among trash-cli (trash-put, trash-restore), gtrash and
# gio trashes a file, and every tool restores it: rtrash, trash-cli, gtrash
# and gio, in every pairing. File names carry a space, a '%' and Unicode.
# Tools that are missing get skipped with a note; rtrash must be there.
# gio needs a session bus and gvfsd, so the script starts both in
# dbus-run-session when gio is on PATH and no gvfsd runs yet.
# Everything happens in a scratch XDG_DATA_HOME, so the real trash stays
# untouched. RTRASH_INTEROP_NO_GIO=1 leaves gio out. Exits non-zero when any pairing fails.
set -euo pipefail

rtrash=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
GVFSD=${GVFSD:-/usr/libexec/gvfsd}
# gvfsd reads XDG_DATA_HOME when it starts, so the scratch trash comes first.
if [[ -z ${RTRASH_INTEROP_SCRATCH:-} ]]; then
  RTRASH_INTEROP_SCRATCH=$(mktemp -d)
  export RTRASH_INTEROP_SCRATCH
fi
scratch=$RTRASH_INTEROP_SCRATCH
export XDG_DATA_HOME=$scratch/data
mkdir -p "$XDG_DATA_HOME" "$scratch/w"

if [[ -z ${RTRASH_INTEROP_NO_GIO:-} ]] && command -v gio >/dev/null && [[ -x $GVFSD && -z ${RTRASH_INTEROP_BUS:-} ]]; then
  export RTRASH_INTEROP_BUS=1
  exec dbus-run-session -- bash -c '"$0" & sleep 2; "$1" "$2"; s=$?; kill %1 2>/dev/null; exit $s' \
    "$GVFSD" "$0" "$rtrash"
fi

trash=$XDG_DATA_HOME/Trash
cd "$scratch/w"

have() { command -v "$1" >/dev/null; }
putters=(rtrash)
restorers=(rtrash)
have trash-put && putters+=(trash-cli) && restorers+=(trash-cli)
have gtrash && putters+=(gtrash) && restorers+=(gtrash)
if [[ -z ${RTRASH_INTEROP_NO_GIO:-} ]] && have gio && [[ -n ${RTRASH_INTEROP_BUS:-} ]]; then
  putters+=(gio)
  restorers+=(gio)
else
  echo "note: gio or gvfsd missing; gio pairings skipped"
fi
echo "putters: ${putters[*]}; restorers: ${restorers[*]}"

put() { # TOOL PATH
  case $1 in
    rtrash) "$rtrash" put "$2" ;;
    trash-cli) trash-put "$2" ;;
    gtrash) gtrash put "$2" ;;
    gio) gio trash "$2" ;;
  esac
}

restore() { # TOOL ABSPATH
  case $1 in
    rtrash) "$rtrash" restore --home-only "$2" ;;
    trash-cli) echo 0 | trash-restore "$2" ;;
    gtrash) gtrash restore "$2" ;;
    gio)
      # gio restores by trash URI; `gio trash --list` pairs each with its path.
      local uri
      uri=$(gio trash --list | awk -F'\t' -v p="$2" '$2 == p { print $1; exit }')
      gio trash --restore "$uri"
      ;;
  esac
}

fail=0
for p in "${putters[@]}"; do
  for r in "${restorers[@]}"; do
    f="$scratch/w/$p to $r 100% é"
    echo "$p $r" > "$f"
    if ! put "$p" "$f" >/dev/null 2>&1 || [[ -e $f ]]; then
      echo "FAIL $p put"
      fail=1
      continue
    fi
    if restore "$r" "$f" >/dev/null 2>&1 && [[ $(cat "$f" 2>/dev/null) == "$p $r" ]]; then
      echo "ok   $p -> $r"
    else
      echo "FAIL $p -> $r"
      fail=1
    fi
  done
done

# Nothing may stay behind in the trash after every restore.
left=$(find "$trash/files" "$trash/info" -mindepth 1 2>/dev/null | wc -l)
if [[ $left -ne 0 ]]; then
  echo "FAIL $left entries left in the trash"
  fail=1
fi
cd /
env -u XDG_DATA_HOME "$rtrash" put -r "$scratch" >/dev/null 2>&1 || true
exit $fail
