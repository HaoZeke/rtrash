#!/usr/bin/env bash
# Fixed demo command sequence for docs/demo recording.
# Invoked by record.sh inside an isolated XDG trash sandbox.
# Do not edit the GIF by hand — regenerate via ./docs/demo/record.sh
set -euo pipefail

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

cd "${RTRASH_DEMO_WORK:?RTRASH_DEMO_WORK not set}"

banner "rm-compatible put → FreeDesktop trash"
printf 'payload for demos\n' > notes.txt
printf 'build artifact\n' > stale.o
run 'ls -1'
run 'rtrash put notes.txt stale.o'
run 'ls -1'
run 'rtrash list'
run 'rtrash status'

banner "restore one file (exact original path)"
run 'rtrash restore --plain "$(pwd)/notes.txt"'
run 'ls -1'
run 'cat notes.txt'

banner "empty the rest permanently"
run 'rtrash empty --plain'
run 'rtrash list'
run 'rtrash status'

banner "Python: replace os.remove / shutil.rmtree"
printf 'import rtrash\nprint("rtrash", rtrash.version())\n' > /tmp/rtrash-demo-py.py 2>/dev/null || true
# Keep Python optional — CLI story is enough if import missing.
if python3 -c 'import rtrash' 2>/dev/null; then
  run 'python3 -c "import rtrash; print(rtrash.version())"'
else
  printf '\n\033[2m(python bindings not installed in this environment — pip install rtrash)\033[0m\n'
  sleep 0.4
fi

printf '\n\033[1;32m# done — TUI: rtrash restore / empty / put on a TTY · keys: rtrash keys --list\033[0m\n'
sleep 1.2
