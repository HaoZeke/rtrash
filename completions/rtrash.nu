# nushell completions for rtrash and its multi-call names
# Preferred install: rtrash setup (into PREFIX/share/nushell/vendor/autoload)
# Or: rtrash completions nu | save -f ($nu.default-config-dir | path join rtrash-completions.nu)
# and add `source rtrash-completions.nu` to config.nu.

def "nu-complete rtrash commands" [] {
    [
        "put"
        "empty"
        "list"
        "status"
        "trash-stats"
        "du"
        "restore"
        "undo"
        "rm"
        "who"
        "reap"
        "finder"
        "wsl"
        "config"
        "setup"
        "keys"
        "completions"
        "man"
    ]
}

def "nu-complete rtrash shells" [] {
    ["bash" "zsh" "fish" "nu" "powershell" "elvish"]
}

def "nu-complete rtrash wsl" [] {
    ["list" "restore" "rm" "empty"]
}

def "nu-complete rtrash finder" [] {
    ["put" "list" "restore"]
}

def "nu-complete rtrash prune" [] {
    ["oldest" "largest"]
}

def "nu-complete rtrash in-use" [] {
    ["warn" "refuse" "ignore"]
}

def "nu-complete rtrash when" [] {
    ["never" "once" "always"]
}

def "nu-complete rtrash preserve-root" [] {
    ["all"]
}

# Trashed original paths or names, for restore and rm.
def "nu-complete rtrash trashed" [field: string] {
    let fmt = $"--format={($field)}\\0"
    try {
        if (which rtrash | is-not-empty) {
            ^rtrash list $fmt
        } else {
            ^trash-list $fmt
        } | split row (char nul) | where $it != ""
    } catch { [] }
}

def "nu-complete rtrash trashed paths" [] {
    nu-complete rtrash trashed path
}

# Recycle Bin items by their path under the automount root.
def "nu-complete rtrash wsl paths" [] {
    try {
        ^rtrash wsl list "--format={path}\\0" | split row (char nul) | where $it != ""
    } catch { [] }
}

# The paths after wsl restore or rm; nothing for the other actions.
def "nu-complete rtrash wsl items" [context: string] {
    if not ($context | split row " " | any {|w| $w in ["restore" "rm"] }) {
        return []
    }
    nu-complete rtrash wsl paths
}

# The file names of the Recycle Bin items, for wsl --name.
def "nu-complete rtrash wsl names" [] {
    nu-complete rtrash wsl paths | each {|p| $p | path basename }
}

def "nu-complete rtrash trashed names" [] {
    nu-complete rtrash trashed name
}

def "nu-complete rtrash conflict" [] {
    ["refuse" "overwrite" "rename" "skip"]
}

export extern "rtrash" [
    --help(-h)  # display help and exit
    --version(-V)  # output version and exit
    command?: string@"nu-complete rtrash commands"
    ...args: string
]

export extern "rtrash put" [
    --force(-f)  # ignore nonexistent files and arguments, never prompt
    -i  # prompt before every removal
    -I  # prompt once before removing more than three files,
    --interactive?: string@"nu-complete rtrash when"  # prompt according to WHEN: never, once (-I), or always (-i)
    --inspect  # prompt before every removal, showing its size and,
    --recursive(-r)  # remove directories and their contents
    -R  # remove directories and their contents
    --dir(-d)  # remove empty directories
    --verbose(-v)  # explain what is being done
    --one-file-system  # accepted for rm compatibility (no-op: entries are
    --preserve-root?: string@"nu-complete rtrash preserve-root"  # do not remove '/', your home directory or a mount point (default)
    --no-preserve-root  # do not treat those specially
    --trash-dir: string  # put into this trash root (creates files/ and info/ if
    --home-only  # always use the home trash (skip volume trash)
    --plain  # never open the put browser; require FILE operands
    --no-space-check  # copy across devices even when the copy would leave
    --dry-run  # run every check and print what would move, moving nothing
    --in-use: string@"nu-complete rtrash in-use"  # when a live process has an item open, mapped or as
    --json  # print a JSON summary on stdout ({trashed, failed});
    --help  # display this help and exit
    --version  # output version information and exit
    ...files: path
]

