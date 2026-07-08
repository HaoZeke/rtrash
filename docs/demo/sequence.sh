#!/usr/bin/env bash
# Fixed demo command sequence for docs/demo recording.
# Invoked by record.sh inside an isolated XDG trash sandbox.
# Do not edit the GIF by hand — regenerate via ./docs/demo/record.sh
#
# Isolation: every rtrash subcommand that discovers trash is pinned with
# RTRASH_DEMO_PIN (set by record.sh to --trash-dir=$XDG_DATA_HOME/Trash).
# Unpinned list/status/empty would scan volume mounts and can wipe real trash.
set -euo pipefail

: "${RTRASH_DEMO_WORK:?RTRASH_DEMO_WORK not set}"
: "${RTRASH_DEMO_PIN:?RTRASH_DEMO_PIN not set — refuse unpinned demo (volume discovery is unsafe)}"
: "${XDG_DATA_HOME:?XDG_DATA_HOME not set}"

# Pretty prompt + slow enough pacing for a readable cast/GIF.
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

# Pin for list/status/restore/empty (suite commands that scan trash roots).
# put uses XDG home trash for files on this sandbox FS; still pass pin via env
# only where the CLI accepts --trash-dir / --home-only.
PIN="$RTRASH_DEMO_PIN"

banner "rm-compatible put → FreeDesktop trash (sandbox pin: $PIN)"
printf 'payload for demos\n' > notes.txt
printf 'build artifact\n' > stale.o
run 'ls -1'
run 'rtrash put notes.txt stale.o'
run 'ls -1'
run "rtrash list $PIN"
run "rtrash status $PIN"

banner "restore one file (exact original path)"
run "rtrash restore --plain $PIN \"\$(pwd)/notes.txt\""
run 'ls -1'
run 'cat notes.txt'

banner "empty the rest permanently (pinned — only sandbox trash)"
run "rtrash empty --plain $PIN"
run "rtrash list $PIN"
run "rtrash status $PIN"

banner "Python: replace os.remove / shutil.rmtree"
if python3 -c 'import rtrash' 2>/dev/null; then
  run 'python3 -c "import rtrash; print(rtrash.version())"'
else
  printf '\n\033[2m(python bindings not installed in this environment — pip install rtrash)\033[0m\n'
  sleep 0.4
fi

printf '\n\033[1;32m# done — TUI: rtrash restore / empty / put on a TTY · keys: rtrash keys --list\033[0m\n'
sleep 1.2
