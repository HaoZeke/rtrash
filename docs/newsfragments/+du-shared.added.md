New `rtrash du --shared` counts each extent that reflinks, clones or snapshots share once per row and in the totals, on copy-on-write file systems for Linux such as Btrfs, so copies no longer add up to more than they take on disk.
It maps extents like `--unique`, so the plain listing stays as fast as before.
