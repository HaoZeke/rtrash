On Windows, the original path from a version 2 `$I` record is read in full: its length field counts characters, not bytes, so names such as `rtrash-purge-1.txt` no longer come out cut short.
