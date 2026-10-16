rtrash exits quietly when a reader closes stdout early, as `| head` or `| grep -q` do, instead of panicking.
