[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = 'Medium')]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$testLeaf = 'claudeplusplus-cu-win-316234'
$localData = [Environment]::GetFolderPath('LocalApplicationData')
if ([string]::IsNullOrWhiteSpace($localData)) {
    throw 'Cannot determine the current user local application data directory.'
}
$tempRoot = [IO.Path]::GetFullPath((Join-Path $localData 'Temp'))
$testRoot = [IO.Path]::GetFullPath((Join-Path $tempRoot $testLeaf))
$comparison = [StringComparison]::OrdinalIgnoreCase
if (-not [IO.Path]::GetDirectoryName($testRoot).Equals($tempRoot, $comparison) -or
    [IO.Path]::GetFileName($testRoot) -ne $testLeaf) {
    throw 'The cleanup target is outside the expected temporary directory.'
}

if (-not (Test-Path -LiteralPath $testRoot)) {
    Write-Output "Already clean: $testRoot"
    return
}

$ancestor = Get-Item -LiteralPath $testRoot -Force
while ($null -ne $ancestor) {
    if (-not $ancestor.PSIsContainer -or
        ($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "Refusing a non-directory or linked cleanup path: $($ancestor.FullName)"
    }
    $ancestor = $ancestor.Parent
}

foreach ($marker in @('windows-anchors.json', 'app-copy\resources\app.asar', 'test-store')) {
    if (-not (Test-Path -LiteralPath (Join-Path $testRoot $marker))) {
        throw "The expected test-copy marker is missing: $marker"
    }
}

$testPrefix = $testRoot + [IO.Path]::DirectorySeparatorChar
$pending = [Collections.Generic.Stack[string]]::new()
$pending.Push($testRoot)
while ($pending.Count -gt 0) {
    foreach ($entry in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
        if (-not $entry.FullName.StartsWith($testPrefix, $comparison) -or
            ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Refusing a linked or out-of-tree entry: $($entry.FullName)"
        }
        if ($entry.PSIsContainer) {
            $pending.Push($entry.FullName)
        }
    }
}

$running = @(Get-CimInstance Win32_Process | Where-Object {
    $_.ExecutablePath -and $_.ExecutablePath.StartsWith($testPrefix, $comparison)
})
if ($running.Count -gt 0) {
    $processIds = ($running | ForEach-Object { $_.ProcessId }) -join ', '
    throw "Close the test-copy processes first (PID: $processIds). No processes were stopped."
}

if ($PSCmdlet.ShouldProcess($testRoot, 'Delete the Computer Use test copy and its isolated test data')) {
    Remove-Item -LiteralPath $testRoot -Recurse -Force
    if (Test-Path -LiteralPath $testRoot) {
        throw "Cleanup did not finish: $testRoot"
    }
    Write-Output "Removed: $testRoot"
}
