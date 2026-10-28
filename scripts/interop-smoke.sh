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
# With trash-cli, rtrash's trash-list must print trash-cli's rows, odd
# names included, and with RTRASH_INTEROP_VOLUME naming a mount point
# both tools must refuse a volume record that leads out of its volume.
# With trash-cli too, rtrash status --check --fix must clear the records
# a trash-put killed mid-way leaves (trash-cli #414).
# rtrash put must treat d/ as d and refuse ./ and link/ (trash-cli #434).
# Tools that are missing get skipped with a note; rtrash must be there.
# gio needs a session bus and gvfsd, so the script starts both in
# dbus-run-session when gio is on PATH and no gvfsd runs yet.
# Everything happens in a scratch XDG_DATA_HOME, so the real trash stays
# untouched. RTRASH_INTEROP_NO_GIO=1 leaves gio out. A gio that cannot
# restore its own trash is left out the same way, after the probe says so.
# Exits non-zero when any pairing that did run fails.
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
have trash-list && echo "trash-cli: $(trash-list --version 2>&1)"

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
# rtrash one. A gio that cannot restore even an ASCII name is left out.
# A non-ASCII restore that fails fails the script. GLib 2.88 restores
# those names.
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
  # gio 2.88 trashes a file and cannot restore it, ASCII names
  # included. That is gio's own round trip, so the gio pairings are
  # left out and the other tools still run.
  if [[ ${seen["x 100%"]} == FAIL ]]; then
    echo "note: gio cannot restore its own trash (the 'x 100%' round trip failed); gio pairings skipped"
    find "$scratch/w" -maxdepth 1 -name 'probe*' -exec rm -f {} +
    rm -rf "$trash/files" "$trash/info"
    mkdir -p "$trash/files" "$trash/info"
    keep=()
    for x in "${putters[@]}"; do
      [[ $x == gio ]] || keep+=("$x")
    done
    putters=("${keep[@]}")
    keep=()
    for x in "${restorers[@]}"; do
      [[ $x == gio ]] || keep+=("$x")
    done
    restorers=("${keep[@]}")
  elif [[ ${seen["x é"]} == FAIL || ${seen["x 100% é"]} == FAIL ]]; then
    echo "FAIL probe: gio cannot restore a non-ASCII name"
    fail=1
  fi
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
  # A .DS_Store in info/ is no record: trash-cli says nothing of it, and
  # neither does rtrash under the trash-list name.
  tl=$scratch/tl-bad
  mkdir -p "$tl/files" "$tl/info"
  printf 'ok\n' > "$tl/files/ok"
  printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:05\n' "$scratch/w/from/ok" > "$tl/info/ok.trashinfo"
  printf '[Trash Info]\nDeletionDate=2026-01-02T03:04:05\n' > "$tl/info/no path.trashinfo"
  mkdir "$tl/info/dir.trashinfo"
  ln -s "$scratch/nowhere" "$tl/info/dangling.trashinfo"
  printf 'Bud1' > "$tl/info/.DS_Store"
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

  # trash-cli #414: a trash-put that dies between its record and its
  # move leaves an empty record, one cut off before Path=, or a whole
  # one whose payload never came. rtrash status --check --fix takes all
  # three, and trash-list then prints the good row and no error.
  tl=$scratch/tl-crash
  mkdir -p "$tl/files" "$tl/info"
  printf 'ok\n' > "$tl/files/ok"
  printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:05\n' "$scratch/w/from/ok" > "$tl/info/ok.trashinfo"
  : > "$tl/info/empty.trashinfo"
  printf '[Trash Info]\n' > "$tl/info/cut.trashinfo"
  printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:05\n' "$scratch/w/from/gone" > "$tl/info/gone.trashinfo"
  before=$(trash-list --trash-dir="$tl" 2>&1 >/dev/null || true)
  code=0; "$rtrash" status --trash-dir="$tl" --check --fix >/dev/null 2>&1 || code=$?
  after=$(trash-list --trash-dir="$tl" 2>&1 >/dev/null || echo "exit $?")
  rows=$(trash-list --trash-dir="$tl" 2>/dev/null || true)
  if [[ $code == 0 && $before == *"Parse Error"* && -z $after && $rows == *"/w/from/ok" \
    && -f $tl/files/ok && $(ls -A "$tl/info") == ok.trashinfo ]]; then
    echo "ok   status --fix clears the records a crashed trash-put leaves (trash-cli #414)"
  else
    echo "FAIL status --fix and the records a crashed trash-put leaves (exit $code)"
    printf '  before: %s\n  after: %s\n  rows: %s\n' "$before" "$after" "$rows"
    ls -lA "$tl/info" | sed 's/^/  | /'
    fail=1
  fi
  rm -rf -- "$tl"

  # trash-cli #434: a trailing slash must not change what put does.
  # rtrash put -r d/ trashes d, and trash-list lists it as d; ./ and
  # link/ (link points to a directory) are refused and lose nothing.
  tl=$scratch/tl-slash
  sw=$scratch/w/slash
  mkdir -p "$sw/d" "$sw/target"
  sw=$(cd "$sw" && pwd -P)  # put records the resolved path
  printf 'x\n' > "$sw/d/f"
  printf 'y\n' > "$sw/target/g"
  ln -s target "$sw/link"
  code=0; "$rtrash" put -r --trash-dir="$tl" "$sw/d/" >/dev/null 2>&1 || code=$?
  dot=0; (cd "$sw" && "$rtrash" put -rf --trash-dir="$tl" ./ >/dev/null 2>&1) || dot=$?
  lnk=0; "$rtrash" put -rf --trash-dir="$tl" "$sw/link/" >/dev/null 2>&1 || lnk=$?
  rows=$(trash-list --trash-dir="$tl" 2>&1 || echo "exit $?")
  if [[ $code == 0 && $dot == 1 && $lnk == 1 && $rows == *" $sw/d" && $rows != *$'\n'* \
    && -f $tl/files/d/f && -L $sw/link && -f $sw/target/g ]]; then
    echo "ok   put d/ trashes d, and ./ and link/ are refused (trash-cli #434)"
  else
    echo "FAIL put with a trailing slash (put $code, ./ $dot, link/ $lnk)"
    printf '%s\n' "$rows" | sed 's/^/  | /'
    fail=1
  fi
  rm -rf -- "$tl" "$sw"

  # Odd names in rows, as trash-cli 0.26.9 prints them: as is in a pipe,
  # with a byte that is not UTF-8 as \xNN, and in GNU ls's shell-escape
  # quoting on a terminal (through script(1)). Each name gets a trash of
  # its own, so no sorting is needed when a newline splits its row.
  tl=$scratch/tl-odd
  base=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$scratch/w/odd/")
  modes=(pipe)
  if have script; then
    modes+=(tty)
  else
    echo "note: script missing; trash-list terminal quoting check skipped"
  fi
  for enc in plain-1.txt 'a%20b' "it's" 'new%0Aline' 'tab%09here' 'esc%1B%5B31m' 'del%7F' 'nel%C2%85%C3%A9' 'bad%FF'; do
    rm -rf -- "$tl"
    mkdir -p "$tl/files" "$tl/info"
    printf 'x\n' > "$tl/files/item"
    printf '[Trash Info]\nPath=%s%s\nDeletionDate=2026-01-02T03:04:05\n' "$base" "$enc" > "$tl/info/item.trashinfo"
    for mode in "${modes[@]}"; do
      for args in "" "--files"; do
        if [[ $mode == pipe ]]; then
          # shellcheck disable=SC2086
          want=$( { trash-list --trash-dir="$tl" $args 2>&1 || echo "exit $?"; } | od -c)
          # shellcheck disable=SC2086
          got=$( { "$scratch/bin/trash-list" --trash-dir="$tl" $args 2>&1 || echo "exit $?"; } | od -c)
        else
          want=$( { script -qec "trash-list --trash-dir='$tl' $args" /dev/null </dev/null 2>&1 || echo "exit $?"; } | tr -d '\r' | od -c)
          got=$( { script -qec "'$scratch/bin/trash-list' --trash-dir='$tl' $args" /dev/null </dev/null 2>&1 || echo "exit $?"; } | tr -d '\r' | od -c)
        fi
        if [[ $want == "$got" ]]; then
          echo "ok   trash-list $mode ${args:-(plain)} row for '$enc' matches trash-cli"
        else
          echo "FAIL trash-list $mode ${args:-(plain)} row for '$enc' differs from trash-cli"
          echo "  trash-cli:"
          printf '%s\n' "$want" | sed 's/^/  | /'
          echo "  rtrash:"
          printf '%s\n' "$got" | sed 's/^/  | /'
          fail=1
        fi
      done
    done
  done
  rm -rf -- "$tl"
