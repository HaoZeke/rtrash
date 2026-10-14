# PowerShell completion for rtrash and its multi-call names
# Install: add this line to $PROFILE
#   rtrash completions powershell | Out-String | Invoke-Expression

Register-ArgumentCompleter -Native -CommandName @('rtrash', 'trash', 'trash-put', 'trash-empty', 'trash-list', 'trash-restore', 'trash-rm') -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $subs = @('put', 'empty', 'list', 'status', 'trash-stats', 'du', 'restore', 'undo', 'rm', 'who', 'reap', 'finder', 'config', 'setup', 'keys', 'completions', 'man')
    $opts = @{
        'put' = @('-f', '--force', '-i', '-I', '--interactive', '-r', '-R', '--recursive', '-d', '--dir', '-v', '--verbose', '--one-file-system', '--preserve-root', '--no-preserve-root', '--trash-dir=', '--home-only', '--plain', '--no-space-check', '--in-use=', '--json', '--help', '--version')
        'empty' = @('-n', '--dry-run', '-v', '--verbose', '-f', '--force', '--plain', '--older-than=', '--max-size=', '--prune=', '--dangling', '--json', '--trash-dir=', '--home-only', '--help', '--version')
        'list' = @('--home-only', '--trash-dir=', '--all-users', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--size', '--sort=', '--reverse', '--json', '--help', '--version')
        'status' = @('--home-only', '--trash-dir=', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '--json', '--help', '--version')
        'trash-stats' = @('--top=', '--home-only', '--trash-dir=', '--history', '--since=', '--json', '--help', '--version')
        'du' = @('-n', '--top=', '-x', '--one-file-system', '--apparent', '--sort=', '-C', '--count', '--inodes', '--types', '--unique', '--shared', '--max-depth=', '--threads=', '--all-dirs', '--artefacts', '--exclude=', '-X', '--exclude-from=', '--exclude-caches', '--gitignore', '--dirs', '--files', '--older-than=', '--newer-than=', '--since=', '--until=', '--home-only', '--trash-dir=', '--no-git', '--include-dirty', '--json', '--print0', '-o', '--output=', '--devices', '--ncdu=', '--ncdu-binary=', '-f', '--file=', '--stacks', '--pick', '-i', '--interactive', '--no-delete', '--si', '--help', '--version')
        'restore' = @('-f', '--force', '--conflict=', '--to=', '--cwd-only', '--plain', '--no-space-check', '--newest-first', '--oldest-first', '--name=', '--glob=', '--regex=', '--latest', '--all', '--json', '--home-only', '--trash-dir=', '--help', '--version')
        'undo' = @('-l', '--list', '-n', '--dry-run', '-f', '--force', '--conflict=', '--json', '--home-only', '--trash-dir=', '--help', '--version')
        'rm' = @('--trash-dir=', '--home-only', '--older-than=', '--newer-than=', '--larger-than=', '--smaller-than=', '-n', '--dry-run', '-f', '--force', '-v', '--verbose', '--json', '--help', '--version')
        'who' = @('--json', '--help', '--version')
        'reap' = @('-p', '--profile=', '--list-profiles', '--roots=', '--depth=', '--tool', '--older-than=', '--age-by=', '-n', '--dry-run', '--permanent', '--trash', '--trash-dir=', '--home-only', '-v', '--verbose', '--shared', '--install-timer', '--remove-timer', '--timer-status', '--when-below=', '--purge-trash', '--until=', '--json', '--help', '--version')
        'finder' = @('--trash-dir=', '--all', '-f', '--force', '--conflict=', '-v', '--verbose', '--json', '--help')
        'config' = @('--path', '--default', '--help', '--version')
        'setup' = @('--prefix=', '--bin-dir=', '--with-rm', '-n', '--dry-run', '-f', '--force', '-v', '--verbose', '--help')
        'keys' = @('--list', '-l', '--sample', '--path', '--help')
        'completions' = @()
        'man' = @()
    }
    $multi = @{'trash' = 'put'; 'trash-put' = 'put'; 'trash-empty' = 'empty'; 'trash-list' = 'list'; 'trash-restore' = 'restore'; 'trash-rm' = 'rm'}
    $values = @{'completions' = @('bash', 'zsh', 'fish', 'nu', 'powershell'); 'finder' = @('put', 'list', 'restore')}
    $words = @($commandAst.CommandElements | ForEach-Object { $_.ToString() })
    $cmd = [System.IO.Path]::GetFileNameWithoutExtension($words[0])
    $sub = $multi[$cmd]
    if ($cmd -eq 'rtrash') {
        $sub = $words | Select-Object -Skip 1 | Where-Object { $subs -contains $_ } | Select-Object -First 1
    }
    if (-not $sub) {
        # Bare rtrash: a subcommand, or the put options it falls through to.
        $cands = if ($wordToComplete -like '-*') { $opts['put'] + @('-h', '-V') } else { $subs }
    } elseif ($wordToComplete -like '-*') {
        $cands = $opts[$sub]
    } else {
        $cands = $values[$sub]
    }
    $cands | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }
}
