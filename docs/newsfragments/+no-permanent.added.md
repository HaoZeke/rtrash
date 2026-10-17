New `RTRASH_NO_PERMANENT` environment variable makes rtrash delete nothing for good: rm and empty (and their trash-cli names) exit 2 before they start, while dry runs, `--help` and `--version` still work.
Set it for agents and shared accounts, so that trash stays the only way to remove files.
