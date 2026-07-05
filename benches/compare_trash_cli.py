#!/usr/bin/env python3
"""Compare rtrash vs Python trash-cli on matched FreeDesktop fixtures.

Runs the real CLIs only (no reimplementation). Writes structured timings to
stdout for capture under the goal scratch dir.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def which(name: str) -> str:
    p = shutil.which(name)
    if not p:
        raise SystemExit(f"missing required command: {name}")
    return p


def run(cmd: list[str], env: dict[str, str]) -> tuple[int, float, str, str]:
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, env=env, capture_output=True, text=True)
    dt = time.perf_counter() - t0
    return proc.returncode, dt, proc.stdout, proc.stderr


def populate_work(work: Path, n_small: int = 400) -> None:
    for i in range(n_small):
        (work / f"f{i}.txt").write_text(f"small-{i}\n", encoding="utf-8")
    deep = work / "deep" / "a" / "b"
    deep.mkdir(parents=True)
    for i in range(80):
        (deep / f"x{i}").write_text(f"d{i}\n", encoding="utf-8")


def count_trash(trash: Path) -> int:
    files = trash / "files"
    if not files.is_dir():
        return 0
    return sum(1 for _ in files.iterdir())


def main() -> int:
    rtrash = which("rtrash") if shutil.which("rtrash") else None
    # Prefer explicit path from env (release build on terra).
    rtrash = os.environ.get("RTRASH_BIN", rtrash)
    if not rtrash or not Path(rtrash).is_file():
        raise SystemExit("set RTRASH_BIN to the rtrash binary to compare")
    trash_put = which("trash-put")
    trash_empty = which("trash-empty")
    trash_list = which("trash-list")

    print(f"rtrash={rtrash}")
    print(f"trash-put={trash_put}")
    print(f"trash-empty={trash_empty}")
    print(f"host={os.uname().nodename}")

    results: list[tuple[str, str, float, int, int]] = []

    with tempfile.TemporaryDirectory(prefix="rtrash-compare-") as tmp:
        root = Path(tmp)
        for tool, put_cmd, empty_cmd in (
            (
                "rtrash",
                [rtrash, "put", "-r"],
                [rtrash, "empty", f"--trash-dir={{trash}}"],
            ),
            (
                "trash-cli",
                [trash_put],
                [trash_empty, "--trash-dir", "{trash}"],
            ),
        ):
            for trial in (1, 2):
                work = root / f"{tool}-{trial}" / "work"
                xdg = root / f"{tool}-{trial}" / "xdg"
                work.mkdir(parents=True)
                xdg.mkdir(parents=True)
                trash = xdg / "Trash"
                env = os.environ.copy()
                env["XDG_DATA_HOME"] = str(xdg)
                env["HOME"] = str(root / f"{tool}-{trial}")
                populate_work(work)
                paths = [str(p) for p in sorted(work.iterdir())]
                put = put_cmd + paths
                ec, dt_put, out, err = run(put, env)
                print(
                    f"PUT tool={tool} trial={trial} ec={ec} wall_s={dt_put:.6f} "
                    f"entries={count_trash(trash)} stderr={err.strip()!r}"
                )
                if ec != 0:
                    print(out)
                    print(err, file=sys.stderr)
                    return 1
                n_after_put = count_trash(trash)
                if n_after_put < 2:
                    print(f"FAIL expected multi-entry trash, got {n_after_put}", file=sys.stderr)
                    return 1
                # deep tree must exist under files/
                deep_ok = any((trash / "files").rglob("x0"))
                if not deep_ok:
                    print("FAIL deep payload missing after put", file=sys.stderr)
                    return 1
                empty = [
                    c.format(trash=str(trash)) if "{trash}" in c else c for c in empty_cmd
                ]
                ec, dt_empty, out, err = run(empty, env)
                print(
                    f"EMPTY tool={tool} trial={trial} ec={ec} wall_s={dt_empty:.6f} "
                    f"stderr={err.strip()!r}"
                )
                if ec != 0:
                    print(out)
                    print(err, file=sys.stderr)
                    return 1
                left = count_trash(trash)
                info_left = (
                    sum(1 for _ in (trash / "info").iterdir())
                    if (trash / "info").is_dir()
                    else 0
                )
                print(f"POST tool={tool} trial={trial} files_left={left} info_left={info_left}")
                if left != 0 or info_left != 0:
                    print("FAIL trash not empty after empty", file=sys.stderr)
                    return 1
                results.append((tool, "put", dt_put, trial, n_after_put))
                results.append((tool, "empty", dt_empty, trial, n_after_put))

    # Summaries
    def avg(tool: str, op: str) -> float:
        xs = [t for t_, o, t, _, _ in results if t_ == tool and o == op]
        return sum(xs) / len(xs)

    print("--- SUMMARY ---")
    for tool in ("rtrash", "trash-cli"):
        print(
            f"{tool} put_avg_s={avg(tool, 'put'):.6f} empty_avg_s={avg(tool, 'empty'):.6f}"
        )
    rp, tp = avg("rtrash", "put"), avg("trash-cli", "put")
    re, te = avg("rtrash", "empty"), avg("trash-cli", "empty")
    print(f"put_speedup_rtrash_over_trashcli={tp / max(rp, 1e-9):.2f}x")
    print(f"empty_speedup_rtrash_over_trashcli={te / max(re, 1e-9):.2f}x")
    # Smoke list on a fresh tiny put for both (correctness, not timed heavily)
    with tempfile.TemporaryDirectory(prefix="rtrash-list-") as tmp:
        root = Path(tmp)
        for tool, put_bin, list_bin in (
            ("rtrash", [rtrash, "put"], [rtrash, "list"]),
            ("trash-cli", [trash_put], [trash_list]),
        ):
            work = root / tool / "work"
            xdg = root / tool / "xdg"
            work.mkdir(parents=True)
            xdg.mkdir(parents=True)
            f = work / "one.txt"
            f.write_text("1\n", encoding="utf-8")
            env = os.environ.copy()
            env["XDG_DATA_HOME"] = str(xdg)
            env["HOME"] = str(root / tool)
            ec, _, out, err = run(put_bin + [str(f)], env)
            assert ec == 0, err
            ec, _, out, err = run(list_bin, env)
            assert ec == 0, err
            assert "one.txt" in out, out
            print(f"LIST_OK tool={tool}")

    print("COMPARE_OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
