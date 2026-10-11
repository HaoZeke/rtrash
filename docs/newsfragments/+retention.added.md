New `rtrash empty --max-size=10G` removes items until the trash fits in 10 GiB, oldest first or with `--prune=largest` largest first.
An age argument still applies: the age cut runs first and the size cut takes the rest.
Setting `RTRASH_MAX_SIZE` prunes each trash directory a put used back under that budget, and the items of that put always stay.
