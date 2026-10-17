In `rtrash du -i`, a symlink is now a row of its own size, marked `@` in the flag column as ncdu marks it.
It never opens, `d` trashes the link and leaves its target alone, and `i` names the target.
The table and `--json` count links as before without listing them.
