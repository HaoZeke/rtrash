# rtrash-hud

A small desktop status display of the trash item count and reclaimable
size. It asks the `rtrash` library for the same numbers `rtrash status`
prints (`status::summarize_dirs_readonly`) and does not parse
`.trashinfo` files itself. The read does not take the trash lock and
does not rewrite `directorysizes`. There is no age filter; `rtrash
status --older-than` is the command that has one.

It is an optional workspace member. `cargo build` and `cargo test` at the
repository root build the CLI only, and the musl release tarball does not
include this program.

```console
cargo build -p rtrash-hud
cargo test -p rtrash-hud
./target/debug/rtrash-hud --trash-dir=/path/to/Trash
```

`rtrash-hud.desktop` launches it. Check that file with
`desktop-file-validate` and copy it to `~/.local/share/applications/`
when the `rtrash-hud` binary is on `PATH`.

The display reads a FreeDesktop trash. It does not read the Finder Trash
or the Recycle Bin. On macOS, where Finder is the default, pass
`--trash-dir` or set `RTRASH_MACOS_TRASH=freedesktop` (or `macos.trash =
"freedesktop"` in the config).

`--json` prints one object. The keys are `items`, `bytes`, and `roots`.
Each root has `path`, `items`, `bytes`, `orphans`, and `dangling`. These
are not the `rtrash status --json` names (`total_items`, `total_bytes`,
and the `fs_*` fields). With no trash found, the text display writes
those two lines to stderr, as `rtrash status` does. With `--json`, both
write only their JSON object to stdout and leave stderr empty. This
program prints `{"items":0,"bytes":0,"roots":[]}`. `rtrash status --json`
prints its own object, with an empty `roots` array and `total_items` and
`total_bytes` of 0.