export extern "rtrash empty" [
    --dry-run(-n)  # report what would be removed without removing anything;
    --verbose(-v)  # print each removed item
    --interactive(-i)  # list the trash directories and ask before emptying
    --force(-f)  # do not ask (cancels an earlier -i)
    --all-users  # every user's trash this account can write (root: all)
    --plain  # skip TUI; non-interactive empty (also used when stdin is not a TTY)
    --all  # let an unscoped empty remove items other puts left
    --older-than: string  # same as positional DAYS, or a duration such as 2h or 1d12h
    --max-size: string  # remove items until the trash (all roots in scope
    --prune: string@"nu-complete rtrash prune"  # which items --max-size removes first
    --until-free: string  # remove the oldest items until each trash file system has SPEC free
    --dangling  # remove only the .trashinfo records whose payload is gone
    --json  # emit a JSON summary on stdout ({dry_run, removed, bytes, days})
    --trash-dir: string  # empty only this trash directory (repeatable);
    --home-only  # only the home trash (skip volume trash discovery)
    --help  # display this help and exit
    --version  # output version information and exit
    days?: string
]

export extern "rtrash list" [
    --home-only  # only the home trash ($XDG_DATA_HOME/Trash)
    --trash-dir: string  # list only this trash directory (repeatable);
    --all-users  # list every user's trash this account can read
    --volumes  # print the mount points searched for trash and exit
    --trash-dirs  # print the trash directories read and exit
    --older-than: string  # only items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only items trashed within the last AGE
    --larger-than: string  # only items taking more than SIZE on disk
    --smaller-than: string  # only items taking less than SIZE on disk
    --size  # add each item's disk usage (payload plus its
    --sort: string  # order by date (default), size, name (file name)
    --reverse  # reverse the order
    --last: int  # only the last N rows of the listing (after --sort and --reverse)
    --depth: int  # under each trashed directory, show what it holds,
    --glob: string  # only items whose original path or file name matches a glob
    --regex: string  # only items whose original path matches RE
    --name: string  # only items with this file name
    --cwd-only  # only items from the current directory or below
    --directory: path  # only items from DIR or below (DIR need not exist)
    --format: string  # print each item through a template ({date} {path} {name} {size} ...)
    --json  # emit a JSON array (scripts; one object per item;
    --help  # display this help and exit
    --version  # output version information and exit
]

export extern "rtrash status" [
    --home-only  # only the home trash ($XDG_DATA_HOME/Trash)
    --trash-dir: string  # only this trash directory (repeatable)
    --older-than: string  # only count items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only count items trashed within the last AGE
    --larger-than: string  # only count items taking more than SIZE on disk
    --smaller-than: string  # only count items taking less than SIZE on disk
    --json  # emit a JSON object (scripts)
    --help  # display this help and exit
    --version  # output version information and exit
]

export extern "rtrash trash-stats" [
    --top: string  # list the N largest items (default 5; 0 for none)
    --home-only  # only the home trash ($XDG_DATA_HOME/Trash)
    --trash-dir: string  # only this trash directory (repeatable)
    --history  # also show each trash's history: its size and items
    --since: string  # with --history, only the lines from the last AGE
    --total  # only size and items, from the stored running total
    --space  # with --total, also give each trash's file system free bytes and inodes
    --exit-low  # with --total, --space and exit 3 when a trash's file system runs low
    --measure  # with --total, measure a trash whose total is missing or stale
    --json  # print a JSON object ({trashes, total}); history
    --help  # display this help and exit
    --version  # output version information and exit
]

