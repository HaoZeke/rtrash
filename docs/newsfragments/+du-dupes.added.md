`rtrash du --dupes` finds files with the same contents, grouped by size, edge hash and full hash.
Hard links count once and shared reflink extents free nothing.
`--trash-dupes` trashes every copy but the oldest, `--min-size` skips small files, and `--json` and `--print0` work for scripts.
