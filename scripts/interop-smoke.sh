#!/usr/bin/env bash
# Cross-restore between rtrash and the other FreeDesktop trash tools.
#
#   scripts/interop-smoke.sh RTRASH
#
# Each tool on PATH among trash-cli (trash-put, trash-restore), gtrash and
# gio trashes a file, and every tool restores it: rtrash, trash-cli, gtrash
# and gio, in every pairing. trash-cli and gtrash then remove one rtrash
# item and empty the rest; gio does too when RTRASH_INTEROP_GIO_EMPTY=1,
# since `gio trash --empty` also empties every volume trash it finds. File names carry a space, a '%' and Unicode.
# With ncdu 2.7 or later on PATH, `rtrash du -f` must read its `ncdu -c`
# export as it reads the plain one.
# Tools that are missing get skipped with a note; rtrash must be there.
# gio needs a session bus and gvfsd, so the script starts both in
# dbus-run-session when gio is on PATH and no gvfsd runs yet.
# Everything happens in a scratch XDG_DATA_HOME, so the real trash stays
# untouched. RTRASH_INTEROP_NO_GIO=1 leaves gio out. Exits non-zero when any pairing fails.
set -euo pipefail
# grep -q stops reading at the first match, and rtrash then exits 141 on
# the closed pipe, which pipefail counts as a failure; read it all instead.
has() { grep "$@" >/dev/null; }
# In the C or POSIX locale GLib escapes non-ASCII bytes when it decodes a
# trash:// URI, so gio restores 'é' as a literal '\xc3\xa9'.
export LANG=C.UTF-8 LC_ALL=C.UTF-8 G_FILENAME_ENCODING=UTF-8

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
  exec dbus-run-session -- bash -c '"$0" 2>"$3" & sleep 2; "$1" "$2"; s=$?; kill %1 2>/dev/null; exit $s' \
    "$GVFSD" "$0" "$rtrash" "$scratch/gvfsd.log"
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

# The trash URI gio gives the item that came from ABSPATH. `gio trash
# --list` pairs each URI with its path, but the GLib on Ubuntu 24.04
# prints a non-ASCII path in another form than the one passed in, so
# fall back to the scratch trash's own records and build the URI from
# the record's name.
gio_uri() { # ABSPATH
  local uri
  uri=$(gio trash --list | awk -F'\t' -v p="$1" '$2 == p { print $1; exit }')
  if [[ -z $uri ]]; then
    uri=$(python3 - "$trash/info" "$1" <<'PY'
import os, sys, urllib.parse
info, want = sys.argv[1], sys.argv[2]
for f in sorted(os.listdir(info)):
    if not f.endswith(".trashinfo"):
        continue
    for line in open(os.path.join(info, f), encoding="utf-8", errors="replace"):
        if line.startswith("Path=") and urllib.parse.unquote(line[5:].strip()) == want:
            print("trash:///" + urllib.parse.quote(f[: -len(".trashinfo")]))
            sys.exit(0)
PY
    )
  fi
  [[ -n $uri ]] && echo "$uri"
}

restore() { # TOOL ABSPATH
  case $1 in
    rtrash) "$rtrash" restore --home-only "$2" ;;
    trash-cli) echo 0 | trash-restore "$2" ;;
    gtrash) gtrash restore "$2" ;;
    gio)
      # gio restores by trash URI; `gio trash --list` pairs each with its path.
      local uri
      uri=$(gio_uri "$2") || { echo "no trash URI for $2"; return 1; }
      gio trash --restore "$uri"
      ;;
  esac
}