export extern "rtrash du" [
    --top(-n): string  # list N directories and N files (default 20)
    --one-file-system(-x)  # skip directories on other file systems
    --include-kernfs  # also walk kernel pseudo file systems such as /proc,
    --follow-symlinks(-L)  # follow symlinks; loops and repeats count once
    --apparent  # rank by apparent size (file length) instead of disk use
    --sort: string  # rank by KEY: disk (default), apparent, items (files
    --count(-C)  # add an ITEMS column (each hard-linked file once)
    --show-mtime  # show the local modification time (YYYY-MM-DD HH:MM)
    --bars  # draw each row share of its PATH total as a bar
    --graph-style: string  # hash, half-block or eighth-block bars (implies --bars)
    --tree?: string  # print each PATH as a tree, DEPTH (default 2) levels down
    --no-unicode  # draw --tree with ASCII lines
    --dirs-first  # in --tree, list each folder's subfolders before its files
    --show-relative-size  # add a SHARE column: each row's percent of its
    --reverse  # print each table in reverse, the top row last
    --summarize(-s)  # print only the totals, no rows (GNU du -s, gdu)
    --skip-total  # print the rows without the Total lines (dust)
    --inodes  # rank by inodes, as --sort=items: files and
    --types  # list file types instead of directories and files:
    --unique  # add a UNIQUE column: the bytes in extents no other
    --shared  # count each extent that reflinks, clones or snapshots
    --max-depth: string  # list only entries at most N levels below PATH
    --threads: string  # walk with N threads (default: one per CPU, or three
    --all-dirs  # list every directory level, not just the one that
    --artefacts  # list only regenerable build artefacts and caches
    --exclude: string  # leave out entries that match PATTERN (repeatable;
    --exclude-from(-X): string  # read patterns from FILE, one per line (# comments
    --files-from: string  # also scan the paths in FILE, one per line (- is
    --files0-from: string  # the same with NUL-separated paths (find -print0)
    --exclude-caches  # leave out directories below PATH that hold a valid
    --type: string  # count only files ending in one of EXTS, such as
    --exclude-type: string  # leave out files ending in one of EXTS
    --collapse: string  # list each directory called NAME as one row
    --exclude-regex: string  # leave out entries whose path matches RE
    --include-regex: string  # count only files whose path matches RE
    --no-hidden  # leave out hidden entries, those named with a
    --gitignore  # leave out what git ignores: .gitignore files,
    --dirs  # list directories only
    --files  # list files only
    --older-than: string  # only list entries whose newest modification is more
    --newer-than: string  # only list entries modified within the last AGE
    --min-age: string  # the same as --older-than (gdu)
    --max-age: string  # the same as --newer-than (gdu)
    --since: string  # only list entries modified at or after DATE
    --until: string  # only list entries whose newest modification is
    --home-only  # consider only the home trash for the trash summary
    --trash-dir: string  # consider only this trash directory (repeatable)
    --no-git  # do not ask git about listed paths
    --include-dirty  # with --print0, also print paths whose git state is
    --json  # emit a JSON object (scripts)
    --print0  # print the listed paths, NUL-separated, for
    --output(-o): string  # write a snapshot of every directory total (to
    --dupes  # list groups of files with the same contents
    --clone-dupes  # replace every copy but the kept one with a reflink clone of it
    --link-dupes  # replace every copy but the kept one with a hard link to it
    --trash-dupes  # trash every copy but one in each duplicate group
    --min-size: string  # with --dupes, skip files smaller than SIZE
    --devices  # list every mounted file system with its size, use
    --ncdu: string  # write an ncdu export of PATH to FILE (- for
    --ncdu-binary: string  # the same in ncdu 2's binary format (ncdu -O),
    --file(-f): string  # list from a snapshot, or an ncdu export (ncdu -o,
    --stacks  # print folded stacks of every directory's own size
    --pick  # open the put browser on the listed rows: mark rows
    --interactive(-i)  # explore PATH one directory at a time (several PATHs
    --no-delete  # with -i, browse only: d, u and b do nothing
    --si  # show sizes in powers of 1000 (kB, MB, GB)
    --help  # display this help and exit
    --version  # output version information and exit
    ...paths: path
]

