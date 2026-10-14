# Parity with other trash tools

rtrash shares the trash with trash-cli, gtrash, GNOME's `gio trash` and the desktop file managers built on it.
The full tables live in `docs/orgmode/parity.org` (the site page "Parity with other trash tools").

- All four tools read and restore each other's entries, including names with spaces, `%` and Unicode.
- A second item with the same name becomes `name_1` under rtrash and trash-cli, and `name.2` under gio and gtrash; each reads the other spelling.
- Under the trash-* names, rtrash takes trash-cli command lines: `--trash-dir DIR`, `--print-completion`, `trash-list --volumes` and `--trash-dirs`, `trash-empty -i` and `--all-users`, exit 74 from `trash-put`, and trash-cli's `trash-restore` scope, `--sort` and `--overwrite`.
- On macOS, plain put, list, restore and empty use the Finder Trash with Put Back; `RTRASH_MACOS_TRASH=freedesktop`, `macos.trash` in the config or a `--trash-dir` pin keeps the FreeDesktop home trash.