else
  echo "note: trash-list missing; trash-list row check skipped"
fi

# trash-cli 0.26.9 restores nothing from a volume trash whose record
# leads out of its volume: an absolute Path= elsewhere, or a relative
# one that climbs out with `..`. rtrash refuses the same two, and both
# restore the record that stays inside. trash-cli finds the volume of
# a --trash-dir by its mount point, so this needs a real mount other
# than /: RTRASH_INTEROP_VOLUME names one (CI mounts a tmpfs there)
# whose parent directory the user can write.
vol=${RTRASH_INTEROP_VOLUME:-}
if [[ -z $vol ]] || ! have trash-restore || ! mountpoint -q "$vol"; then
  echo "note: RTRASH_INTEROP_VOLUME unset or not a mount point, or trash-restore missing; volume escape check skipped"
else
  vt=$vol/.Trash-$(id -u)
  outside=$(dirname "$vol")
  abs_t=$outside/rtrash-interop-abs.txt
  climb_t=$outside/rtrash-interop-climb.txt
  ok_t=$vol/rtrash-interop-ok.txt
  for tool in trash-cli rtrash; do
    rm -rf -- "$vt" "$abs_t" "$climb_t" "$ok_t"
    mkdir -p "$vt/files" "$vt/info"
    for rec in "ok|rtrash-interop-ok.txt" "abs|$abs_t" "climb|../rtrash-interop-climb.txt"; do
      printf 'payload\n' > "$vt/files/${rec%%|*}"
      printf '[Trash Info]\nPath=%s\nDeletionDate=2026-01-02T03:04:05\n' "${rec#*|}" > "$vt/info/${rec%%|*}.trashinfo"
    done
    for case in "abs|$abs_t" "climb|$climb_t" "ok|$ok_t"; do
      name=${case%%|*}
      target=${case#*|}
      if [[ $tool == trash-cli ]]; then
        log=$(printf '0\n' | trash-restore --trash-dir="$vt" "$target" 2>&1 || true)
      else
        log=$("$rtrash" restore --trash-dir="$vt" "$target" </dev/null 2>&1 || true)
      fi
      if [[ $name == ok ]]; then
        if [[ -f $target && ! -e $vt/files/ok ]]; then
          echo "ok   $tool restores the record inside its volume"
        else
          echo "FAIL $tool did not restore the record inside its volume"
          printf '%s\n' "$log" | sed 's/^/  | /'
          fail=1
        fi
      elif [[ ! -e $target && -f $vt/files/$name ]]; then
        echo "ok   $tool refuses the $name Path= that leads out of its volume"
      else
        echo "FAIL $tool restored the $name Path= that leads out of its volume"
        printf '%s\n' "$log" | sed 's/^/  | /'
        fail=1
      fi
    done
  done
  rm -rf -- "$vt" "$abs_t" "$climb_t" "$ok_t"

  # trash-cli #433: a `.Trash-$uid` that is a symlink, that other users
  # can write to, or that another user owns is skipped by list and put.
  # list reads every mount and only reports, so the real trash stays
  # untouched; put falls back to the scratch home trash.
  bait=$scratch/planted
  put_t=$vol/rtrash-interop-planted.txt
  for how in symlink writable foreign; do
    rm -rf -- "$bait" "$vt" "$put_t"
    case $how in
      symlink) rec=$bait; mkdir -p "$bait"; ln -s "$bait" "$vt" ;;
      *) rec=$vt ;;
    esac
    mkdir -p "$rec/files" "$rec/info"
    printf 'payload\n' > "$rec/files/planted"
    printf '[Trash Info]\nPath=rtrash-interop-planted-rec\nDeletionDate=2026-01-02T03:04:05\n' \
      > "$rec/info/planted.trashinfo"
    case $how in
      writable) chmod 0777 "$vt" ;;
      foreign)
        if ! sudo -n chown -R 65534 "$vt" 2>/dev/null; then
          echo "note: sudo -n chown unavailable; foreign-owner .Trash-\$uid check skipped"
          rm -rf -- "$vt"
          continue
        fi ;;
    esac
    printf 'mine\n' > "$put_t"
    list_out=$("$rtrash" list 2>"$scratch/planted.err" || true)
    put_log=$("$rtrash" put "$put_t" 2>&1 || true)
    if [[ $list_out != *planted-rec* ]] && grep -qF "skipping '$vt'" "$scratch/planted.err" \
      && [[ ! -e $put_t && -f $trash/files/rtrash-interop-planted.txt ]] \
      && [[ $(ls -A "$rec/files") == planted ]]; then
      echo "ok   rtrash list and put skip a planted .Trash-\$uid ($how; trash-cli #433)"
    else
      echo "FAIL rtrash used a planted .Trash-\$uid ($how; trash-cli #433)"
      sed 's/^/  | /' "$scratch/planted.err"
      printf '%s\n' "$list_out" "$put_log" | sed 's/^/  | /'
      fail=1
    fi
    rm -f -- "$trash/files/rtrash-interop-planted.txt" "$trash/info/rtrash-interop-planted.txt.trashinfo"
    if [[ $how == foreign ]]; then
      sudo -n rm -rf -- "$vt"
    fi
    rm -rf -- "$bait" "$vt" "$put_t" "$scratch/planted.err"
  done
