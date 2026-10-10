New `rtrash du --unique` adds a UNIQUE column with the bytes of each row that no other file shares, roughly what deleting it would free on copy-on-write file systems such as Btrfs and the Apple File System.
On Linux it maps each file's extents with `FS_IOC_FIEMAP` and leaves out the ones flagged shared; on macOS it reads the private size kept for each clone.
The totals gain a unique figure, and a closing line gives the shared extents once each and as the files reference them.
JSON output carries `unique` on rows and roots and a `shared` object, only on these scans.
The plain listing stays as fast as before, since only `--unique` maps extents.
