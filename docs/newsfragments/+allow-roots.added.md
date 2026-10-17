New `RTRASH_ALLOW_ROOTS` environment variable, a path list, scopes rtrash to those folders: put trashes only paths under a root, restore and undo write only under one, and rm deletes only items trashed from one.
empty, reap and wsl rm and empty refuse while it is set, and a run that would touch a path outside exits 2 and changes nothing.
Paths compare on disk, so a symlinked folder that leads out of a root stays outside, and on Windows short 8.3 names and letter case do not matter.
