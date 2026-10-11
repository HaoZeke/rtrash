`rtrash du` no longer reads a large directory twice before the walk: the check for an uncached tree now lists only the 48 entries it times.
A directory of 200000 files took about 110 milliseconds again instead of 160, with 10 MiB less memory.
