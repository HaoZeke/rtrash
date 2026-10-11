# Compare `rtrash du` totals with PowerShell on real trees (Windows CI).
#
#   scripts/du-smoke.ps1 -Rtrash target\release\rtrash.exe -Trees DIR, DIR
#
# For each tree (pick ones without hard links or symlinks):
#   - apparent bytes must equal the sum of Length over every file;
#   - the file count must equal the number of files;
#   - disk bytes (allocation size) must sit within a sane band of that sum.
# Then a hard-link fixture checks that three names of one file count once,
# which needs real NTFS file IDs. Exits 1 on any mismatch.
param(
    [Parameter(Mandatory = $true)][string]$Rtrash,
    [Parameter(Mandatory = $true)][string[]]$Trees
)
$ErrorActionPreference = 'Stop'
$failed = $false

function Get-DuRoot([string]$Path) {
    $out = & $Rtrash du --json -n 1 --no-git $Path
    if ($LASTEXITCODE -ne 0) { throw "rtrash du exited $LASTEXITCODE for $Path" }
    return ($out | Out-String | ConvertFrom-Json).roots[0]
}

foreach ($tree in $Trees) {
    $root = Get-DuRoot $tree
    $items = @(Get-ChildItem -LiteralPath $tree -Recurse -Force -File -Attributes !ReparsePoint)
    $sum = [int64]0
    foreach ($f in $items) { $sum += $f.Length }
    $count = $items.Count
    $slack = [int64]$count * 65536
    Write-Host ("{0}: rtrash disk={1} apparent={2} files={3} | PowerShell length={4} files={5}" -f `
            $tree, $root.disk, $root.apparent, $root.files, $sum, $count)
    if ([int64]$root.apparent -ne $sum -or [int64]$root.files -ne $count) {
        Write-Host "MISMATCH: apparent size or file count differs in $tree"
        $failed = $true
    }
    # Small files can live inside the MFT with no clusters, so disk use may
    # fall below the length sum; it can never exceed it by more than a
    # cluster run per file.
    if ([int64]$root.disk -lt [int64]($sum / 2) -or [int64]$root.disk -gt $sum + $slack) {
        Write-Host "MISMATCH: disk use $($root.disk) is outside the sane band for $tree"
        $failed = $true
    }
}

# Hard links: three names, one file.
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) ("rtrash-du-smoke-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path (Join-Path $fixture 'd') | Out-Null
$original = Join-Path $fixture 'd\a.bin'
$bytes = New-Object byte[] 1000000
(New-Object System.Random 7).NextBytes($bytes)
[System.IO.File]::WriteAllBytes($original, $bytes)
New-Item -ItemType HardLink -Path (Join-Path $fixture 'd\b.bin') -Target $original | Out-Null
New-Item -ItemType HardLink -Path (Join-Path $fixture 'c.bin') -Target $original | Out-Null
$root = Get-DuRoot $fixture
Write-Host "hard links: rtrash disk=$($root.disk) apparent=$($root.apparent) files=$($root.files)"
if ([int64]$root.apparent -ne 1000000) {
    Write-Host "MISMATCH: three hard links should count as one 1000000-byte file"
    $failed = $true
}
if ([int64]$root.disk -lt 1000000 -or [int64]$root.disk -gt 1000000 + 65536) {
    Write-Host "MISMATCH: disk use $($root.disk) for one 1000000-byte file"
    $failed = $true
}
Remove-Item -Recurse -Force $fixture

if ($failed) { exit 1 }
Write-Host "du smoke test passed"
