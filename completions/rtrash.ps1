# PowerShell completion for rtrash and its multi-call names
# Install: add this line to $PROFILE
#   rtrash completions powershell | Out-String | Invoke-Expression

Register-ArgumentCompleter -Native -CommandName @('rtrash', 'trash', 'trash-put', 'trash-empty', 'trash-list', 'trash-restore', 'trash-rm') -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $subs = @('put', 'empty', 'list', 'status', 'trash-stats', 'du', 'restore', 'undo', 'rm', 'who', 'reap', 'finder', 'wsl', 'config', 'setup', 'keys', 'completions', 'man')
    $opts = @{
        'put' = @('-f', '--force', '-i', '-I', '--interactive', '--inspect', '-r', '-R', '--recursive', '-d', '--dir', '-v', '--verbose', '--one-file-system', '--preserve-root', '--no-preserve-root', '--trash-dir=', '--home-only', '--plain', '--no-space-check', '--dry-run', '--in-use=', '--json', '--help', '--version')
        'empty' = @('-n', '--dry-run', '-v', '--verbose', '-f', '--force', '-i', '--interactive', '--all-users', '--plain', '--all', '--older-than=', '--max-size=', '--prune=', '--until-free=', '--dangling', '--json', '--trash-dir=', '--home-only', '--help', '--version')
        'list' = @('--home-only', '--trash-dir=', '--all-users', '--volumes', '--trash-dirs', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--size', '--show-trashpath', '--sort=', '--reverse', '--last=', '--depth=', '--glob=', '--regex=', '--name=', '--cwd-only', '--directory=', '--json', '--format=', '--help', '--version')
        'status' = @('--home-only', '--trash-dir=', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--json', '--help', '--version')
        'trash-stats' = @('--top=', '--home-only', '--trash-dir=', '--history', '--since=', '--total', '--measure', '--space', '--exit-low', '--json', '--help', '--version')
        'du' = @('-n', '--top=', '-x', '--one-file-system', '--include-kernfs', '-L', '--follow-symlinks', '--apparent', '--sort=', '-C', '--count', '--show-mtime', '--show-relative-size', '--bars', '--graph-style=', '--tree', '--tree=', '--no-unicode', '--dirs-first', '--reverse', '-s', '--summarize', '--skip-total', '--inodes', '--types', '--unique', '--shared', '--max-depth=', '--threads=', '--all-dirs', '--artefacts', '--exclude=', '-X', '--exclude-from=', '--files-from=', '--files0-from=', '--exclude-caches', '--type=', '--exclude-type=', '--collapse=', '--exclude-regex=', '--include-regex=', '--no-hidden', '--gitignore', '--dirs', '--files', '--older-than=', '--newer-than=', '--min-age=', '--max-age=', '--since=', '--until=', '--home-only', '--trash-dir=', '--no-git', '--include-dirty', '--json', '--print0', '--dupes', '--trash-dupes', '--link-dupes', '--clone-dupes', '--min-size=', '-o', '--output=', '--devices', '--ncdu=', '--ncdu-binary=', '-f', '--file=', '--stacks', '--pick', '-i', '--interactive', '--no-delete', '--si', '--no-prefix', '--help', '--version')
        'restore' = @('-f', '--force', '--overwrite', '--sort=', '--conflict=', '--to=', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--cwd-only', '--directory=', '--plain', '--no-space-check', '--newest-first', '--oldest-first', '--name=', '--glob=', '--regex=', '-m', '--match=', '--latest', '--all', '--batch=', '-n', '--dry-run', '--json', '--home-only', '--trash-dir=', '--help', '--version')
        'undo' = @('-l', '--list', '-n', '--dry-run', '-f', '--force', '--conflict=', '--json', '--home-only', '--trash-dir=', '--help', '--version')
        'rm' = @('--trash-dir=', '--home-only', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--batch=', '--regex', '-m', '--match=', '-n', '--dry-run', '-f', '--force', '-v', '--verbose', '--json', '--help', '--version')
        'who' = @('--json', '--help', '--version')
        'reap' = @('-p', '--profile=', '--list-profiles', '--roots=', '--depth=', '--tool', '--older-than=', '--age-by=', '-n', '--dry-run', '--permanent', '--trash', '--trash-dir=', '--home-only', '-v', '--verbose', '--shared', '--install-timer', '--watch', '--watch=', '--remove-timer', '--timer-status', '--when-below=', '--purge-trash', '--until=', '--json', '--help', '--version')
        'wsl' = @('--older-than=', '--all', '--json', '--format=', '--glob=', '--regex=', '--name=', '-n', '--dry-run', '-f', '--force', '--help')
        'finder' = @('--trash-dir=', '--all', '-f', '--force', '--conflict=', '-v', '--verbose', '--json', '--help')
        'config' = @('--path', '--default', '--help', '--version')
        'setup' = @('--prefix=', '--bin-dir=', '--with-rm', '--powershell-profile', '-n', '--dry-run', '-f', '--force', '-v', '--verbose', '--help')
        'keys' = @('--list', '-l', '--sample', '--path', '--help')
        'completions' = @()
        'man' = @()
    }
    # The Windows build has its own put, list, empty, restore, status and rm,
    # with fewer options (the Recycle Bin, not a FreeDesktop trash).
    $winOpts = @{
        'put' = @('-f', '--force', '--inspect', '-v', '--verbose', '--dry-run', '--plain', '--json', '--in-use=', '--trash-dir=', '--home-only', '--help', '--version')
        'list' = @('--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--glob=', '--regex=', '--name=', '--cwd-only', '--directory=', '--sort=', '--reverse', '--last=', '--depth=', '--size', '--show-trashpath', '--format=', '--help', '--version')
        'empty' = @('-n', '--dry-run', '-v', '--verbose', '--older-than=', '--max-size=', '--prune=', '--until-free=', '--all', '--json', '--help', '--version')
        'restore' = @('-f', '--force', '--conflict=', '--to=', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--sort=', '--cwd-only', '--directory=', '--name=', '--glob=', '--regex=', '-m', '--match=', '--latest', '--all', '-n', '--dry-run', '--json', '--plain', '--newest-first', '--oldest-first', '--batch=', '--no-space-check', '--help', '--version')
        'status' = @('--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--help', '--version')
        'rm' = @('--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--batch=', '--regex', '--match=', '-n', '--dry-run', '-f', '--force', '-v', '--verbose', '--json', '--help')
    }
    if ($IsWindows -or $env:OS -eq 'Windows_NT') {
        foreach ($k in @($winOpts.Keys)) { $opts[$k] = $winOpts[$k] }
    }
    $multi = @{'trash' = 'put'; 'trash-put' = 'put'; 'trash-empty' = 'empty'; 'trash-list' = 'list'; 'trash-restore' = 'restore'; 'trash-rm' = 'rm'}
    $values = @{'completions' = @('bash', 'zsh', 'fish', 'nu', 'powershell', 'elvish'); 'finder' = @('put', 'list', 'restore'); 'wsl' = @('list', 'restore', 'rm', 'empty')}
    $words = @($commandAst.CommandElements | ForEach-Object { $_.ToString() })
    $cmd = [System.IO.Path]::GetFileNameWithoutExtension($words[0])
    $sub = $multi[$cmd]
    if ($cmd -eq 'rtrash') {
        $sub = $words | Select-Object -Skip 1 | Where-Object { $subs -contains $_ } | Select-Object -First 1
    }
    # Trashed original paths (path) or names (name), for restore and rm.
    $trashed = {
        param($field)
        $fmt = "--format={$field}\0"
        $out = if (Get-Command rtrash -CommandType Application -ErrorAction SilentlyContinue) {
            & rtrash list $fmt 2>$null
        } else {
            & trash-list $fmt 2>$null
        }
        (@($out) -join "`n") -split "`0" | Where-Object { $_ -ne '' }
    }
    if ($sub -in @('restore', 'rm') -and $wordToComplete -notlike '-*') {
        $cands = @(& $trashed 'path')
    } elseif ($sub -eq 'restore' -and $wordToComplete -like '--name=*') {
        $cands = @(& $trashed 'name' | ForEach-Object { "--name=$_" })
    } elseif ($sub -eq 'wsl' -and ($wordToComplete -like '--name=*' -or ($wordToComplete -notlike '-*' -and @(@($words | Select-Object -SkipLast ([int]($wordToComplete -ne ''))) | Where-Object { $_ -in @('restore', 'rm') }).Count -gt 0))) {
        # wsl restore and rm: Recycle Bin items by their path under the
        # automount root; --name=: their file names.
        $wslPaths = @((@(& rtrash wsl list '--format={path}\0' 2>$null) -join "`n") -split "`0" | Where-Object { $_ -ne '' })
        $cands = if ($wordToComplete -like '--name=*') { $wslPaths | ForEach-Object { '--name=' + ($_ -replace '^.*/', '') } } else { $wslPaths }
    } elseif (-not $sub) {
        # Bare rtrash: a subcommand, or the put options it falls through to.
        $cands = if ($wordToComplete -like '-*') { $opts['put'] + @('-h', '-V') } else { $subs }
    } elseif ($wordToComplete -like '-*') {
        $cands = $opts[$sub]
    } else {
        $cands = $values[$sub]
    }
    $cands | Where-Object { $_ -like ([WildcardPattern]::Escape($wordToComplete) + '*') } | ForEach-Object {
        # Quote trashed paths with spaces or quotes so they stay one argument.
        $text = if ($_ -match "[\s'`$]") { "'" + ($_ -replace "'", "''") + "'" } else { $_ }
        [System.Management.Automation.CompletionResult]::new($text, $_, 'ParameterValue', $_)
    }
}
