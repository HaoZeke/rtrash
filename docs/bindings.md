# Python bindings

## Goal

| Instead of | Use |
|------------|-----|
| `os.remove` / `os.unlink` | `rtrash.unlink(path)` |
| `pathlib.Path.unlink()` | `rtrash.unlink(path)` |
| `shutil.rmtree(path)` | `rtrash.rmtree(path)` |

Does **not** monkey-patch `os` or `pathlib`.

## Install

```shell
pip install rtrash
```

Dev checkout: `pip install maturin && maturin develop --features python`.

## API

```python
import rtrash

rtrash.put(path, recursive=False, force=False)
rtrash.put_paths([p1, p2], recursive=False, force=False)
rtrash.unlink(path, recursive=False, force=False)
rtrash.rmtree(path, force=False)
rtrash.list_trash(trash_dir=None)
rtrash.empty_trash(days=None, trash_dir=None, dry_run=False)
rtrash.restore_path(path, force=False, trash_dir=None)
rtrash.home_trash()
rtrash.version()
```

## Testing

```shell
maturin develop --features python
python -m unittest tests.python.test_rtrash -v
```