export extern "rtrash restore" [
    --force(-f)  # overwrite an existing file (same as --conflict=overwrite)
    --conflict: string@"nu-complete rtrash conflict"  # when the destination exists: refuse (default),
    --to: string  # restore into DIR, keeping each item's file name
    --older-than: string  # only items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only items trashed within the last AGE
    --larger-than: string  # only items taking more than SIZE on disk (500M, 1.5G, 64k)
    --smaller-than: string  # only items taking less than SIZE on disk
    --cwd-only  # without PATH, only items trashed from under the current directory
    --directory: path  # without PATH, only items trashed from under DIR
    --plain  # force numbered line selection even on a TTY (no TUI)
    --no-space-check  # copy across devices even when the copy would leave
    --batch: string  # restore one whole put run (an undo --list id, or last)
    --newest-first  # list the most recently trashed item first
    --overwrite  # same as --force (trash-cli spelling)
    --sort: string  # numbered table order: date, size, path or none
    --oldest-first  # list the oldest trashed item first
    --name: string@"nu-complete rtrash trashed names"  # match items by file name or trash name instead of PATH
    --glob: string  # match items by shell pattern: against the original
    --regex: string  # match items whose original path contains REGEX
    --match(-m): string  # read PATH as glob, regex, substring or exact
    --latest  # of several matches, restore the most recently trashed
    --dry-run(-n)  # show where each chosen item would go and change nothing
    --all  # of several matches, restore every one
    --json  # print {restored, failed, skipped} as JSON on stdout;
    --home-only  # only the home trash (skip volume trash)
    --trash-dir: string  # only consider this trash directory (repeatable)
    --help  # display this help and exit
    --version  # output version information and exit
    path?: string@"nu-complete rtrash trashed paths"
]

export extern "rtrash undo" [
    --list(-l)  # list put runs, newest first: id, date, item count, paths
    --dry-run(-n)  # show what would be restored and change nothing
    --force(-f)  # overwrite existing files at the original locations
    --conflict: string@"nu-complete rtrash conflict"  # when an original location is taken: refuse
    --json  # print {batch, restored, failed, skipped} as JSON on
    --home-only  # only the home trash (skip volume trash)
    --trash-dir: string  # only consider this trash directory (repeatable)
    --help  # display this help and exit
    --version  # output version information and exit
    batch?: string
]

export extern "rtrash rm" [
    --trash-dir: string  # only consider this trash directory (repeatable)
    --home-only  # only the home trash (skip volume trash)
    --older-than: string  # only match items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only match items trashed within the last AGE
    --larger-than: string  # only match items taking more than SIZE on disk
    --smaller-than: string  # only match items taking less than SIZE on disk
    --batch: string  # only items from one put run: an ID from `undo --list`, or `last`
    --regex  # PATTERNs are regular expressions on the original path
    --match(-m): string  # how to read PATTERNs: glob, regex, substring or exact
    --dry-run(-n)  # list matches and reclaimable size; do not delete
    --force(-f)  # allow mass patterns that match everything (e.g. '*')
    --verbose(-v)  # print each permanently removed original path
    --json  # emit a JSON summary (and matched originals array)
    --help  # display this help and exit
    --version  # output version information and exit
    ...patterns: string@"nu-complete rtrash trashed paths"
]

export extern "rtrash who" [
    --json  # print a JSON object ({holders, unreadable})
    --help  # display this help and exit
    --version  # output version information and exit
    ...paths: path
]

export extern "rtrash reap" [
    --profile(-p): string  # also reap a profile's caches and temp files, each
    --list-profiles  # list the profiles (with -v, their paths)
    --roots: string  # directories a profile's {roots} globs search, such
    --depth: string  # how deep ** reaches below home or a root (default 4)
    --tool  # hand a profile to its tool's own cleanup (such as
    --older-than: string  # only entries whose newest time is older than AGE:
    --age-by: string  # which times count, from a (access), c (change) and
    --dry-run(-n)  # show what would go and each profile's total
    --permanent  # delete instead of moving to the trash
    --trash  # move to the trash, even for a permanent profile
    --trash-dir: string  # put into this trash root instead of the usual one
    --home-only  # always use the home trash
    --verbose(-v)  # also show each skipped entry and why
    --shared  # also show what a delete would free: the bytes in
    --install-timer?: string  # write a user systemd service and timer, a launchd agent or a Task Scheduler task
    --watch?: string  # reap again every EVERY (default 1m) under --when-below
    --remove-timer  # stop and remove the job --install-timer made; with
    --timer-status  # say whether that job is installed, and its state
    --when-below: string  # run only when a reaped file system has less free
    --purge-trash  # under --when-below, first empty the oldest items
    --until: string  # the target to reap back to under --when-below
    --json  # print a JSON object ({dry_run, older_than,
    --help  # display this help and exit
    --version  # output version information and exit
    ...dirs: path
]

