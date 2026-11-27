# rtrash-hud

A small desktop status display of trash size and item count. It asks the
`rtrash` library for the same numbers `rtrash status` prints
(`status::summarize_dirs`) and does not parse `.trashinfo` files itself.

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
`--trash-dir` or choose the FreeDesktop home trash.
