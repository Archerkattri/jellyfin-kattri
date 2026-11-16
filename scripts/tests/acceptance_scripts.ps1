# Static checks for the opt-in acceptance helpers. These scripts drive real servers, displays and
# credentials, so the gate can only prove their syntax and their fail-closed contracts here; their
# behavior is exercised by the opt-in runs documented in docs/validation.

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$failures = [System.Collections.Generic.List[string]]::new()

$acceptanceDirectory = Join-Path $repoRoot 'scripts/acceptance'
$releaseValidationDirectory = Join-Path $repoRoot 'scripts/release-validation'
$files = @()
foreach ($directory in @($acceptanceDirectory, $releaseValidationDirectory)) {
    if (-not (Test-Path -LiteralPath $directory)) {
        $failures.Add("missing script directory: $directory")
        continue
    }
    $files += Get-ChildItem -LiteralPath $directory -Filter *.ps1 -File
}

if ($files.Count -eq 0) {
    $failures.Add('no acceptance scripts were found')
}

foreach ($file in $files) {
    $tokens = $null
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$tokens, [ref]$errors)
    foreach ($error in $errors) {
        $failures.Add("$($file.Name): parse error at line $($error.Extent.StartLineNumber): $($error.Message)")
    }
    $text = Get-Content -LiteralPath $file.FullName -Raw
    if ($text -match '10\.50\.0\.10') {
        $failures.Add("$($file.Name): contains the private validation-server address")
    }
    if ($text -match '(?m)^\s*\$serverUrl\s*=\s*''http://') {
        $failures.Add("$($file.Name): hard-codes a plain-HTTP server base instead of resolving JELLYFIN_TEST_URL")
    }
}

# Contract checks: the analyzer and the launcher must keep naming the same trace schema and the
# same debug opt-ins the host emitter uses.
$analyzer = Join-Path $acceptanceDirectory 'syncplay-real-analyze.ps1'
$launcher = Join-Path $acceptanceDirectory 'syncplay-real-launch.ps1'
if (Test-Path -LiteralPath $analyzer) {
    $analyzerText = Get-Content -LiteralPath $analyzer -Raw
    foreach ($column in @('utc_unix_ms', 'monotonic_ms', 'position_ticks', 'paused', 'buffering', 'player_error')) {
        if ($analyzerText -notmatch [regex]::Escape($column)) {
            $failures.Add("syncplay-real-analyze.ps1: trace column '$column' is not named")
        }
    }
}
else {
    $failures.Add('syncplay-real-analyze.ps1 is missing')
}
if (Test-Path -LiteralPath $launcher) {
    $launcherText = Get-Content -LiteralPath $launcher -Raw
    foreach ($name in @('KATTRI_DEBUG_STREAM_ONLY', 'KATTRI_DEBUG_SYNCPLAY_STATUS', 'KATTRI_DEBUG_DEVICE_ID')) {
        if ($launcherText -notmatch [regex]::Escape($name)) {
            $failures.Add("syncplay-real-launch.ps1: does not set $name")
        }
    }
}
else {
    $failures.Add('syncplay-real-launch.ps1 is missing')
}

if ($failures.Count -gt 0) {
    foreach ($failure in $failures) {
        Write-Output "FAIL: $failure"
    }
    exit 1
}
Write-Output "PASS: $($files.Count) acceptance scripts parse, carry no private address, and keep the trace/opt-in contracts."
exit 0
