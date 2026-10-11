New `rtrash du -i PATH` explores a tree one directory at a time, largest first, with a size bar on each row.
Each row carries its artefact kind and git state, and a `!` marks git work at risk.
Pressing `d` moves rows to the trash after a confirmation, `u` restores the last deletion, and `r` rescans the current directory.
Parent totals follow every change without a rescan, and the explorer reuses the keymap of the other browsers.
