rtrash now skips a volume's `.Trash/$uid` or `.Trash-$uid` when your group or other users can write to it. Anyone who can write there could swap its `files/` or `info/` for a symlink (trash-cli #433).
Put warns and falls back to the next trash, in the end the home trash; list, empty and rm warn and skip it. Run `chmod go-w` on the folder to use it again.
