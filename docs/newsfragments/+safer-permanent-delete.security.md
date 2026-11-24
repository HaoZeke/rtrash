Permanent deletion now matches exact original paths by default, refuses
ambiguous duplicates, honors a per-user keep file in rm, empty, reap and
retention, rechecks items immediately before removal, and gives dry runs the
same victim list and empty-result status as a real run. Broad patterns cannot
cover every name below a matched directory or empty a known scope without
`--force`, even when only its last entry remains.
The WSL Recycle Bin rm and empty paths apply the same selection, keep and
duplicate guards.
When the scope is unknown, regex and substring matching and globs containing
a path separator require --force; use an exact path, --id, or a basename glob
instead.
