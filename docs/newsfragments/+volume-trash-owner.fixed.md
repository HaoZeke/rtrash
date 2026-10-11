Put, list, empty and rm only use a volume's `.Trash/$uid` or `.Trash-$uid` when it is a real directory that belongs to you.
One that is a symlink or belongs to another user gets a warning: put falls back to the next choice, in the end the home trash, and the others skip it.
