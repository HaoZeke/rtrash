New `rtrash undo` restores every item one put run trashed, in one step.
Each put run now writes a batch id into its `.trashinfo` files under the extra key `X-Rtrash-Batch`, which other tools ignore.
Without an argument, undo takes the most recent run; `undo --list` shows past runs and `undo BATCH` picks one.
Items from gio, Dolphin or trash-cli carry no id and never match.
JSON from `list --json` now includes the `batch` of each item.
