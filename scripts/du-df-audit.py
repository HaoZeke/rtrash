#!/usr/bin/env python3
"""Audit `rtrash du -x /` against df (35(f), gdu #666 and #639).

    scripts/du-df-audit.py RTRASH
    scripts/du-df-audit.py --self-test

Scans / with -x (as root through `sudo -n` when that works, so few
folders are unreadable) and compares the disk total, plus the deleted
files still open (--open-deleted), with the used space df gives for the
file systems that -x walks. Before any check it prints every input it
reads on "du-vs-df:" lines: each df row, the device number of each
mount, which mounts count, and the du totals. It also prints, without
checking it, what the system du -x -s gives for / (BSD du on macOS). A missing input is then
plain in the log, and an expected df row that is absent exits 2.

Which file systems -x walks: rtrash du -x, like BSD du (fts FTS_XDEV),
GNU du, ncdu and dua, stays on the device number (st_dev) of PATH. So
the used space to compare is the sum over the mounts whose st_dev is the
root's, one per file system. On Linux that is / (a bind mount of the
same file system counts once). On macOS the system and data volumes of
the boot APFS volume group report one st_dev on the CI runners (the
macOS test du_x_at_the_macos_root_reaches_users_once asserts it), so -x /
walks both, and both count. Preboot, Recovery, VM and Update have their
own st_dev, so -x leaves them out and so does the sum. Were the data
volume on a device of its own, -x would stop at the firmlinks, as du -x
does, and the sum would be / alone.

Expected df rows: / everywhere, and /System/Volumes/Data on macOS. Round
272b read the data volume through os.path.ismount, which is False there
(the mount shares its parent's st_dev), so its row never printed and the
check compared 231 GB of du with the 12.6 GB of / alone.

Tolerance:
  Linux: du and df agree within 1% of df's used space, or 512 MiB when
    that is more. df also counts what no walk sees: blocks of files
    written or deleted during the scan, folders it may not read, and
    about 80 MB on an 87 GB ext4 root that no stat shows.
  macOS: du lies within 50% to 150% of the summed used space. APFS
    clones count once in df and once per name in du, and local snapshots
    count only in df, so the bound is wide. A firmlinked folder counted
    twice, or the data volume left out, falls outside it.

Exits 1 outside the tolerance and 2 on a usage or run error or a
missing df row. --self-test checks the parsers and the decisions on
canned df output and exits 0 when they hold.
"""
import json
import os
import platform
import re
import subprocess
import sys

MIB = 1024 * 1024
DATA = "/System/Volumes/Data"
# A `df -Pk` row; the file system name ("map auto_home") and the mount
# point ("/Volumes/My Disk") may hold spaces.
DF_ROW = re.compile(r"^(?P<fs>.+?)\s+(?P<blocks>\d+)\s+(?P<used>\d+)\s+(?P<avail>\d+)\s+"
                    r"(?P<cap>\d+%|-)\s+(?P<mount>/.*)$")


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


def parse_df(text):
    """(fs, blocks, used bytes, mount) of each `df -Pk` row; the header
    and any line that is not a row are left out."""
    rows = []
    for line in text.splitlines():
        m = DF_ROW.match(line.strip())
        if m:
            rows.append((m["fs"], int(m["blocks"]), int(m["used"]) * 1024, m["mount"]))
    return rows


def walked(rows, devs, root_dev):
    """The rows -x walks from /: a mount with blocks whose device number
    is the root's, the first row of each file system."""
    out, seen = [], set()
    for fs, blocks, used, mount in rows:
        if blocks and devs.get(mount) == root_dev and fs not in seen:
            seen.add(fs)
            out.append((fs, blocks, used, mount))
    return out


def missing(system, rows):
    """The expected mounts that no df row names."""
    want = ["/"] + ([DATA] if system == "Darwin" else [])
    have = {mount for _, _, _, mount in rows}
    return [m for m in want if m not in have]


