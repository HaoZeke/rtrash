# fish completion for rtrash and multi-call names
# Preferred install: rtrash setup
# Or: rtrash completions fish > ~/.config/fish/completions/rtrash.fish

function __rtrash_seen_subcommand
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for c in $cmd
        switch $c
            case put empty list status trash-stats du restore undo rm who reap finder config setup keys completions man
                echo $c
                return 0
        end
    end
    return 1
end

# Top-level rtrash: subcommands when no subcommand yet
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'put' -d 'move files to the trash (rm-compatible flags)'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'empty' -d 'purge trashed items'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'list' -d 'list trashed items'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'status' -d 'item count and reclaimable size summary'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'trash-stats' -d 'per-trash size, inodes, oldest and largest items'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'du' -d 'find the largest directories and files to trash'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'restore' -d 'restore a trashed item'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'undo' -d 'restore everything one put run trashed'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'rm' -d 'permanently delete matching trash entries'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'who' -d 'list the live processes holding PATH'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'reap' -d 'trash stale temp entries no live process holds'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'finder' -d 'macOS Finder Trash with Put Back'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'config' -d 'show the config file and the settings it makes'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'setup' -d 'install multi-call links, completions, man page'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'keys' -d 'show or sample the TUI keybind config'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'completions' -d 'print embedded shell completion script'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'man' -d 'print embedded man page to stdout'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -s h -l help -d 'display help and exit'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -s V -l version -d 'output version and exit'

# put / bare put fallthrough flags
set -l put_cond '__rtrash_seen_subcommand | string match -q put; or not __rtrash_seen_subcommand'
complete -c rtrash -n $put_cond -s f -l force -d 'ignore nonexistent files, never prompt'
complete -c rtrash -n $put_cond -s i -d 'prompt before every removal'
complete -c rtrash -n $put_cond -s I -d 'prompt once for more than three files or recursive'
complete -c rtrash -n $put_cond -l interactive -d 'prompt according to WHEN'
complete -c rtrash -n $put_cond -s r -s R -l recursive -d 'remove directories and contents'
complete -c rtrash -n $put_cond -s d -l dir -d 'remove empty directories'
complete -c rtrash -n $put_cond -s v -l verbose -d 'explain what is being done'
complete -c rtrash -n $put_cond -l one-file-system -d 'accepted for rm compatibility (no-op)'
complete -c rtrash -n $put_cond -l preserve-root -f -a all -d 'refuse /, home and mount points (default)'
complete -c rtrash -n $put_cond -l no-preserve-root -d 'do not treat those specially'
complete -c rtrash -n $put_cond -l trash-dir -d 'put into this trash root' -r
complete -c rtrash -n $put_cond -l home-only -d 'always use the home trash'
complete -c rtrash -n $put_cond -l plain -d 'skip TUI; require file operands'
complete -c rtrash -n $put_cond -l no-space-check -d 'copy across devices even when space or inodes run low'
complete -c rtrash -n $put_cond -l in-use -x -a 'warn refuse ignore' -d 'when a live process holds an item'
complete -c rtrash -n $put_cond -l json -d 'JSON summary of trashed items'

# empty
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s n -l dry-run -d 'report what would be removed; show reclaimable size'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s v -l verbose -d 'print each removed item'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s f -l force -d 'accepted for trash-cli compatibility'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l trash-dir -d 'empty only this trash directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l older-than -d 'only items older than DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l max-size -d 'remove items until the trash fits in SIZE' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l prune -d 'which items --max-size removes first' -x -a 'oldest largest'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l json -d 'JSON summary'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l plain -d 'skip TUI'

# list / status
for sub in list status
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l home-only -d 'only the home trash'
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l trash-dir -d 'only this trash directory' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l older-than -d 'only items older than DAYS days' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l newer-than -d 'only items within last DAYS days' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l larger-than -d 'only items over SIZE on disk' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l smaller-than -d 'only items under SIZE on disk' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l json -d 'JSON output'
end
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q list' -l size -d 'add each item size'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q list' -l sort -x -a 'date size name path' -d 'row order'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q list' -l reverse -d 'reverse the order'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q list' -l all-users -d 'list every readable user trash with an owner column'

