New `rtrash status --check` checks each FreeDesktop trash record and lists records whose payload is gone, records that cannot serve a restore, bad deletion dates, payloads with no record and stray names in `info/`, with exit 1 when it finds any.
`--fix` removes the dangling and bad records but keeps each payload, as an orphan. `--fix -n` shows the plan.
`RTRASH_NO_PERMANENT`, `RTRASH_MAX_PERMANENT` and `RTRASH_ALLOW_ROOTS` apply to `--fix`.
