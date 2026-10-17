Every hand-off to the OS trash now goes through `allow_roots::trash_delete`, which checks the scope first, and `scripts/precheck.py` refuses any other call of `trash::delete`.