# du
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s n -l top -d 'list N directories and N files' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s x -l one-file-system -d 'skip directories on other file systems'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l apparent -d 'rank by apparent size instead of disk use'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l sort -d 'rank rows by this key' -x -a 'disk apparent items inodes mtime'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s C -l count -d 'add an ITEMS column'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l inodes -d 'rank by inodes and total them'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l unique -d 'add a UNIQUE column of bytes no other file shares'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l shared -d 'count each shared extent once in sizes'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l types -d 'list disk use per file type instead of directories and files'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l max-depth -d 'list only entries at most N levels deep' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l threads -d 'walk with N threads' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l all-dirs -d 'list every directory level'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l artefacts -d 'list only regenerable build artefacts and caches'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l exclude -d 'leave out entries that match a gitignore-style pattern' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s X -l exclude-from -d 'read exclude patterns from a file' -r -F
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l exclude-caches -d 'leave out directories with a valid CACHEDIR.TAG'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l gitignore -d 'leave out what git ignores'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l dirs -d 'list directories only'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l files -d 'list files only'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l older-than -d 'only entries untouched for more than DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l newer-than -d 'only entries modified within the last DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l since -d 'only entries modified at or after DATE' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l until -d 'only entries last modified before DATE' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l home-only -d 'only the home trash in the trash summary'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l trash-dir -d 'only this trash directory in the summary' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l no-git -d 'do not ask git about listed paths'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l include-dirty -d 'with --print0, also print rows with git work at risk'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l json -d 'emit a JSON object'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l print0 -d 'print listed paths NUL-separated'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l pick -d 'mark listed rows in the put browser and trash them'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s i -l interactive -d 'explore one directory at a time; trash, undo, rescan'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l no-delete -d 'with -i, browse only: no trash or undo'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l si -d 'sizes in powers of 1000 (kB, MB, GB)'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s o -l output -r -d 'write a snapshot of every directory total'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l devices -d 'list every mounted file system with its size and free space'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l ncdu -r -d 'write an ncdu export of PATH'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l ncdu-binary -r -d 'write an ncdu binary export of PATH (ncdu -O)'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -s f -l file -r -d 'list from a snapshot or an ncdu export instead of scanning'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q du' -l stacks -d 'print folded stacks for flame graph tools'

# finder
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -f -a 'put list restore'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -l trash-dir -r -d 'use this Finder trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -l all -d 'restore every item with a Put Back record'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -s f -l force -d 'put: ignore missing; restore: overwrite'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -l conflict -x -a 'refuse overwrite rename skip' -d 'when the destination exists'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -s v -l verbose -d 'put: name each trashed file'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -l json -d 'print JSON'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q finder' -l help -d 'display help'

# config
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q config' -f
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q config' -l path -d 'print where the config file is looked for'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q config' -l default -d 'print a config file with every setting commented out'

# undo
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -f
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -s l -l list -d 'list put runs, newest first'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -s n -l dry-run -d 'show what would be restored'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -s f -l force -d 'overwrite existing files at original locations'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -l conflict -x -a 'refuse overwrite rename skip' -d 'when an original location is taken'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -l json -d 'print the result as JSON'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q undo' -l trash-dir -d 'only this trash directory' -r

# restore
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -s f -l force -d 'overwrite existing file at original location'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l trash-dir -d 'only this trash directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l cwd-only -d 'only items trashed from under the current directory'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l plain -d 'numbered selection even on a TTY'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l no-space-check -d 'copy across devices even when space or inodes run low'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l newest-first -d 'list the most recently trashed item first'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l oldest-first -d 'list the oldest trashed item first'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l name -r -d 'match by file name or trash name'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l latest -d 'of several matches, restore the newest'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l all -d 'of several matches, restore every one'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l glob -x -d 'match by shell pattern'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l regex -x -d 'match original paths by regex'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l to -x -a '(__fish_complete_directories)' -d 'restore into this directory'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l conflict -x -a 'refuse overwrite rename skip' -d 'when the destination exists'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l json -d 'print the result as JSON'

# rm (trash-rm)
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l trash-dir -d 'only this trash directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l older-than -d 'only match items older than DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l newer-than -d 'only match items within last DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l larger-than -d 'only match items over SIZE on disk' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l smaller-than -d 'only match items under SIZE on disk' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -s n -l dry-run -d 'list matches and reclaimable size; do not delete'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -s f -l force -d 'allow mass patterns that match everything'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -s v -l verbose -d 'print each permanently removed original path'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l json -d 'JSON summary with matches'

# setup
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -l prefix -d 'install root' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -l bin-dir -d 'binary/link directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -l with-rm -d 'also link rm to rtrash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -s n -l dry-run -d 'print actions without writing'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -s f -l force -d 'replace existing links/files'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q setup' -s v -l verbose -d 'print each path written'

# keys
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q keys' -f -s l -l list -d 'print the resolved keymap'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q keys' -f -l sample -d 'print a sample keys.toml'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q keys' -f -l path -d 'print the config path'

# completions shell names
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q completions' -f -a 'bash zsh fish' -d 'shell'

