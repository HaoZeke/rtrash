#!/usr/bin/env bash
# Compare `rtrash du` totals with independent references on real trees.
#
#   scripts/du-smoke.sh RTRASH TREE...
#
# For each TREE:
#   - disk bytes must match the system du in KiB (`du -sk`, rounded up), and
#     a Python walk that sums st_blocks * 512 once per inode;
#   - apparent bytes must match the same walk summing st_size of every
#     non-directory once per inode (GNU `du -sb` semantics);
#   - the file count must match the walk (every name that is not a directory).
# RTRASH_DU_BULK in the environment passes through, so macOS runs both
# directory readers. Exits non-zero on the first mismatch.
set -euo pipefail

rtrash=$1
shift
status=0
for tree in "$@"; do
  json=$("$rtrash" du --json -n 1 --no-git "$tree")
  read -r disk apparent files < <(
    python3 -c 'import json,sys; r=json.load(sys.stdin)["roots"][0]; print(r["disk"], r["apparent"], r["files"])' <<<"$json"
  )
  read -r ref_disk ref_apparent ref_files < <(
    python3 - "$tree" <<'PY'
import os, stat, sys

seen = set()
disk = apparent = files = 0

def visit(path):
    global disk, apparent, files
    st = os.lstat(path)
    if stat.S_ISDIR(st.st_mode):
        disk += st.st_blocks * 512
        try:
            names = os.listdir(path)
        except OSError:
            return
        for name in names:
            visit(os.path.join(path, name))
        return
    files += 1
    if st.st_nlink > 1:
        key = (st.st_dev, st.st_ino)
        if key in seen:
            return
        seen.add(key)
    disk += st.st_blocks * 512
    apparent += st.st_size

visit(sys.argv[1])
print(disk, apparent, files)
PY
  )
  du_k=$(du -sk "$tree" | cut -f1)
  want_k=$(((disk + 1023) / 1024))
  echo "$tree: rtrash disk=$disk apparent=$apparent files=$files" \
    "| walk disk=$ref_disk apparent=$ref_apparent files=$ref_files | du -sk=$du_k"
  if [ "$disk" != "$ref_disk" ] || [ "$apparent" != "$ref_apparent" ] ||
    [ "$files" != "$ref_files" ] || [ "$du_k" != "$want_k" ]; then
    echo "MISMATCH in $tree" >&2
    status=1
  fi
done
exit $status
