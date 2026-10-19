#!/usr/bin/env python3
"""Audit `rtrash du -x /` against df (35(f), gdu #666 and #639).

    scripts/du-df-audit.py RTRASH

Scans / with -x (as root through `sudo -n` when that works, so few
folders are unreadable) and compares the disk total, plus the deleted
files still open (--open-deleted), with the space df gives as used. It
prints one "du-vs-df:" line per number before it checks the tolerance:

  Linux: du and df agree within 1% of df's used space, or 512 MiB when
    that is more. df also counts what no walk sees: blocks of files
    written or deleted during the scan, folders it may not read, and
    about 80 MB on an 87 GB ext4 root that no stat shows: there the gap
    was 0.10% as root with nothing unreadable, and 0.09% as a plain user
    with 18 folders unreadable. A bind mount walked twice, or a mount
    that -x failed to leave out, would be far outside.
  macOS: du lies within 50% to 150% of the used space of / and
    /System/Volumes/Data together, the system and data volumes that -x
    walks as one device. APFS clones count once in df and once per name
    in du, and local snapshots count only in df, so the bound is wide. A
    firmlinked folder counted twice, or the data volume left out, would
    fall outside it.

Exits 1 outside the tolerance and 2 on a usage or run error.
"""
import json
import os
import platform
import subprocess
import sys

MIB = 1024 * 1024


def run(cmd):
    """The stdout of cmd; exits 2 when it fails."""
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        print(f"du-vs-df: {' '.join(cmd)} failed ({out.returncode}): {out.stderr.strip()}")
        sys.exit(2)
    return out.stdout


def as_root():
    """The prefix that runs a command as root, or none."""
    if os.geteuid() == 0:
        return []
    ok = subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode == 0
    return ["sudo", "-n"] if ok else []


def df_used(mounts):
    """Used bytes of each mount, by `df -Pk`, one per file system."""
    used, seen = {}, set()
    for m in mounts:
        lines = run(["df", "-Pk", m]).strip().splitlines()
        fields = lines[-1].split()
        if fields[0] in seen:
            continue
        seen.add(fields[0])
        used[m] = int(fields[2]) * 1024
    return used


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    rtrash = os.path.abspath(sys.argv[1])
    system = platform.system()
    root = as_root()
    cmd = root + [rtrash, "du", "-x", "-n", "0", "--no-git", "--json"]
    if "--open-deleted" in run([rtrash, "du", "--help"]):
        cmd.append("--open-deleted")
    cmd.append("/")
    j = json.loads(run(cmd))
    disk = j["roots"][0]["disk"]
    held = (j.get("open_deleted") or {}).get("disk", 0)
    mounts = ["/"]
    if system == "Darwin" and os.path.ismount("/System/Volumes/Data"):
        mounts.append("/System/Volumes/Data")
    used = df_used(mounts)
    total = sum(used.values())
    print(f"du-vs-df: system {system}, as {'root' if root or os.geteuid() == 0 else 'user'}")
    print(f"du-vs-df: rtrash du -x / disk {disk}, open-deleted {held}, "
          f"unreadable {j.get('unreadable')}, inodes {j['roots'][0]['inodes']}")
    for m, u in used.items():
        print(f"du-vs-df: df used {m} {u}")
    seen = disk + held
    gap = total - seen
    print(f"du-vs-df: df used {total}, du {seen}, gap {gap} "
          f"({100 * gap / max(total, 1):.2f}% of df)")
    if system == "Darwin":
        ok = total // 2 <= seen <= total + total // 2
        bound = "du within 50% to 150% of df"
    else:
        slack = max(total // 100, 512 * MIB)
        ok = abs(gap) <= slack
        bound = f"gap within {slack} bytes"
    print(f"du-vs-df: {'ok' if ok else 'OUTSIDE'}: {bound}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
