Permanent deletion now matches exact original paths by default, refuses
ambiguous duplicates, honors a per-user keep file in rm, empty, reap and
retention, rechecks items immediately before removal, and gives dry runs the
same victim list and empty-result status as a real run. Broad patterns cannot
empty the whole trash or one scope root without `--force`, even when only its
last entry remains.
