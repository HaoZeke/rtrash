`rtrash undo` takes `--conflict=refuse|overwrite|rename|skip`, as restore does, for items whose original location is taken.
With `-n`, each line says what the policy would do.
The undo JSON adds `skipped`, and each restored item carries `restored_to`.