# Multi-call names: complete flags for each tool (no system rm by default)
for cmd in trash-put trash
    complete -c $cmd -s f -l force
    complete -c $cmd -s r -s R -l recursive
    complete -c $cmd -s d -l dir
    complete -c $cmd -s v -l verbose
    complete -c $cmd -s i
    complete -c $cmd -s I
    complete -c $cmd -l interactive
    complete -c $cmd -l one-file-system
    complete -c $cmd -l preserve-root
    complete -c $cmd -l no-preserve-root
    complete -c $cmd -l trash-dir -r
    complete -c $cmd -l home-only
    complete -c $cmd -l plain
    complete -c $cmd -l no-space-check
    complete -c $cmd -l in-use -x -a 'warn refuse ignore'
    complete -c $cmd -l json
    complete -c $cmd -l help
    complete -c $cmd -l version
end

complete -c trash-empty -s n -l dry-run
complete -c trash-empty -s v -l verbose
complete -c trash-empty -s f -l force
complete -c trash-empty -l trash-dir -r
complete -c trash-empty -l home-only
complete -c trash-empty -l older-than -r
complete -c trash-empty -l max-size -r
complete -c trash-empty -l prune -x -a 'oldest largest'
complete -c trash-empty -l json
complete -c trash-empty -l plain
complete -c trash-empty -l help
complete -c trash-empty -l version

complete -c trash-list -l home-only
complete -c trash-list -l trash-dir -r
complete -c trash-list -l older-than -r
complete -c trash-list -l newer-than -r
complete -c trash-list -l larger-than -r
complete -c trash-list -l smaller-than -r
complete -c trash-list -l json
complete -c trash-list -l size
complete -c trash-list -l sort -x -a 'date size name path'
complete -c trash-list -l reverse
complete -c trash-list -l all-users
complete -c trash-list -l help
complete -c trash-list -l version

complete -c trash-restore -s f -l force
complete -c trash-restore -l home-only
complete -c trash-restore -l trash-dir -r
complete -c trash-restore -l cwd-only
complete -c trash-restore -l plain
complete -c trash-restore -l no-space-check
complete -c trash-restore -l newest-first
complete -c trash-restore -l oldest-first
complete -c trash-restore -l name -r
complete -c trash-restore -l latest
complete -c trash-restore -l all
complete -c trash-restore -l glob -x
complete -c trash-restore -l regex -x
complete -c trash-restore -l to -x -a '(__fish_complete_directories)'
complete -c trash-restore -l conflict -x -a 'refuse overwrite rename skip'
complete -c trash-restore -l json
complete -c trash-restore -l help
complete -c trash-restore -l version

complete -c trash-rm -l trash-dir -r
complete -c trash-rm -l home-only
complete -c trash-rm -l older-than -r
complete -c trash-rm -l newer-than -r
complete -c trash-rm -l larger-than -r
complete -c trash-rm -l smaller-than -r
complete -c trash-rm -l json
complete -c trash-rm -s n -l dry-run
complete -c trash-rm -s f -l force
complete -c trash-rm -s v -l verbose
complete -c trash-rm -l help
complete -c trash-rm -l version
# Multi-call name `rm` is intentionally not completed here (would shadow system rm).

# who
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q who' -l json -d 'print a JSON object'

# reap
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -s p -l profile -x -a 'ai claude cursor codex aider ollama compilers cargo-target cargo-registry sccache ccache go zig bazel gradle pkg pip uv npm pnpm yarn bun conda thumbnails' -d 'also reap a profile'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l list-profiles -d 'list the profiles'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l shared -d 'show what a delete frees, counting shared extents once'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l install-timer -d 'write a user timer that runs this reap'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l remove-timer -d 'stop and remove the reap timer'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l timer-status -d 'show whether the reap timer is installed'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l when-below -x -d 'reap only when free space is under this'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l until -x -d 'free space to reap back to'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l purge-trash -d 'empty the oldest trash items first when space is short'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l roots -r -d 'directories {roots} globs search'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l depth -x -d 'how deep ** reaches'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l tool -d 'hand a profile to its tool cleanup'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l trash -d 'move to the trash, even for a permanent profile'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l older-than -x -d 'only entries older than AGE (2h, 10d)'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l age-by -x -d 'times that count: a, c, m'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -s n -l dry-run -d 'show what would go, change nothing'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l permanent -d 'delete instead of trashing'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l trash-dir -r -d 'put into this trash root'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l home-only -d 'always use the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -s v -l verbose -d 'also show skipped entries'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q reap' -l json -d 'print a JSON object'

# trash-stats
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l top -x -d 'list the N largest items'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l trash-dir -r -d 'only this trash directory'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l history -d 'also show each trash history'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l since -r -d 'with --history, only lines from the last AGE'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l json -d 'print a JSON object'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l help -d 'display help'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q trash-stats' -l version -d 'display version'
