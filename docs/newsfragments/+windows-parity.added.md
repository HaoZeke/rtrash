On Windows, `rtrash restore` opens the restore browser on a terminal, and `rtrash du -i` explores a tree and deletes into the Recycle Bin, with `u` to undo.
`rtrash status` lists each drive's Recycle Bin with its cap from `MaxCapacity` in the registry and the share in use, then the total.
The trash line of `rtrash du` adds the cap of the scanned drives, and `--json` carries it as `cap` when known.
The keymap file falls back to `%APPDATA%\rtrash\keys.toml` on Windows, next to the config file.
