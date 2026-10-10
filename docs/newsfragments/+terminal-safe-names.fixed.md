Text output on a terminal now shows control characters and bidirectional overrides in names as `?`, as `ls` does.
A trashed name, or a `Path=` line another user wrote into a shared trash, could carry escape sequences that move the cursor, recolor the screen, or fake extra rows.
Pipes, files, and `--json` still get the raw name.
