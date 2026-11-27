`rtrash put --files-from=FILE` trashes the paths in FILE, one per line,
or NUL-separated with `-0`/`--zero` or `--files0-from`. `-0` clusters
with other short options. `-` reads standard input. put and du share
the list reader. Errors name `--files-from` or `--files0-from`,
whichever was given.