fail=0
# gio's own round trip, by name shape, to tell a gvfs restore bug from an
# rtrash one. On Ubuntu 24.04 (GLib 2.80.0, gvfs 1.54.4) gio restored
# 'x é' as 'x \xc3\xa9' even between gio and gio, under a UTF-8 locale,
# while 'x 100%' came back intact.
gio_utf8_bug=0
if [[ " ${restorers[*]} " == *" gio "* ]]; then
  echo "gio: $(gio --version 2>&1)"
  if command -v dpkg-query >/dev/null; then
    dpkg-query -W -f='  ${Package} ${Version}\n' 'libglib2.0-0*' 'libglib2.0-bin' gvfs gvfs-daemons 2>/dev/null || true
  fi
  probe() { # NAME -> 0 when gio trashes and restores it intact
    local f="$scratch/w/probe $1"
    echo probe > "$f"
    gio trash "$f" 2>/dev/null || return 1
    local uri
    uri=$(gio_uri "$f") || return 1
    gio trash --restore "$uri" 2>/dev/null
    [[ $(cat "$f" 2>/dev/null) == probe ]] && rm -f -- "$f"
  }
  declare -A seen=()
  for n in "x é" "x 100%" "x 100% é"; do
    if probe "$n"; then seen[$n]=ok; else seen[$n]=FAIL; fi
    echo "probe gio -> gio '$n': ${seen[$n]}"
  done
  ls -b "$scratch/w" | grep '^probe' | sed 's/^/  left: /' || true
  # An ASCII name gio cannot restore is no known bug: fail.
  if [[ ${seen["x 100%"]} == FAIL ]]; then
    echo "FAIL probe: gio cannot restore the ASCII name 'x 100%'"
    fail=1
  fi
  if [[ ${seen["x é"]} == FAIL ]]; then
    gio_utf8_bug=1
    echo "note: this gio cannot restore a non-ASCII name; gio restores of the non-ASCII pairing names count as known failures (xfail), and an ASCII pairing name stays strict"
  elif [[ -n ${RTRASH_INTEROP_EXPECT_GIO_UTF8_BUG:-} ]]; then
    echo "FAIL probe: gio now restores non-ASCII names; drop RTRASH_INTEROP_EXPECT_GIO_UTF8_BUG and the xfail"
    fail=1
  fi
fi

# With the gio bug, every putter still pairs with a gio restore on an
# ASCII name, strictly.
if [[ $gio_utf8_bug == 1 ]]; then
  for p in "${putters[@]}"; do
    f="$scratch/w/$p to gio ascii 100%"
    echo "$p gio" > "$f"
    if put "$p" "$f" >/dev/null 2>&1 && ! [[ -e $f ]] \
      && restore gio "$f" >/dev/null 2>&1 && [[ $(cat "$f" 2>/dev/null) == "$p gio" ]]; then
      echo "ok   $p -> gio (ASCII name)"
    else
      echo "FAIL $p -> gio (ASCII name)"
      fail=1
    fi
  done
fi

for p in "${putters[@]}"; do
  for r in "${restorers[@]}"; do
    f="$scratch/w/$p to $r 100% é"
    echo "$p $r" > "$f"
    if ! put "$p" "$f" >/dev/null 2>&1 || [[ -e $f ]]; then
      echo "FAIL $p put"
      fail=1
      continue
    fi
    before=$(ls -b "$trash/info" 2>&1 || true)
    uri_before=$( [[ $r == gio ]] && gio_uri "$f" 2>&1 || true)
    if out=$(restore "$r" "$f" 2>&1; echo "exit $?") && [[ $(cat "$f" 2>/dev/null) == "$p $r" ]]; then
      echo "ok   $p -> $r"
    elif [[ $r == gio && $gio_utf8_bug == 1 ]]; then
      echo "xfail $p -> $r (gio's own restore bug for non-ASCII names, see the probe above)"
      ls -b "$scratch/w" 2>&1 | sed 's/^/  | /' || true
      # Clear what gio left so later pairings start clean.
      for left in "$scratch/w/$p to $r"*; do
        [[ -e $left ]] && rm -f -- "$left"
      done
    else
      echo "FAIL $p -> $r"
      printf '%s\n' "$out" | sed 's/^/  | /'
      echo "  trash before the restore:"
      printf '%s\n' "$before" | sed 's/^/  | /'
      echo "  work folder after (ls -b):"
      ls -b "$scratch/w" 2>&1 | sed 's/^/  | /' || true
      if [[ $r == gio ]]; then
        echo "  uri before: $uri_before"
        echo "  uri now: $(gio_uri "$f" 2>&1 || true)"
        echo "  gio trash --list:"
        gio trash --list 2>&1 | sed 's/^/  | /' || true
        echo "  $trash/info:"
        ls "$trash/info" 2>&1 | sed 's/^/  | /' || true
      fi
      fail=1
    fi
  done
done