fi

# trash-cli 0.26.9's trash-put refuses a trash whose files/ or info/ is
# a symbolic link and exits 74; rtrash under the trash-put name does the
# same. The file stays, and the folder the link points at stays empty.
if have trash-put; then
  mkdir -p "$scratch/bin"
  ln -sf "$rtrash" "$scratch/bin/trash-put"
  lt=$scratch/linked
  for sub in files info; do
    for tool in trash-cli rtrash; do
      rm -rf -- "$lt"
      mkdir -p "$lt/T" "$lt/elsewhere"
      ln -s "$lt/elsewhere" "$lt/T/$sub"
      printf 'keep\n' > "$lt/a.txt"
      if [[ $tool == trash-cli ]]; then
        code=0; trash-put --trash-dir="$lt/T" "$lt/a.txt" 2>/dev/null || code=$?
      else
        code=0; "$scratch/bin/trash-put" --trash-dir="$lt/T" "$lt/a.txt" 2>/dev/null || code=$?
      fi
      if [[ $code == 74 && -f $lt/a.txt && -z $(ls -A "$lt/elsewhere") ]]; then
        echo "ok   $tool trash-put refuses a trash whose $sub/ is a symlink"
      else
        echo "FAIL $tool trash-put with a symlinked $sub/ (exit $code)"
        ls -lA "$lt" "$lt/elsewhere" | sed 's/^/  | /'
        fail=1
      fi
    done
  done
  rm -rf -- "$lt"