export extern "rtrash finder" [
    --trash-dir: string  # use this Finder trash (default ~/.Trash); the volume
    --all  # restore every item that has a Put Back record
    --force(-f)  # put: ignore missing files; restore: overwrite an
    --conflict: string@"nu-complete rtrash conflict"  # refuse (default), overwrite, rename or skip
    --verbose(-v)  # put: name each trashed file
    --json  # list: a JSON array; restore: {restored, failed, skipped}
    --help  # display this help and exit
    action?: string@"nu-complete rtrash finder"
    ...args: path
]

export extern "rtrash wsl" [
    --json  # list: print a JSON array
    --format: string  # list: print each item through a template
    --glob: string  # items whose path (or name, without a slash) matches
    --regex: string  # items whose path matches
    --name: string@"nu-complete rtrash wsl names"  # items with exactly this file name
    --older-than: string  # empty: items deleted more than AGE ago
    --all  # empty: every item, without asking
    --dry-run(-n)  # rm, empty: print what would go, delete nothing
    --force(-f)  # restore over an existing file
    --help  # display this help and exit
    action?: string@"nu-complete rtrash wsl"
    ...paths: string@"nu-complete rtrash wsl items"
]

export extern "rtrash config" [
    --path  # print where the config file is looked for
    --default  # print a config file with every setting commented out
    --help  # display this help and exit
    --version  # output version information and exit
]

export extern "rtrash setup" [
    --prefix: string  # install root (default: $HOME/.local, or $PREFIX if set)
    --bin-dir: string  # override binary/link directory (default: PREFIX/bin)
    --with-rm  # also link `rm` → rtrash (put into trash; optional)
    --powershell-profile?: path  # also add a line that loads the PowerShell completer to PATH
    --dry-run(-n)  # print actions without writing
    --force(-f)  # replace existing links/files
    --verbose(-v)  # print each path written
    --help  # display this help and exit
]

export extern "rtrash keys" [
    --list(-l)  # print resolved keymap (default)
    --sample  # print a full sample keys.toml to stdout
    --path  # print the default config path
    --help  # this help
]

export extern "rtrash completions" [
    shell: string@"nu-complete rtrash shells"
]

export extern "rtrash man" [
]

export extern "trash-put" [
    --force(-f)  # ignore nonexistent files and arguments, never prompt
    -i  # prompt before every removal
    -I  # prompt once before removing more than three files,
    --interactive?: string@"nu-complete rtrash when"  # prompt according to WHEN: never, once (-I), or always (-i)
    --inspect  # prompt before every removal, showing its size and,
    --recursive(-r)  # remove directories and their contents
    -R  # remove directories and their contents
    --dir(-d)  # remove empty directories
    --verbose(-v)  # explain what is being done
    --one-file-system  # accepted for rm compatibility (no-op: entries are
    --preserve-root?: string@"nu-complete rtrash preserve-root"  # do not remove '/', your home directory or a mount point (default)
    --no-preserve-root  # do not treat those specially
    --trash-dir: string  # put into this trash root (creates files/ and info/ if
    --home-only  # always use the home trash (skip volume trash)
    --plain  # never open the put browser; require FILE operands
    --no-space-check  # copy across devices even when the copy would leave
    --dry-run  # run every check and print what would move, moving nothing
    --in-use: string@"nu-complete rtrash in-use"  # when a live process has an item open, mapped or as
    --json  # print a JSON summary on stdout ({trashed, failed});
    --help  # display this help and exit
    --version  # output version information and exit
    ...files: path
]

export extern "trash-empty" [
    --dry-run(-n)  # report what would be removed without removing anything;
    --verbose(-v)  # print each removed item
    --interactive(-i)  # list the trash directories and ask before emptying
    --force(-f)  # do not ask (cancels an earlier -i)
    --all-users  # every user's trash this account can write (root: all)
    --plain  # skip TUI; non-interactive empty (also used when stdin is not a TTY)
    --all  # let an unscoped empty remove items other puts left
    --older-than: string  # same as positional DAYS, or a duration such as 2h or 1d12h
    --max-size: string  # remove items until the trash (all roots in scope
    --prune: string@"nu-complete rtrash prune"  # which items --max-size removes first
    --until-free: string  # remove the oldest items until each trash file system has SPEC free
    --dangling  # remove only the .trashinfo records whose payload is gone
    --json  # emit a JSON summary on stdout ({dry_run, removed, bytes, days})
    --trash-dir: string  # empty only this trash directory (repeatable);
    --home-only  # only the home trash (skip volume trash discovery)
    --help  # display this help and exit
    --version  # output version information and exit
    days?: string
]

