#!/usr/bin/env bash
# Cross-restore between rtrash and the other FreeDesktop trash tools.
#
#   scripts/interop-smoke.sh RTRASH
#
# Each tool on PATH among trash-cli (trash-put, trash-restore), gtrash and
# gio trashes a file, and every tool restores it: rtrash, trash-cli, gtrash
# and gio, in every pairing. trash-cli and gtrash then remove one rtrash
# item and empty the rest. File names carry a space, a '%' and Unicode.
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

# trash-cli and gtrash remove and empty what rtrash trashed: rm takes one
# item and leaves the rest whole, and empty leaves nothing that rtrash
# still lists or counts. Empty stays pinned to the scratch trash, so no
# real volume trash gets emptied.
tool_rm() { # TOOL ABSPATH
  case $1 in
    # trash-rm reads every trash; the name is unique to this run.
    trash-cli) trash-rm "$(basename "$2")" ;;
    gtrash) gtrash rm -f "$2" ;;
  esac
}
tool_empty() { # TOOL
  case $1 in
    trash-cli) trash-empty -f --trash-dir "$trash" ;;
    gtrash) gtrash prune -f --day 0 --trash-dir "$trash" ;;
  esac
}
count() { "$rtrash" list --home-only | grep -c . || true; }
for t in "${restorers[@]}"; do
  [[ $t == trash-cli || $t == gtrash ]] || continue
  mkdir -p "$scratch/w/$t dir"
  echo d > "$scratch/w/$t dir/inner"
  echo k > "$scratch/w/$t keep 100%.txt"
  echo r > "$scratch/w/$t drop 100%.txt"
  "$rtrash" put -r "$scratch/w/$t dir" "$scratch/w/$t keep 100%.txt" "$scratch/w/$t drop 100%.txt" >/dev/null
  if tool_rm "$t" "$scratch/w/$t drop 100%.txt" >/dev/null 2>&1 \
    && [[ $(count) -eq 2 ]] \
    && ! "$rtrash" list --home-only | grep -qF "drop 100%" \
    && ! "$rtrash" list --home-only | grep -qF "????"; then
    echo "ok   $t rm of an rtrash item"
  else
    echo "FAIL $t rm of an rtrash item"
    fail=1
  fi
  if tool_empty "$t" >/dev/null 2>&1 && [[ $(count) -eq 0 ]] \
    && "$rtrash" status --home-only --json | grep -q '"items":0'; then
    echo "ok   $t empty of rtrash items"
  else
    echo "FAIL $t empty of rtrash items"
    fail=1
  fi
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
