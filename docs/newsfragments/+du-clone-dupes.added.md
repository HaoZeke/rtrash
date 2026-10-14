`rtrash du --dupes --clone-dupes` replaces every extra copy with a reflink clone of the kept file on btrfs, XFS or APFS, so each path keeps its own mode and times while the blocks are shared.