def verdict(system, total, seen):
    """(ok, bound) for du `seen` against df `total`."""
    if system == "Darwin":
        return total // 2 <= seen <= total + total // 2, "du within 50% to 150% of df"
    slack = max(total // 100, 512 * MIB)
    return abs(total - seen) <= slack, f"gap within {slack} bytes"


MAC_DF = """Filesystem     1024-blocks      Used Available Capacity  Mounted on
/dev/disk3s1s1   367371064  12342820 136093628     9%    /
devfs                  217       217         0   100%    /dev
/dev/disk3s6     367371064   3145748 136093628     3%    /System/Volumes/VM
/dev/disk3s2     367371064   6512340 136093628     5%    /System/Volumes/Preboot
/dev/disk3s5     367371064 213000000 136093628    62%    /System/Volumes/Data
map auto_home            0         0         0   100%    /System/Volumes/Data/home
/dev/disk5s1        102400      2048    100352     2%    /Volumes/My Disk
"""


def self_test():
    """The parsers and decisions on canned df output; a list of faults."""
    faults = []
    rows = parse_df(MAC_DF)
    if [r[3] for r in rows][-2:] != [f"{DATA}/home", "/Volumes/My Disk"]:
        faults.append(f"parse_df mounts: {[r[3] for r in rows]}")
    devs = {"/": 16777233, DATA: 16777233, "/System/Volumes/VM": 16777234,
            "/System/Volumes/Preboot": 16777235, "/Volumes/My Disk": 16777240}
    got = [r[3] for r in walked(rows, devs, 16777233)]
    if got != ["/", DATA]:
        faults.append(f"walked on one volume group device: {got}")
    alone = dict(devs, **{DATA: 16777236})
    got = [r[3] for r in walked(rows, alone, 16777233)]
    if got != ["/"]:
        faults.append(f"walked with the data volume on its own device: {got}")
    if missing("Darwin", rows):
        faults.append(f"missing on full macOS rows: {missing('Darwin', rows)}")
    no_data = [r for r in rows if r[3] != DATA]
    if missing("Darwin", no_data) != [DATA]:
        faults.append(f"missing without the data row: {missing('Darwin', no_data)}")
    if missing("Linux", no_data):
        faults.append("missing on Linux wants more than /")
    # Round 272b on macos-latest: du 231711145984 + 307200 against / alone.
    seen = 231711145984 + 307200
    if verdict("Darwin", 12639047680, seen)[0]:
        faults.append("verdict passes du against / alone on macOS")
    if not verdict("Darwin", 12639047680 + 219000000000, seen)[0]:
        faults.append("verdict fails du against / and the data volume")
    if not verdict("Linux", 87047512064, 86962987008)[0]:
        faults.append("verdict fails the 0.10% ext4 gap")
    if verdict("Linux", 87047512064, 80000000000)[0]:
        faults.append("verdict passes an 8% Linux gap")
    return faults


def main():
    if sys.argv[1:] == ["--self-test"]:
        faults = self_test()
        for f in faults:
            print(f"du-df-audit self-test: {f}")
        return 1 if faults else 0
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    rtrash = os.path.abspath(sys.argv[1])
    system = platform.system()
    root = as_root()
    who = "root" if root or os.geteuid() == 0 else "user"
    print(f"du-vs-df: system {system} {platform.release()}, python {platform.python_version()}, "
          f"as {who}")
    print(f"du-vs-df: {rtrash} {run([rtrash, '--version']).strip()}")
    rows = parse_df(run(["df", "-Pk"]))
    root_dev = os.stat("/").st_dev
    devs = {}
    for fs, blocks, used, mount in rows:
        dev = "-"
        if blocks:
            try:
                devs[mount] = dev = os.stat(mount).st_dev
            except OSError as e:
                dev = f"stat failed: {e.strerror}"
        print(f"du-vs-df: df row {mount!r} fs {fs!r} blocks {blocks} used {used} dev {dev}")
    print(f"du-vs-df: root dev {root_dev}, {DATA} dev {devs.get(DATA, '-')}, "
          f"os.path.ismount({DATA}) {os.path.ismount(DATA)}")
    gone = missing(system, rows)
    if gone:
        print(f"du-vs-df: MISSING: df -Pk has no row for {', '.join(gone)}; stopping")
        return 2
    counted = walked(rows, devs, root_dev)
    for fs, _, used, mount in counted:
        print(f"du-vs-df: df used {mount} {used} ({fs}, the root's device)")
    total = sum(used for _, _, used, _ in counted)
    cmd = root + [rtrash, "du", "-x", "-n", "0", "--no-git", "--json"]
    if "--open-deleted" in run([rtrash, "du", "--help"]):
        cmd.append("--open-deleted")
    cmd.append("/")
    print(f"du-vs-df: running {' '.join(cmd)}")
    j = json.loads(run(cmd))
    disk = j["roots"][0]["disk"]
    held = (j.get("open_deleted") or {}).get("disk", 0)
    print(f"du-vs-df: rtrash du -x / disk {disk}, open-deleted {held}, "
          f"unreadable {j.get('unreadable')}, inodes {j['roots'][0]['inodes']}")
    seen = disk + held
    gap = total - seen
    print(f"du-vs-df: df used {total}, du {seen}, gap {gap} "
          f"({100 * gap / max(total, 1):.2f}% of df)")
    # The system du with -x, for the record only: BSD du on macOS, GNU du
    # on Linux. It stays on st_dev too, so it shows what -x walks there.
    ref = subprocess.run(root + ["du", "-x", "-s", "-k", "/"], capture_output=True, text=True)
    errs = len(ref.stderr.splitlines())
    print(f"du-vs-df: system du -x -s -k / says {ref.stdout.split()[:1]} KiB "
          f"(exit {ref.returncode}, {errs} stderr lines; not checked)")
    ok, bound = verdict(system, total, seen)
    print(f"du-vs-df: {'ok' if ok else 'OUTSIDE'}: {bound}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
