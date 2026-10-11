`rtrash du --ncdu=FILE PATH` writes an ncdu export that `ncdu -f FILE` browses and `rtrash du -f FILE` lists.
It streams as it walks, honours `-x` and the filters, and marks what it could not read.
