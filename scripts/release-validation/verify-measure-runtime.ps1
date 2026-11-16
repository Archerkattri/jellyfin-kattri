[CmdletBinding()]
param(
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'The runtime-monitor verification fixture currently requires Windows.'
}

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$fixturePath = Join-Path $PSScriptRoot 'fixture-process.ps1'
$monitorPath = Join-Path $PSScriptRoot 'measure-runtime.ps1'
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repoRoot '.local\parallel-20261114\soak-verification'
}
$runDirectory = Join-Path $OutputDirectory ([Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $runDirectory -Force
$reportsDirectory = Join-Path $runDirectory 'reports'
$null = New-Item -ItemType Directory -Path $reportsDirectory
$powerShellPath = (Get-Process -Id $PID).Path
$fixtureArgumentPath = '"{0}"' -f $fixturePath
$normalCloseHelperLoaded = $false

$fixture = Start-Process -FilePath $powerShellPath `
    -ArgumentList @('-NoProfile', '-NonInteractive', '-STA', '-ExecutionPolicy', 'Bypass', '-File', $fixtureArgumentPath, '-DurationSeconds', '180', '-MemoryMiB', '32', '-Windowed', '-CancelFirstClose') `
    -PassThru -WindowStyle Minimized

try {
    $windowDeadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $fixture.Refresh()
        if ($fixture.HasExited) { throw 'The synthetic window fixture exited before observation.' }
        if ($fixture.MainWindowHandle -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $windowDeadline)
    if ($fixture.MainWindowHandle -eq [IntPtr]::Zero) {
        throw 'The synthetic fixture did not expose a main-window handle.'
    }

    $monitorOutput = & $powerShellPath -NoProfile -NonInteractive -ExecutionPolicy Bypass `
        -File $monitorPath -ProcessId $fixture.Id -DurationMinutes 1 `
        -SampleIntervalSeconds 5 -OutputDirectory $reportsDirectory 2>&1
    $monitorExitCode = $LASTEXITCODE
    if ($monitorExitCode -ne 0) {
        $safeDiagnostic = @($monitorOutput | ForEach-Object { [string]$_ }) -join [Environment]::NewLine
        throw "The monitor returned exit code $monitorExitCode. Diagnostic output: $safeDiagnostic"
    }

    $summaryFiles = @(Get-ChildItem -LiteralPath $reportsDirectory -Filter 'kattri-runtime-*.json')
    $sampleFiles = @(Get-ChildItem -LiteralPath $reportsDirectory -Filter 'kattri-runtime-*.csv')
    if ($summaryFiles.Count -ne 1 -or $sampleFiles.Count -ne 1) {
        throw 'Expected exactly one JSON summary and one CSV sample file.'
    }

    $summaryText = Get-Content -LiteralPath $summaryFiles[0].FullName -Raw
    $summary = $summaryText | ConvertFrom-Json
    $sampleRows = @(Import-Csv -LiteralPath $sampleFiles[0].FullName)
    if ($summary.status -ne 'full_duration_observed') { throw 'The fixture observation did not finish its requested duration.' }
    if ($summary.sample_count -lt 10 -or $sampleRows.Count -lt 10) { throw 'Too few samples were recorded.' }
    if ($summary.memory.private_mb.max -lt 8) { throw 'The synthetic private-memory allocation was not observed.' }
    if ($summary.startup.first_rendered_frame_measured -ne $false) { throw 'The startup limitation was not recorded.' }
    if ($summary.process_name -notmatch 'powershell|pwsh') { throw 'The observed process name does not match the synthetic fixture.' }
    if ($summary.main_window_probe_available -ne $true) { throw 'The synthetic fixture window was not observed.' }
    if ($summary.launched_process_close_outcome -ne 'not_applicable') { throw 'Attach mode incorrectly claims it closed the observed process.' }
    if ($summary.profile_isolation_verified -ne $false) { throw 'The synthetic fixture was incorrectly reported as an isolated product profile.' }
    if ($summary.local_media_path_verified -ne $false) { throw 'The synthetic fixture was incorrectly reported as a verified local-media launch.' }
    if ($summary.local_media_launch_intent -ne $false) { throw 'The attach-mode fixture was incorrectly reported as a product media launch.' }
    if ($summary.workload_note -match 'no Jellyfin sign-in|no server session was requested') {
        throw 'The summary incorrectly claims that local-media launch cannot trigger startup network activity.'
    }
    if ($summaryText -match '[A-Za-z]:\\|\\\\' -or $summaryText -match 'command_line|window_title|machine_name|media_url') {
        throw 'The JSON summary contains a path or a forbidden identity/content field.'
    }
    if ((Get-Content -LiteralPath $sampleFiles[0].FullName -TotalCount 1) -match 'pid|path|title|url|token') {
        throw 'The CSV sample header contains a forbidden identity/content field.'
    }

    $monitorTokens = $null
    $monitorParseErrors = $null
    $monitorAst = [System.Management.Automation.Language.Parser]::ParseFile(
        $monitorPath,
        [ref]$monitorTokens,
        [ref]$monitorParseErrors
    )
    if ($monitorParseErrors.Count -ne 0) { throw 'The runtime monitor did not parse for the path-safety fixture.' }
    foreach ($functionName in @('Assert-NoReparsePointPath', 'Assert-LocalMediaDriveType', 'Get-LocalMediaDriveType', 'Resolve-LocalMediaFile')) {
        $guardAst = $monitorAst.Find({
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
                $node.Name -eq $functionName
        }, $true)
        if ($null -eq $guardAst) { throw 'A media path-safety helper was not found.' }
        . ([scriptblock]::Create($guardAst.Extent.Text))
    }

    $pathFixtureRoot = [IO.Path]::GetFullPath((Join-Path $runDirectory 'reparse-point-fixture'))
    $ordinaryProfile = [IO.Path]::GetFullPath((Join-Path $pathFixtureRoot 'ordinary-profile'))
    $ordinarySettings = Join-Path $ordinaryProfile 'settings.toml'
    $junctionProfile = [IO.Path]::GetFullPath((Join-Path $pathFixtureRoot 'junction-profile'))
    $outsideTarget = [IO.Path]::GetFullPath((Join-Path $pathFixtureRoot 'outside-target'))
    $outsideMedia = Join-Path $outsideTarget 'media.mp4'
    $null = New-Item -ItemType Directory -Path $ordinaryProfile -Force
    [IO.File]::WriteAllText($ordinarySettings, 'fixture')
    $resolvedOrdinaryMedia = Resolve-LocalMediaFile -Path $ordinarySettings
    if ($resolvedOrdinaryMedia -ne [IO.Path]::GetFullPath($ordinarySettings)) {
        throw 'The media resolver did not return the canonical local path.'
    }
    $null = New-Item -ItemType Directory -Path $outsideTarget -Force
    [IO.File]::WriteAllText($outsideMedia, 'outside fixture target')
    $outsideTargetPath = (Resolve-Path -LiteralPath $outsideTarget).Path
    $null = New-Item -ItemType Junction -Path $junctionProfile -Target $outsideTargetPath
    $junctionPrefix = $junctionProfile.TrimEnd('\') + '\'
    $script:mediaPathProbeCount = 0
    $script:reparseProbePaths = New-Object 'System.Collections.Generic.List[string]'
    function Get-Item {
        [CmdletBinding()]
        param([string]$LiteralPath, [switch]$Force)
        $script:reparseProbePaths.Add($LiteralPath) | Out-Null
        if ($LiteralPath.StartsWith($junctionPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'The verifier detected a child-path probe beneath a junction.'
        }
        Microsoft.PowerShell.Management\Get-Item -LiteralPath $LiteralPath -Force:$Force -ErrorAction Stop
    }
    function Test-Path {
        $script:mediaPathProbeCount++
        throw 'The verifier detected Test-Path before a reparse point was rejected.'
    }
    function Resolve-Path {
        param([string]$LiteralPath)
        $script:mediaPathProbeCount++
        throw 'The verifier detected Resolve-Path before a reparse point was rejected.'
    }
    $reparsePointRejected = $false
    try {
        Resolve-LocalMediaFile -Path (Join-Path $junctionProfile 'media.mp4')
    } catch {
        $reparsePointRejected = $_.Exception.Message -match 'reparse-point'
        if (-not $reparsePointRejected) { throw }
    }
    if (-not $reparsePointRejected) { throw 'The monitor accepted media reached through a junction.' }
    $deeperReparseProbes = @($script:reparseProbePaths | Where-Object {
        $_.StartsWith($junctionPrefix, [StringComparison]::OrdinalIgnoreCase)
    }).Count
    if ($deeperReparseProbes -ne 0 -or $script:mediaPathProbeCount -ne 0) {
        throw 'The monitor probed a junction target before rejecting its reparse-point ancestor.'
    }
    Remove-Item Function:\Get-Item -ErrorAction Stop
    Remove-Item Function:\Test-Path -ErrorAction Stop
    Remove-Item Function:\Resolve-Path -ErrorAction Stop

    $networkDriveRejected = $false
    try {
        Assert-LocalMediaDriveType -DriveType ([IO.DriveType]::Network)
    } catch {
        $networkDriveRejected = $_.Exception.Message -match 'fixed local drive'
        if (-not $networkDriveRejected) { throw }
    }
    if (-not $networkDriveRejected) { throw 'The monitor accepted a mapped network drive type.' }

    $uncPathRejected = $false
    try {
        Resolve-LocalMediaFile -Path '\\example.invalid\share\media.mp4'
    } catch {
        $uncPathRejected = $_.Exception.Message -match 'network share'
        if (-not $uncPathRejected) { throw }
    }
    if (-not $uncPathRejected) { throw 'The monitor accepted a UNC media path.' }

    $script:mediaPathProbeCount = 0
    function Get-LocalMediaDriveType {
        param([string]$DriveRoot)
        return [IO.DriveType]::Network
    }
    function Assert-NoReparsePointPath {
        param([string[]]$Paths)
        $script:mediaPathProbeCount++
    }
    function Test-Path {
        $script:mediaPathProbeCount++
        return $true
    }
    function Resolve-Path {
        param([string]$LiteralPath)
        $script:mediaPathProbeCount++
        return [pscustomobject]@{ Path = $LiteralPath }
    }
    $mappedDriveRejectedBeforeProbe = $false
    try {
        Resolve-LocalMediaFile -Path 'Z:\mapped\media.mp4'
    } catch {
        $mappedDriveRejectedBeforeProbe = $_.Exception.Message -match 'fixed local drive'
        if (-not $mappedDriveRejectedBeforeProbe) { throw }
    }
    if (-not $mappedDriveRejectedBeforeProbe -or $script:mediaPathProbeCount -ne 0) {
        throw 'Mapped network-drive type was not rejected before filesystem probes.'
    }

    $launchGuardOutput = & $powerShellPath -NoProfile -NonInteractive -ExecutionPolicy Bypass `
        -File $monitorPath -ExecutablePath $env:ComSpec -DurationMinutes 1 `
        -OutputDirectory $reportsDirectory 2>&1
    $launchGuardExitCode = $LASTEXITCODE
    if ($launchGuardExitCode -eq 0) {
        throw 'The monitor allowed product launch without an explicit acceptance profile and local media.'
    }

    $monitorSource = Get-Content -LiteralPath $monitorPath -Raw
    if ($monitorSource -notmatch '(?s)\$closeResult\s*=\s*Get-LaunchedProcessCloseResult.*?-LaunchedByMonitor\s+\$launchedByMonitor.*?-ObservationComplete\s+\$completedRequestedObservation' -or
        $monitorSource -notmatch 'launched_process_close_outcome\s*=\s*\$normalCloseOutcome') {
        throw 'The monitor does not report the launched-process close result after observation.'
    }
    $closeResultAst = $monitorAst.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq 'Get-LaunchedProcessCloseResult'
    }, $true)
    $closeWindowAst = $monitorAst.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq 'Close-LaunchedProcessNormally'
    }, $true)
    if ($null -eq $closeResultAst -or $null -eq $closeWindowAst) {
        throw 'The monitor close-result helpers were not found.'
    }
    . ([scriptblock]::Create($closeWindowAst.Extent.Text))
    . ([scriptblock]::Create($closeResultAst.Extent.Text))
    $normalCloseHelperLoaded = $true

    $fixture.Refresh()
    if ($fixture.HasExited) { throw 'Attach mode closed the synthetic fixture automatically.' }
    $attachCloseResult = Get-LaunchedProcessCloseResult -Process $fixture -LaunchedByMonitor $false -ObservationComplete $true
    if ($attachCloseResult.outcome -ne 'not_applicable') { throw 'Attach mode reported a launched-process close.' }
    $fixture.Refresh()
    if ($fixture.HasExited) { throw 'Attach mode changed the synthetic process lifetime.' }

    $incompleteCloseResult = Get-LaunchedProcessCloseResult -Process $fixture -LaunchedByMonitor $true -ObservationComplete $false
    if ($incompleteCloseResult.outcome -ne 'not_attempted_after_incomplete_observation') {
        throw 'An incomplete observation attempted to close the synthetic process.'
    }
    $fixture.Refresh()
    if ($fixture.HasExited) { throw 'An incomplete observation closed the synthetic process.' }

    $launchedCloseResult = Get-LaunchedProcessCloseResult -Process $fixture -LaunchedByMonitor $true -ObservationComplete $true
    if ($launchedCloseResult.outcome -ne 'closed_normally') {
        throw 'A normally completed launched observation did not report a normal close.'
    }
    $fixture.Refresh()
    if (-not $fixture.HasExited) { throw 'The synthetic process remained alive after the normal-close result.' }

    Write-Output ("PASS: {0} samples; {1:N1} MiB synthetic private-memory peak; product runtime not exercised." -f $summary.sample_count, $summary.memory.private_mb.max)
    Write-Output 'PASS: product launch requires an explicit acceptance profile and local media.'
    Write-Output 'PASS: attach mode preserves its process; incomplete observations stay open; normal launched completion retries one rejected close and closes a synthetic windowed process.'
    Write-Output 'PASS: mapped drives reject before probes; UNC and junction targets reject; fixed local files pass.'
    Write-Output ("Private verification output: {0}" -f $runDirectory)
} finally {
    if ($null -ne $fixture) {
        if (-not $fixture.HasExited) {
            $closedNormally = $false
            try {
                if ($normalCloseHelperLoaded) {
                    $closedNormally = Close-LaunchedProcessNormally -Process $fixture
                } else {
                    $null = $fixture.CloseMainWindow()
                    $closedNormally = $fixture.WaitForExit(5000)
                }
            } catch {
                $closedNormally = $false
            }
            if (-not $closedNormally -and -not $fixture.HasExited) {
                Write-Warning 'The synthetic fixture did not close on request; waiting for its bounded timer. No force-stop was used.'
                $null = $fixture.WaitForExit(180000)
            }
        }
        $fixture.Dispose()
    }
}
