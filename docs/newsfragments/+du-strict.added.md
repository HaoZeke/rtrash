New `du --strict` exits 1 when any entry could not be read, as coreutils du does, after the usual output; it covers the table, `--json` and the ncdu exports.
In the NOTE column and in `--tree`, a folder du could not read shows `unreadable`, and a folder with an unreadable entry below it shows `incomplete`, since its sizes are low.
