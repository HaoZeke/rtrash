Ctrl-C during `rtrash du` now stops the scan and prints what it counted, with a line saying the sizes are lower bounds, and exits 130.
JSON output gains `"partial": true` in that case, while `--print0`, `--pick`, `-o` and `--stacks` print nothing, since a partial list would pass for a whole one.
A second Ctrl-C ends the process at once.
In `rtrash du -i`, `q` or Ctrl-C while a directory scans stops that scan and keeps its partial sizes; pressed again, it quits.