else
  echo "note: trash-put missing; symlinked trash folder check skipped"
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

# trash-cli #412: trash-list leaves out the `.Trash` folders of mounts
# it takes for virtual, such as virtiofs, 9p, fuse.sshfs and overlay.
# rtrash lists every mount that holds files. A ramfs and an overlay
# mount, made here when sudo -n allows, stand in for the ones CI cannot
# make: each must be listed, and put must keep a file on it in its own
# `.Trash-$uid`.
fsroot=$scratch/fs
for kind in ramfs overlay; do
  m=$fsroot/$kind
  mkdir -p "$m" "$fsroot/lower" "$fsroot/upper" "$fsroot/work"
  m=$(cd "$m" && pwd -P)
  if [[ $kind == ramfs ]]; then
    made=$(sudo -n mount -t ramfs -o mode=0755 ramfs "$m" 2>/dev/null \
      && sudo -n chown "$(id -u):$(id -g)" "$m" && echo yes || true)
  else
    made=$(sudo -n mount -t overlay overlay \
      -o "lowerdir=$fsroot/lower,upperdir=$fsroot/upper,workdir=$fsroot/work" "$m" 2>/dev/null \
      && echo yes || true)
  fi
  if [[ $made != yes ]]; then
    echo "note: sudo -n mount -t $kind unavailable; $kind volume check skipped"
    if mountpoint -q "$m"; then sudo -n umount "$m"; fi
    continue
  fi
  f=$m/rtrash-interop-fs.txt
  vt_k=$m/.Trash-$(id -u)
  printf 'on %s\n' "$kind" > "$f"
  vols=$("$rtrash" list --volumes 2>&1 || true)
  log=$("$rtrash" put "$f" 2>&1 || true)
  if grep -qxF "$m" <<<"$vols" && [[ ! -e $f && -f $vt_k/files/rtrash-interop-fs.txt ]]; then
    echo "ok   rtrash lists the $kind mount and puts a file there in its own .Trash-\$uid (trash-cli #412)"
  else
    echo "FAIL rtrash missed the $kind mount at $m (trash-cli #412)"
    printf '%s\n' "$vols" "$log" | sed 's/^/  | /'
    fail=1
  fi
  if have trash-list; then
    vols_tc=$(trash-list --volumes 2>/dev/null || true)
    if ! grep -qxF "$m" <<<"$vols_tc"; then
      echo "info: trash-list --volumes leaves out the $kind mount (trash-cli #412)"
    fi
  fi
  rm -rf -- "$vt_k" "$f"
  sudo -n umount "$m"
done
rm -rf -- "$fsroot"

# Nothing may stay behind in the trash after every restore.
left=$(find "$trash/files" "$trash/info" -mindepth 1 2>/dev/null | wc -l)
if [[ $left -ne 0 ]]; then
  echo "FAIL $left entries left in the trash"
  fail=1
fi
cd /
env -u XDG_DATA_HOME "$rtrash" put -r "$scratch" >/dev/null 2>&1 || true
exit $fail
