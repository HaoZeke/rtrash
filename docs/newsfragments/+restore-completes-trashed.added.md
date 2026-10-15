Shell completion for `restore` now offers the original paths in the trash, and the trashed names after `--name=`, in all six scripts.
The zsh script adds them through `compadd`, so a `matcher-list` style applies (trash-cli #363).
