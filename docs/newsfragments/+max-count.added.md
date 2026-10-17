New `--max-count=N` for rm and empty, and the `RTRASH_MAX_PERMANENT` environment variable, cap how many items one run may delete for good.
A run past the cap exits 2 and deletes nothing, and `-n` lists what it would have deleted.
