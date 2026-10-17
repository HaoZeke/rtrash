`put` sends a file on a mount that volume discovery skips, such as a document portal (`fuse.portal`) or the `gvfs` mount under `/run/user/$uid`, to the home trash. Before, it made a `.Trash-$uid` on the mount around it, which is another device, and copied the file into it.
Discovery now goes by the later of two mounts on one mount point, the one in view.
