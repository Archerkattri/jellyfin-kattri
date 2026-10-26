# Format, lint, and test gate for the Rust workspace.
# Runs every step even if an earlier one fails, then prints a summary.
# Exit 0 only when all steps pass.

$ErrorActionPreference = 'Continue'
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $repoRoot

$steps = @(
    @{ Name = 'fmt';    Args = @('fmt', '--all', '--', '--check') },
    @{ Name = 'clippy'; Args = @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') },
    @{ Name = 'test';   Args = @('test', '--workspace') }
)

$results = @()
foreach ($step in $steps) {
    Write-Host ''
    Write-Host ("==> [{0}] cargo {1}" -f $step.Name, ($step.Args -join ' '))
    & cargo @($step.Args)
    $code = $LASTEXITCODE
    $results += [pscustomobject]@{ Name = $step.Name; Code = $code }
}

Write-Host ''
Write-Host '==> [test-credentials] local encrypted credential helper checks'
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'tests/test_credentials.ps1')
$results += [pscustomobject]@{ Name = 'test-credentials'; Code = $LASTEXITCODE }

Write-Host ''
Write-Host '==> [live-runner] no-network validation runner regressions'
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'tests/live_runner.ps1')
$results += [pscustomobject]@{ Name = 'live-runner'; Code = $LASTEXITCODE }

Write-Host ''
Write-Host '==> [release-targets] native host and packaging contract checks'
& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'tests/release_targets.ps1')
$results += [pscustomobject]@{ Name = 'release-targets'; Code = $LASTEXITCODE }

Write-Host ''
Write-Host '================ check summary ================'
$overall = 0
foreach ($r in $results) {
    if ($r.Code -eq 0) {
        Write-Host ("PASS  {0}" -f $r.Name)
    } else {
        Write-Host ("FAIL  {0} (exit {1})" -f $r.Name, $r.Code)
        $overall = 1
    }
}
Write-Host '==============================================='
if ($overall -eq 0) {
    Write-Host 'RESULT: PASS'
} else {
    Write-Host 'RESULT: FAIL'
}
exit $overall
