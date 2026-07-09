# fish completion for rtrash and multi-call names
# Preferred install: rtrash setup
# Or: rtrash completions fish > ~/.config/fish/completions/rtrash.fish

function __rtrash_seen_subcommand
    set -l cmd (commandline -opc)
    set -e cmd[1]
    for c in $cmd
        switch $c
            case put empty list status restore rm setup completions man
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
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'restore' -d 'restore a trashed item'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'rm' -d 'permanently delete matching trash entries'
complete -c rtrash -n 'not __rtrash_seen_subcommand' -f -a 'setup' -d 'install multi-call links, completions, man page'
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
complete -c rtrash -n $put_cond -l preserve-root -d 'do not remove / (default)'
complete -c rtrash -n $put_cond -l no-preserve-root -d 'do not treat / specially'
complete -c rtrash -n $put_cond -l trash-dir -d 'put into this trash root' -r
complete -c rtrash -n $put_cond -l home-only -d 'always use the home trash'
complete -c rtrash -n $put_cond -l plain -d 'skip TUI; require file operands'

# empty
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s n -l dry-run -d 'report what would be removed; show reclaimable size'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s v -l verbose -d 'print each removed item'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -s f -l force -d 'accepted for trash-cli compatibility'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l trash-dir -d 'empty only this trash directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l older-than -d 'only items older than DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l json -d 'JSON summary'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q empty' -l plain -d 'skip TUI'

# list / status
for sub in list status
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l home-only -d 'only the home trash'
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l trash-dir -d 'only this trash directory' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l older-than -d 'only items older than DAYS days' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l newer-than -d 'only items within last DAYS days' -r
    complete -c rtrash -n "__rtrash_seen_subcommand | string match -q $sub" -l json -d 'JSON output'
end

# restore
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -s f -l force -d 'overwrite existing file at original location'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q restore' -l trash-dir -d 'only this trash directory' -r

# rm (trash-rm)
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l trash-dir -d 'only this trash directory' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l home-only -d 'only the home trash'
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l older-than -d 'only match items older than DAYS days' -r
complete -c rtrash -n '__rtrash_seen_subcommand | string match -q rm' -l newer-than -d 'only match items within last DAYS days' -r
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
    complete -c $cmd -l help
    complete -c $cmd -l version
end

complete -c trash-empty -s n -l dry-run
complete -c trash-empty -s v -l verbose
complete -c trash-empty -s f -l force
complete -c trash-empty -l trash-dir -r
complete -c trash-empty -l home-only
complete -c trash-empty -l help
complete -c trash-empty -l version

complete -c trash-list -l home-only
complete -c trash-list -l trash-dir -r
complete -c trash-list -l older-than -r
complete -c trash-list -l newer-than -r
complete -c trash-list -l json
complete -c trash-list -l help
complete -c trash-list -l version

complete -c trash-restore -s f -l force
complete -c trash-restore -l home-only
complete -c trash-restore -l trash-dir -r
complete -c trash-restore -l help
complete -c trash-restore -l version

complete -c trash-rm -l trash-dir -r
complete -c trash-rm -l home-only
complete -c trash-rm -s n -l dry-run
complete -c trash-rm -s f -l force
complete -c trash-rm -s v -l verbose
complete -c trash-rm -l help
complete -c trash-rm -l version
# Multi-call name `rm` is intentionally not completed here (would shadow system rm).
