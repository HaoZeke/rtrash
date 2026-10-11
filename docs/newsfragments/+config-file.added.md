New config file at `$XDG_CONFIG_HOME/rtrash/config.toml` sets defaults for `du` and `list`, the trash budget after every put (`[policy]` with `max_size` and `prune`), and extra paths that put refuses (`[protect]` with `paths`).
Environment variables and command-line options still win, `RTRASH_CONFIG` points at another file, and an empty `RTRASH_CONFIG` turns the file off.
The new `rtrash config` prints the settings in effect, `--path` prints where the file lives, and `--default` prints a template.
