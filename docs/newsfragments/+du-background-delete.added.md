The `du -i` explorer now trashes in the background, as dua 2.45 does. The status line counts the rows done and the bytes left, and each row leaves the list as it goes, so the explorer stays usable during a large delete.
A delete in progress keeps queued directories closed. `d`, `u` and `r` wait for it. On `q`, the explorer lets the row in hand finish and then quits; rows not yet started stay in place.
The explorer used to crash when you trashed the directory on screen through a mark from a view above. It now shows the parent instead.