export extern "trash-list" [
    --home-only  # only the home trash ($XDG_DATA_HOME/Trash)
    --trash-dir: string  # list only this trash directory (repeatable);
    --all-users  # list every user's trash this account can read
    --volumes  # print the mount points searched for trash and exit
    --trash-dirs  # print the trash directories read and exit
    --older-than: string  # only items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only items trashed within the last AGE
    --larger-than: string  # only items taking more than SIZE on disk
    --smaller-than: string  # only items taking less than SIZE on disk
    --size  # add each item's disk usage (payload plus its
    --sort: string  # order by date (default), size, name (file name)
    --reverse  # reverse the order
    --last: int  # only the last N rows of the listing (after --sort and --reverse)
    --depth: int  # under each trashed directory, show what it holds,
    --glob: string  # only items whose original path or file name matches a glob
    --regex: string  # only items whose original path matches RE
    --name: string  # only items with this file name
    --cwd-only  # only items from the current directory or below
    --directory: path  # only items from DIR or below (DIR need not exist)
    --format: string  # print each item through a template ({date} {path} {name} {size} ...)
    --json  # emit a JSON array (scripts; one object per item;
    --help  # display this help and exit
    --version  # output version information and exit
]

export extern "trash-restore" [
    --force(-f)  # overwrite an existing file (same as --conflict=overwrite)
    --conflict: string@"nu-complete rtrash conflict"  # when the destination exists: refuse (default),
    --to: string  # restore into DIR, keeping each item's file name
    --older-than: string  # only items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only items trashed within the last AGE
    --larger-than: string  # only items taking more than SIZE on disk (500M, 1.5G, 64k)
    --smaller-than: string  # only items taking less than SIZE on disk
    --cwd-only  # without PATH, only items trashed from under the current directory
    --directory: path  # without PATH, only items trashed from under DIR
    --plain  # force numbered line selection even on a TTY (no TUI)
    --no-space-check  # copy across devices even when the copy would leave
    --batch: string  # restore one whole put run (an undo --list id, or last)
    --newest-first  # list the most recently trashed item first
    --overwrite  # same as --force (trash-cli spelling)
    --sort: string  # numbered table order: date, size, path or none
    --oldest-first  # list the oldest trashed item first
    --name: string@"nu-complete rtrash trashed names"  # match items by file name or trash name instead of PATH
    --glob: string  # match items by shell pattern: against the original
    --regex: string  # match items whose original path contains REGEX
    --match(-m): string  # read PATH as glob, regex, substring or exact
    --latest  # of several matches, restore the most recently trashed
    --dry-run(-n)  # show where each chosen item would go and change nothing
    --all  # of several matches, restore every one
    --json  # print {restored, failed, skipped} as JSON on stdout;
    --home-only  # only the home trash (skip volume trash)
    --trash-dir: string  # only consider this trash directory (repeatable)
    --help  # display this help and exit
    --version  # output version information and exit
    path?: string@"nu-complete rtrash trashed paths"
]

export extern "trash-rm" [
    --trash-dir: string  # only consider this trash directory (repeatable)
    --home-only  # only the home trash (skip volume trash)
    --older-than: string  # only match items trashed more than AGE ago (DAYS, 2h, 1d12h)
    --newer-than: string  # only match items trashed within the last AGE
    --larger-than: string  # only match items taking more than SIZE on disk
    --smaller-than: string  # only match items taking less than SIZE on disk
    --dry-run(-n)  # list matches and reclaimable size; do not delete
    --batch: string  # only items from one put run: an ID from `undo --list`, or `last`
    --regex  # PATTERNs are regular expressions on the original path
    --match(-m): string  # how to read PATTERNs: glob, regex, substring or exact
    --force(-f)  # allow mass patterns that match everything (e.g. '*')
    --verbose(-v)  # print each permanently removed original path
    --json  # emit a JSON summary (and matched originals array)
    --help  # display this help and exit
    --version  # output version information and exit
    ...patterns: string@"nu-complete rtrash trashed paths"
]