# trash-cli, gtrash and gio remove and empty what rtrash trashed: rm takes one
# item and leaves the rest whole, and empty leaves nothing that rtrash
# still lists or counts. Empty stays pinned to the scratch trash, so no
# real volume trash gets emptied.
tool_rm() { # TOOL ABSPATH
  case $1 in
    # trash-rm reads every trash; the name is unique to this run.
    trash-cli) trash-rm "$(basename "$2")" ;;
    gtrash) gtrash rm -f "$2" ;;
    # gio deletes one trash URI; `gio trash --list` pairs each with its path.
    gio) gio remove "$(gio_uri "$2")" ;;
  esac
}
tool_empty() { # TOOL
  case $1 in
    trash-cli) trash-empty -f --trash-dir "$trash" ;;
    gtrash) gtrash prune -f --day 0 --trash-dir "$trash" ;;
    # gio empties every trash it knows, volume trashes included, and takes
    # no pin; the caller allows it only on a throwaway machine.
    gio) gio trash --empty ;;
  esac
}
count() { "$rtrash" list --home-only | grep -c . || true; }
for t in "${restorers[@]}"; do
  case $t in
    trash-cli | gtrash) ;;
    gio) [[ -n ${RTRASH_INTEROP_GIO_EMPTY:-} ]] || {
      echo "note: gio empty skipped (set RTRASH_INTEROP_GIO_EMPTY=1 on a throwaway machine)"
      continue
    } ;;
    *) continue ;;
  esac
  mkdir -p "$scratch/w/$t dir"
  echo d > "$scratch/w/$t dir/inner"
  echo k > "$scratch/w/$t keep 100%.txt"
  echo r > "$scratch/w/$t drop 100%.txt"
  "$rtrash" put -r "$scratch/w/$t dir" "$scratch/w/$t keep 100%.txt" "$scratch/w/$t drop 100%.txt" >/dev/null
  if tool_rm "$t" "$scratch/w/$t drop 100%.txt" >/dev/null 2>&1 \
    && [[ $(count) -eq 2 ]] \
    && ! "$rtrash" list --home-only | has -F "drop 100%" \
    && ! "$rtrash" list --home-only | has -F "????"; then
    echo "ok   $t rm of an rtrash item"
  else
    echo "FAIL $t rm of an rtrash item"
    fail=1
  fi
  if tool_empty "$t" >/dev/null 2>&1 && [[ $(count) -eq 0 ]] \
    && "$rtrash" status --home-only --json | has '"items":0'; then
    echo "ok   $t empty of rtrash items"
  else
    echo "FAIL $t empty of rtrash items"
    fail=1
  fi
done

# trash-cli's trash-list and rtrash's read one pinned trash and print the
# same rows, plain and with --size and trash-cli's hidden --files: a file,
# a directory (its own size), a dangling link (0) and a link to a file.
# trash-cli lists in directory order, so both sides get sorted.
if have trash-list; then
  tl=$scratch/tl
  mkdir -p "$tl/files/d" "$tl/info" "$scratch/bin"
  ln -sf "$rtrash" "$scratch/bin/trash-list"
  printf 'hello\n' > "$tl/files/a 100%.txt"
  head -c 5000 /dev/zero > "$tl/files/d/big"
  ln -s "$scratch/nowhere" "$tl/files/dl"
  ln -s "a 100%.txt" "$tl/files/gl"
  n=0
  for f in "a 100%.txt" d dl gl; do
    n=$((n + 1))
    enc=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$scratch/w/from/$f")
    printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:0%s\n' "$enc" "$n" > "$tl/info/$f.trashinfo"
  done
  for args in "" "--size" "--files" "--size --files"; do
    # $args splits on purpose: it carries zero, one or two options.
    # shellcheck disable=SC2086
    want=$( { trash-list --trash-dir="$tl" $args 2>&1 || echo "exit $?"; } | LC_ALL=C sort)
    # shellcheck disable=SC2086
    got=$( { "$scratch/bin/trash-list" --trash-dir="$tl" $args 2>&1 || echo "exit $?"; } | LC_ALL=C sort)
    if [[ $want == "$got" ]]; then
      echo "ok   trash-list ${args:-(plain)} rows match trash-cli"
    else
      echo "FAIL trash-list ${args:-(plain)} rows differ from trash-cli"
      echo "  trash-cli:"
      printf '%s\n' "$want" | sed 's/^/  | /'
      echo "  rtrash:"
      printf '%s\n' "$got" | sed 's/^/  | /'
      fail=1
    fi
  done
  rm -rf -- "$tl"

  # Records trash-cli reports on stderr: one with no Path= line, a
  # directory and a dangling symlink where a record should be. Both
  # tools print the same lines (rtrash adds no hint for them) and the
  # same rows; trash-cli's go in directory order, so both get sorted.
  tl=$scratch/tl-bad
  mkdir -p "$tl/files" "$tl/info"
  printf 'ok\n' > "$tl/files/ok"
  printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:05\n' "$scratch/w/from/ok" > "$tl/info/ok.trashinfo"
  printf '[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n' > "$tl/info/no path.trashinfo"
  mkdir "$tl/info/dir.trashinfo"
  ln -s "$scratch/nowhere" "$tl/info/dangling.trashinfo"
  for stream in stdout stderr; do
    if [[ $stream == stdout ]]; then
      want=$( { trash-list --trash-dir="$tl" 2>/dev/null || echo "exit $?"; } | LC_ALL=C sort)
      got=$( { "$scratch/bin/trash-list" --trash-dir="$tl" 2>/dev/null || echo "exit $?"; } | LC_ALL=C sort)
    else
      want=$( { trash-list --trash-dir="$tl" 2>&1 >/dev/null || echo "exit $?"; } | LC_ALL=C sort)
      got=$( { "$scratch/bin/trash-list" --trash-dir="$tl" 2>&1 >/dev/null || echo "exit $?"; } | LC_ALL=C sort)
    fi
    if [[ $want == "$got" ]]; then
      echo "ok   trash-list $stream beside bad records matches trash-cli"
    else
      echo "FAIL trash-list $stream beside bad records differs from trash-cli"
      echo "  trash-cli:"
      printf '%s\n' "$want" | sed 's/^/  | /'
      echo "  rtrash:"
      printf '%s\n' "$got" | sed 's/^/  | /'
      fail=1
    fi
  done
  rm -rf -- "$tl"
