"""ASV benchmarks for the rtrash FreeDesktop CLI.

Times the **shipped binary** only (``RTRASH_BIN`` or ``rtrash`` on ``PATH``).
Does not reimplement trash logic in Python.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from pathlib import Path


def _bin() -> str:
    env = os.environ.get("RTRASH_BIN")
    if env and Path(env).is_file():
        return env
    found = shutil.which("rtrash")
    if not found:
        raise RuntimeError("set RTRASH_BIN or install rtrash on PATH for ASV")
    return found


def _populate(work: Path, n_files: int) -> None:
    for i in range(n_files):
        (work / f"f{i}.txt").write_text(f"payload-{i}\n", encoding="utf-8")
    deep = work / "tree" / "a" / "b"
    deep.mkdir(parents=True)
    for i in range(max(1, n_files // 5)):
        (deep / f"d{i}").write_text(f"d{i}\n", encoding="utf-8")


class PutEmptySuite:
    """put a tree into trash then empty (isolated XDG_DATA_HOME)."""

    params = [50, 200]
    param_names = ["n_files"]
    timeout = 120.0

    def setup(self, n_files: int) -> None:
        self.rtrash = _bin()
        self._tmp = tempfile.TemporaryDirectory(prefix="rtrash-asv-")
        root = Path(self._tmp.name)
        self.work = root / "work"
        self.work.mkdir()
        self.xdg = root / "xdg"
        self.xdg.mkdir()
        self.trash = self.xdg / "Trash"
        _populate(self.work, n_files)
        self.env = os.environ.copy()
        self.env["XDG_DATA_HOME"] = str(self.xdg)

    def teardown(self, n_files: int) -> None:
        self._tmp.cleanup()

    def time_put_tree(self, n_files: int) -> None:
        # Recursive put of the whole work tree in one invocation.
        subprocess.run(
            [self.rtrash, "put", "-r", str(self.work)],
            env=self.env,
            check=True,
            capture_output=True,
        )
        # recreate for next sample if asv reuses setup
        if not self.work.exists():
            self.work.mkdir()
            _populate(self.work, n_files)

    def time_put_then_empty(self, n_files: int) -> None:
        if not self.work.exists():
            self.work.mkdir()
            _populate(self.work, n_files)
        subprocess.run(
            [self.rtrash, "put", "-r", str(self.work)],
            env=self.env,
            check=True,
            capture_output=True,
        )
        subprocess.run(
            [self.rtrash, "empty", f"--trash-dir={self.trash}"],
            env=self.env,
            check=True,
            capture_output=True,
        )
        self.work.mkdir(exist_ok=True)
        _populate(self.work, n_files)


class StatusSuite:
    """status after a small put (exercises list/directorysizes path)."""

    timeout = 60.0

    def setup(self) -> None:
        self.rtrash = _bin()
        self._tmp = tempfile.TemporaryDirectory(prefix="rtrash-asv-status-")
        root = Path(self._tmp.name)
        self.work = root / "work"
        self.work.mkdir()
        self.xdg = root / "xdg"
        self.xdg.mkdir()
        _populate(self.work, 40)
        self.env = os.environ.copy()
        self.env["XDG_DATA_HOME"] = str(self.xdg)
        subprocess.run(
            [self.rtrash, "put", "-r", str(self.work)],
            env=self.env,
            check=True,
            capture_output=True,
        )

    def teardown(self) -> None:
        self._tmp.cleanup()

    def time_status(self) -> None:
        subprocess.run(
            [self.rtrash, "status"],
            env=self.env,
            check=True,
            capture_output=True,
        )
