`rtrash du` counts directories past 64 levels whose paths exceed `PATH_MAX`, which it used to skip without saying so, and no longer crashes with a stack overflow on trees about 2000 levels deep.
The walk stops at 4096 levels or 32 KiB of path, counts what lies past that with its own blocks only, and reports how many such directories there were.
