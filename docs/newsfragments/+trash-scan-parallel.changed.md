Commands `list`, `status` and `restore` read a big trash faster: they read the trashinfo files in parallel and size the entries in parallel.
Reading dates now costs one time-zone lookup per hour instead of one per item, and each lookup used to stat the zone file.
On a trash of 2,050 items, `list` went from 19.7 to 10.1 milliseconds and `status` from 38.9 to 17.2.
