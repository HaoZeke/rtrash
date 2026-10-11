A copy across devices on macOS no longer stops with "File name too long" on a deep tree.
The walk measures the resolved path, so `/var` counts as `/private/var`, and switches to directory descriptors before that path would exceed the limit.
