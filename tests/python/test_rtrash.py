"""Drive the shipped rtrash extension under an isolated XDG_DATA_HOME."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path


class TrashSandbox(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="rtrash-py-")
        self.root = Path(self._tmp.name)
        self.work = self.root / "work"
        self.work.mkdir()
        self.xdg = self.root / "xdg"
        self.xdg.mkdir()
        self._old_xdg = os.environ.get("XDG_DATA_HOME")
        self._old_home = os.environ.get("HOME")
        os.environ["XDG_DATA_HOME"] = str(self.xdg)
        os.environ["HOME"] = str(self.root)
        # Import after env is set so home_trash() sees the sandbox.
        import rtrash as rt

        self.rt = rt

    def tearDown(self) -> None:
        if self._old_xdg is None:
            os.environ.pop("XDG_DATA_HOME", None)
        else:
            os.environ["XDG_DATA_HOME"] = self._old_xdg
        if self._old_home is None:
            os.environ.pop("HOME", None)
        else:
            os.environ["HOME"] = self._old_home
        self._tmp.cleanup()

    def trash_files(self) -> Path:
        return self.xdg / "Trash" / "files"

    def test_put_moves_and_is_recoverable(self) -> None:
        target = self.work / "hello.txt"
        target.write_text("payload", encoding="utf-8")
        self.rt.put(str(target))
        self.assertFalse(target.exists())
        self.assertTrue((self.trash_files() / "hello.txt").is_file())
        info = self.xdg / "Trash" / "info" / "hello.txt.trashinfo"
        self.assertTrue(info.is_file())
        body = info.read_text(encoding="utf-8")
        self.assertIn("[Trash Info]", body)
        self.assertIn("Path=", body)
        listed = self.rt.list_trash()
        paths = [p for _, p in listed]
        self.assertTrue(any(str(target) in p or p.endswith("hello.txt") for p in paths), listed)

    def test_unlink_alias_and_restore(self) -> None:
        target = self.work / "bye.txt"
        target.write_text("x", encoding="utf-8")
        self.rt.unlink(str(target))
        self.assertFalse(target.exists())
        self.rt.restore_path(str(target))
        self.assertTrue(target.is_file())
        self.assertEqual(target.read_text(encoding="utf-8"), "x")

    def test_rmtree_trashes_directory(self) -> None:
        d = self.work / "tree"
        d.mkdir()
        (d / "a").write_text("a", encoding="utf-8")
        self.rt.rmtree(str(d))
        self.assertFalse(d.exists())
        self.assertTrue((self.trash_files() / "tree" / "a").is_file())

    def test_empty_clears_pinned_semantics_via_home(self) -> None:
        f = self.work / "z.txt"
        f.write_text("z", encoding="utf-8")
        self.rt.put(str(f))
        trash = self.xdg / "Trash"
        self.rt.empty_trash(trash_dir=str(trash))
        self.assertFalse(any(self.trash_files().iterdir()) if self.trash_files().is_dir() else True)
        infos = list((trash / "info").iterdir()) if (trash / "info").is_dir() else []
        self.assertEqual(infos, [])


if __name__ == "__main__":
    unittest.main()
