$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$runner = Join-Path $repoRoot 'scripts\run_live_m1_20.ps1'
$focusedRunner = Join-Path $repoRoot 'scripts\run_live_m1_01_07.ps1'
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("kattri-live-runner-" + [guid]::NewGuid().ToString('N'))
$savedLocation = Get-Location
$savedUserWasSet = Test-Path Env:JELLYFIN_TEST_USER
$savedUser = $env:JELLYFIN_TEST_USER
$savedPasswordWasSet = Test-Path Env:JELLYFIN_TEST_PASSWORD
$savedPassword = $env:JELLYFIN_TEST_PASSWORD
$script:CargoCalls = 0
$script:RunnerOutputRaceTarget = $null
$script:CreateOutputRace = $false

function cargo {
    $script:CargoCalls += 1
    if ($script:CreateOutputRace -and $script:CargoCalls -eq 1) {
        [System.IO.File]::WriteAllText($script:RunnerOutputRaceTarget, 'pre-existing race fixture')
    }
    Write-Output 'fixture output https://fixture.invalid/path?token=fixture-token api_key=fixture-key'
    $global:LASTEXITCODE = 0
}

function Assert-RunnerTest([bool] $condition, [string] $message) {
    if (-not $condition) { throw $message }
}

New-Item -ItemType Directory -Path $tempRoot | Out-Null
try {
    $env:JELLYFIN_TEST_USER = 'fixture-user'
    $env:JELLYFIN_TEST_PASSWORD = 'fixture-password'
    $successPath = Join-Path $tempRoot 'successful-output.txt'
    $script:RunnerOutputRaceTarget = $null
    $script:CreateOutputRace = $false
    $script:CargoCalls = 0
    $successResult = @(& { . $runner -OutputPath $successPath 2>&1 | ForEach-Object { [string]$_ } })
    $captured = $successResult -join "`n"
    Assert-RunnerTest ($successResult[-1] -eq '0') 'Runner fixture invocation should return success.'
    Assert-RunnerTest ($script:CargoCalls -eq 3) 'Runner should invoke the mocked cargo three times.'
    Assert-RunnerTest ($env:JELLYFIN_TEST_USER -eq 'fixture-user') 'Pre-existing test user was not restored.'
    Assert-RunnerTest ($env:JELLYFIN_TEST_PASSWORD -eq 'fixture-password') 'Pre-existing test password was not restored.'
    $written = [System.IO.File]::ReadAllText($successPath)
    Assert-RunnerTest (-not $written.Contains('fixture-password')) 'Output contains the fixture password.'
    Assert-RunnerTest (-not $written.Contains('fixture-user')) 'Output contains the fixture username.'
    Assert-RunnerTest (-not $written.Contains('fixture-token')) 'Output contains the fixture token.'
    Assert-RunnerTest (-not $written.Contains('fixture-key')) 'Output contains the fixture api key.'
    Assert-RunnerTest ($written.Contains('<redacted-url>')) 'Output URL was not redacted.'
    Assert-RunnerTest (-not $captured.Contains('fixture-password')) 'Console output contains the fixture password.'

    $focusedPath = Join-Path $tempRoot 'focused-output.txt'
    $script:CargoCalls = 0
    $focusedResult = @(& { . $focusedRunner -OutputPath $focusedPath 2>&1 | ForEach-Object { [string]$_ } })
    Assert-RunnerTest ($focusedResult[-1] -eq '0') 'Focused runner fixture invocation should return success.'
    Assert-RunnerTest ($script:CargoCalls -eq 1) 'Focused runner should invoke only its selected live test.'
    $focusedWritten = [System.IO.File]::ReadAllText($focusedPath)
    Assert-RunnerTest (-not $focusedWritten.Contains('fixture-password')) 'Focused output contains the fixture password.'
    Assert-RunnerTest (-not $focusedWritten.Contains('fixture-token')) 'Focused output contains the fixture token.'
    Assert-RunnerTest ($focusedWritten.Contains('<redacted-url>')) 'Focused output URL was not redacted.'

    $before = [System.IO.File]::ReadAllBytes($successPath)
    $callsBeforeRefusal = $script:CargoCalls
    $existingOutputError = $null
    try { & { . $runner -OutputPath $successPath | Out-Null } }
    catch { $existingOutputError = $_.Exception.Message }
    Assert-RunnerTest ($existingOutputError -like '*Refusing to overwrite existing validation output*') 'Existing output was not rejected before execution.'
    Assert-RunnerTest ($script:CargoCalls -eq $callsBeforeRefusal) 'Existing output refusal invoked cargo.'
    Assert-RunnerTest ([BitConverter]::ToString($before) -ceq [BitConverter]::ToString([System.IO.File]::ReadAllBytes($successPath))) 'Existing output contents changed.'

    $racePath = Join-Path $tempRoot 'race-output.txt'
    $script:RunnerOutputRaceTarget = $racePath
    $script:CreateOutputRace = $true
    $script:CargoCalls = 0
    $raceError = $null
    try { & { . $runner -OutputPath $racePath | Out-Null } }
    catch { $raceError = $_.Exception.Message }
    Assert-RunnerTest ($raceError -match 'already exists|cannot create') 'CreateNew did not reject a path created after preflight.'
    Assert-RunnerTest ([System.IO.File]::ReadAllText($racePath) -eq 'pre-existing race fixture') 'CreateNew overwrote the race fixture.'
    Assert-RunnerTest ($env:JELLYFIN_TEST_USER -eq 'fixture-user') 'User environment was not restored after output creation failed.'
    Assert-RunnerTest ($env:JELLYFIN_TEST_PASSWORD -eq 'fixture-password') 'Password environment was not restored after output creation failed.'

    $script:CreateOutputRace = $false
    $script:CargoCalls = 0
    $env:JELLYFIN_TEST_USER = 'partial-fixture-user'
    Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    $partialPath = Join-Path $tempRoot 'partial-credentials-output.txt'
    $partialError = $null
    try { & { . $runner -OutputPath $partialPath | Out-Null } }
    catch { $partialError = $_.Exception.Message }
    Assert-RunnerTest ($partialError -like '*Provide both Jellyfin test environment credentials together*') 'Partial credential setup did not fail through the credential helper.'
    Assert-RunnerTest ($env:JELLYFIN_TEST_USER -eq 'partial-fixture-user') 'Partial pre-existing user environment was not restored.'
    Assert-RunnerTest (-not (Test-Path Env:JELLYFIN_TEST_PASSWORD)) 'Previously absent password environment was not restored as absent.'
    Assert-RunnerTest ($script:CargoCalls -eq 0) 'Partial credential failure invoked cargo.'
    Assert-RunnerTest (-not (Test-Path -LiteralPath $partialPath)) 'Partial credential failure created an output file.'

    Write-Output 'Live runner fixture regressions: PASS (sanitization, environment restoration, existing-output refusal, atomic no-overwrite, no-network credential failure).'
}
finally {
    Set-Location -LiteralPath $savedLocation
    if ($savedUserWasSet) { $env:JELLYFIN_TEST_USER = $savedUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($savedPasswordWasSet) { $env:JELLYFIN_TEST_PASSWORD = $savedPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    $resolvedTempRoot = [System.IO.Path]::GetFullPath($tempRoot)
    $resolvedTempBase = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
    if ($resolvedTempRoot.StartsWith($resolvedTempBase, [System.StringComparison]::OrdinalIgnoreCase) -and
        (Split-Path -Leaf $resolvedTempRoot).StartsWith('kattri-live-runner-', [System.StringComparison]::Ordinal)) {
        Remove-Item -LiteralPath $resolvedTempRoot -Recurse -Force
    }
}
