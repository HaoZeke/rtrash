The `directorysizes` cache now records allocated blocks the way `du -B1` counts them, with directory blocks included and each hard-linked file counted once.
Put sizes a trashed directory in parallel after releasing the trash lock, so a 40,000-file tree goes to the trash in 91 milliseconds instead of 148.
Commands `status` and `empty --dry-run` write sizes they had to walk back to the cache and drop lines for directories no longer in the trash, so a repeat run drops from 53 to 1.4 milliseconds.
A `$top/.Trash` that fails the sticky-directory check now prints one warning before put falls back to `$top/.Trash-$uid`.
