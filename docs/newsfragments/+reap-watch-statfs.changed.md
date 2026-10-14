`rtrash reap --watch` checks the last pass's file systems with one statfs each before a full pass and skips it while they are above the target, with a full pass at least every 10 ticks.
