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
    $OutputDirectory = Join-Path $repoRoot '.local\parallel-20261113\release-validation-verify'
}
$runDirectory = Join-Path $OutputDirectory ([Guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $runDirectory -Force
$reportsDirectory = Join-Path $runDirectory 'reports'
$null = New-Item -ItemType Directory -Path $reportsDirectory
$powerShellPath = (Get-Process -Id $PID).Path
$fixtureArgumentPath = '"{0}"' -f $fixturePath

$fixture = Start-Process -FilePath $powerShellPath `
    -ArgumentList @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $fixtureArgumentPath, '-DurationSeconds', '75', '-MemoryMiB', '32') `
    -PassThru -WindowStyle Hidden

try {
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
    if ($summaryText -match '[A-Za-z]:\\|\\\\' -or $summaryText -match 'command_line|window_title|machine_name|media_url') {
        throw 'The JSON summary contains a path or a forbidden identity/content field.'
    }
    if ((Get-Content -LiteralPath $sampleFiles[0].FullName -TotalCount 1) -match 'pid|path|title|url|token') {
        throw 'The CSV sample header contains a forbidden identity/content field.'
    }

    Write-Output ("PASS: {0} samples; {1:N1} MiB synthetic private-memory peak; product runtime not exercised." -f $summary.sample_count, $summary.memory.private_mb.max)
    Write-Output ("Private verification output: {0}" -f $runDirectory)
} finally {
    if ($null -ne $fixture) {
        if (-not $fixture.HasExited) {
            $null = $fixture.WaitForExit(30000)
        }
        $fixture.Dispose()
    }
}
