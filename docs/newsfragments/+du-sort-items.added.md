`rtrash du --sort=items` ranks directories by the files and directories below them, to find inode hogs such as `node_modules`; `-C` adds an item count column to any listing, and JSON rows carry `items`.
`rtrash du --sort=mtime` lists the most recently modified entries first, and a directory gives way to the subdirectory that contains its newest change.
Short options now take an attached value, as in `rtrash du -n5`.
