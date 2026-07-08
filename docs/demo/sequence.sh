#!/usr/bin/env bash
# Short FreeDesktop loop: put → list/status → restore → empty (pinned sandbox).
# See sequence-suite.sh for rm-flags, multi-call, dry-run, trash-rm, keys.
set -euo pipefail

: "${RTRASH_DEMO_WORK:?RTRASH_DEMO_WORK not set}"
: "${RTRASH_DEMO_PIN:?RTRASH_DEMO_PIN not set — refuse unpinned demo (volume discovery is unsafe)}"
: "${XDG_DATA_HOME:?XDG_DATA_HOME not set}"

run() {
  local cmd=$1
  printf '\n\033[1;36m❯\033[0m \033[1m%s\033[0m\n' "$cmd"
  sleep 0.35
  # shellcheck disable=SC2086
  eval $cmd
  sleep 0.55
}

banner() {
  printf '\n\033[1;33m# %s\033[0m\n' "$1"
  sleep 0.25
}

cd "$RTRASH_DEMO_WORK"
PIN="$RTRASH_DEMO_PIN"

banner "put → list → restore → empty (pinned sandbox)"
printf 'payload for demos\n' > notes.txt
printf 'build artifact\n' > stale.o
run 'ls -1'
run 'rtrash put notes.txt stale.o'
run "rtrash list $PIN"
run "rtrash status $PIN"
run "rtrash restore --plain $PIN \"\$(pwd)/notes.txt\""
run 'cat notes.txt'
run "rtrash empty --plain $PIN"
run "rtrash status $PIN"

printf '\n\033[1;32m# more: suite demo (rm -rf, multi-call, dry-run, trash-rm, keys)\033[0m\n'
printf '\033[2m#   docs/demo/rtrash-suite.gif · TUI on TTY: rtrash restore|empty|put\033[0m\n'
sleep 1.0
