With `RTRASH_ALLOW_ROOTS` set, the du browser on Windows no longer trashes a row outside the roots, and wsl restore checks every destination before it changes anything, so `-f` cannot remove a file outside the scope.
A delete in the du browser with any row outside trashes nothing.