else
  echo "note: trash-list missing; trash-list row check skipped"
fi

# ncdu 2.7 and later compress a JSON export with -c. One scan exported
# both ways must list the same in `rtrash du -f` and diff clean in
# `du diff`; the compressed file must start with the zstd magic.
if have ncdu && ncdu --help 2>&1 | has -e '--compress'; then
  nc=$scratch/ncdu
  mkdir -p "$nc/tree/a b/c" "$nc/tree/d"
  head -c 70000 /dev/urandom > "$nc/tree/a b/c/big"
  printf 'x\n' > "$nc/tree/d/small 100%.txt"
  ln "$nc/tree/d/small 100%.txt" "$nc/tree/a b/hard"
  ncdu --ignore-config -0 -e -o "$nc/plain.json" "$nc/tree" >/dev/null
  ncdu --ignore-config -0 -e -c -o "$nc/packed" "$nc/tree" >/dev/null
  magic=$(od -An -tx1 -N4 "$nc/packed" | tr -d ' \n')
  want=$("$rtrash" du -f "$nc/plain.json" --json 2>&1 || echo "exit $?")
  got=$("$rtrash" du -f "$nc/packed" --json 2>&1 || echo "exit $?")
  if [[ $magic == 28b52ffd && $want == "$got" && $got == *'"files":2'* ]]; then
    echo "ok   du -f reads an ncdu -c export like its plain form"
  else
    echo "FAIL du -f reads an ncdu -c export differently (magic $magic)"
    printf '  plain: %s\n  ncdu -c: %s\n' "$want" "$got"
    fail=1
  fi
  if "$rtrash" du diff "$nc/plain.json" "$nc/packed" 2>&1 | has -F "No directory changed size."; then
    echo "ok   du diff of an ncdu -c export and its plain form is clean"
  else
    echo "FAIL du diff of an ncdu -c export and its plain form"
    "$rtrash" du diff "$nc/plain.json" "$nc/packed" 2>&1 | sed 's/^/  | /' || true
    fail=1
  fi
  rm -rf -- "$nc"
else
  echo "note: ncdu 2.7 or later missing; ncdu -c import check skipped"
fi

# Nothing may stay behind in the trash after every restore.
left=$(find "$trash/files" "$trash/info" -mindepth 1 2>/dev/null | wc -l)
if [[ $left -ne 0 ]]; then
  echo "FAIL $left entries left in the trash"
  fail=1
fi
cd /
env -u XDG_DATA_HOME "$rtrash" put -r "$scratch" >/dev/null 2>&1 || true
exit $fail
