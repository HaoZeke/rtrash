#!/bin/bash
set -e
unset XDG_DATA_HOME
# Usage: RTRASH_BIN=target/release/rtrash benches/million_cold.sh [DISK_DIR]
# Cold runs need passwordless sudo to drop the page cache.
B=${RTRASH_BIN:-target/release/rtrash}
t(){ local s=$(date +%s%N); local r=0; "$@" >/dev/null 2>&1 || r=$?; local e=$(( ($(date +%s%N)-s)/1000000 )); printf '%-70s %7d ms rc=%d\n' "${*:2}" $e $r; }
mk(){ python3 - "$1" "$2" <<'P'
import os,sys
root,n=sys.argv[1],int(sys.argv[2])
for d in range(n//1000):
    p=f"{root}/d{d:04}"; os.makedirs(p)
    for f in range(1000):
        with open(f"{p}/f{f:04}","wb") as h: h.write(b"x"*(f%7*100))
P
}
echo "== tmpfs, 1M files"
TS=/dev/shm/.Trash-$(id -u); mkdir -p -m 700 $TS/files $TS/info
S=/dev/shm/b75-$$; mkdir -p $S; mk $S/tree 1000000
t $B du -n 5 $S/tree
t $B du --inodes $S/tree
t $B du --dupes --min-size=1 $S/tree
t $B put -r --trash-dir=$TS $S/tree
t $B trash-stats --trash-dir=$TS --total --measure
t $B trash-stats --trash-dir=$TS --top=0
t $B empty --trash-dir=$TS -f
echo "== disk, 200k files, cold (drop_caches) vs warm"
D=${1:-$PWD/b75}; mkdir -p $D; mk $D/tree 200000
for c in "du -n 5" "du --inodes" "du --dupes --min-size=1"; do
  sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'; t $B $c $D/tree
  t $B $c $D/tree
done
T=$D/.Trash-b75; mkdir -p -m 700 $T/files $T/info
t $B put -r --trash-dir=$T $D/tree
sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'; t $B trash-stats --trash-dir=$T --total --measure
t $B trash-stats --trash-dir=$T --total
sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'; t $B empty --trash-dir=$T -f
