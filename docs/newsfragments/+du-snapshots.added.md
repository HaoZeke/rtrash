New `rtrash du -o FILE` writes a snapshot of every directory total, and `rtrash du -f FILE` lists the largest directories from one without scanning.
Snapshots keep one record per line with percent-encoded paths, so grep and text diff tools read them too.
The new `rtrash du diff` lists the directories that grew or shrank most between two snapshots.
Given only the older snapshot, it scans the same roots again and compares against that.
Keeping thousands of rows no longer costs quadratic time: listing all 11,881 directories under /usr dropped from 6 seconds to 0.19.
