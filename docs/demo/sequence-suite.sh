#!/usr/bin/env bash
# Broader surface demo (non-TUI, hard-pinned to sandbox trash).
# Covers: rm-shaped put, multi-call names, status, empty --dry-run,
# trash-rm / rtrash rm, keys. TUI called out in text only.
set -euo pipefail

: "${RTRASH_DEMO_WORK:?}"
: "${RTRASH_DEMO_PIN:?RTRASH_DEMO_PIN not set — refuse unpinned demo}"
: "${XDG_DATA_HOME:?}"
: "${RTRASH_DEMO_BIN_DIR:?need multi-call link dir}"

run() {
  local cmd=$1
  printf '\n\033[1;36m❯\033[0m \033[1m%s\033[0m\n' "$cmd"
  sleep 0.3
  # shellcheck disable=SC2086
  eval $cmd
  sleep 0.45
}

banner() {
  printf '\n\033[1;33m# %s\033[0m\n' "$1"
  sleep 0.2
}

cd "$RTRASH_DEMO_WORK"
PIN="$RTRASH_DEMO_PIN"
export PATH="${RTRASH_DEMO_BIN_DIR}:$PATH"

banner "rm-shaped put (rtrash -rf … falls through to put)"
mkdir -p build/out
printf 'obj\n' > build/out/a.o
printf 'src\n' > app.c
run 'rtrash -rf build'
run 'rtrash -f app.c'
run "rtrash list $PIN"
run "rtrash status $PIN"

banner "multi-call names (same binary via setup-style links)"
run "ls -1 \"\$RTRASH_DEMO_BIN_DIR\""
printf 'keep-me\n' > keep.txt
printf 'drop.o\n' > drop.o
run 'trash-put keep.txt drop.o'
run "trash-list $PIN"

banner "selective permanent delete (rtrash rm / trash-rm)"
run "rtrash rm -n $PIN '*.o'"
run "rtrash rm $PIN '*.o'"
run "trash-list $PIN"

banner "empty --dry-run then purge leftover"
run "rtrash empty --plain -n $PIN"
run "rtrash empty --plain $PIN"
run "rtrash status $PIN"

banner "customizable TUI keybinds (dump only)"
run 'rtrash keys --path'
run 'rtrash keys --list | head -n 14'

printf '\n\033[1;32m# TUI not recorded: rtrash restore|empty|put on a real TTY\033[0m\n'
printf '\033[2m# multi-select · live fuzzy · ? help · keys.toml\033[0m\n'
sleep 1.0
