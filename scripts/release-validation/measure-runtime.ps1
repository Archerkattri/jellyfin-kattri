[CmdletBinding(DefaultParameterSetName = 'LaunchTimed')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchTimed')]
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchEightHour')]
    [ValidateNotNullOrEmpty()]
    [string]$ExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'AttachTimed')]
    [Parameter(Mandatory = $true, ParameterSetName = 'AttachEightHour')]
    [ValidateRange(1, 2147483647)]
    [int]$ProcessId,

    [Parameter(ParameterSetName = 'LaunchTimed')]
    [Parameter(ParameterSetName = 'AttachTimed')]
    [ValidateRange(1, 480)]
    [int]$DurationMinutes = 15,

    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchEightHour')]
    [Parameter(Mandatory = $true, ParameterSetName = 'AttachEightHour')]
    [switch]$EightHourSoak,

    [ValidateRange(1, 60)]
    [int]$SampleIntervalSeconds = 5,

    [ValidateRange(15, 300)]
    [int]$StartupTimeoutSeconds = 90,

    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'

function Get-Percentile {
    param(
        [double[]]$Values,
        [double]$Percentile
    )

    if ($null -eq $Values -or $Values.Count -eq 0) {
        return $null
    }

    $ordered = @($Values | Sort-Object)
    $index = [Math]::Max(0, [Math]::Ceiling(($Percentile / 100.0) * $ordered.Count) - 1)
    return [Math]::Round([double]$ordered[$index], 2)
}

function Get-Stats {
    param([double[]]$Values)

    if ($null -eq $Values -or $Values.Count -eq 0) {
        return [ordered]@{
            min = $null
            median = $null
            p95 = $null
            max = $null
        }
    }

    return [ordered]@{
        min = [Math]::Round([double](($Values | Measure-Object -Minimum).Minimum), 2)
        median = Get-Percentile -Values $Values -Percentile 50
        p95 = Get-Percentile -Values $Values -Percentile 95
        max = [Math]::Round([double](($Values | Measure-Object -Maximum).Maximum), 2)
    }
}

function Get-PrivateMemorySlopeMbPerHour {
    param([object[]]$Samples)

    if ($null -eq $Samples -or $Samples.Count -lt 2) {
        return $null
    }

    $xs = @($Samples | ForEach-Object { [double]$_.elapsed_seconds / 3600.0 })
    $ys = @($Samples | ForEach-Object { [double]$_.private_mb })
    $sumX = ($xs | Measure-Object -Sum).Sum
    $sumY = ($ys | Measure-Object -Sum).Sum
    $sumXX = 0.0
    $sumXY = 0.0
    for ($i = 0; $i -lt $xs.Count; $i++) {
        $sumXX += $xs[$i] * $xs[$i]
        $sumXY += $xs[$i] * $ys[$i]
    }

    $denominator = ($xs.Count * $sumXX) - ($sumX * $sumX)
    if ([Math]::Abs($denominator) -lt 0.0000001) {
        return $null
    }

    $slope = (($xs.Count * $sumXY) - ($sumX * $sumY)) / $denominator
    return [Math]::Round($slope, 2)
}

function Get-HardwareAndSystemInfo {
    $osCaption = $null
    $osVersion = $null
    $osBuild = $null
    $ramTotalMb = $null
    $computerModel = $null
    $cpuModel = $null
    $logicalProcessors = [Environment]::ProcessorCount

    try {
        $os = Get-CimInstance -ClassName Win32_OperatingSystem
        $osCaption = [string]$os.Caption
        $osVersion = [string]$os.Version
        $osBuild = [string]$os.BuildNumber
        $ramTotalMb = [Math]::Round(([double]$os.TotalVisibleMemorySize / 1024.0), 0)
    } catch {
        # System identification is optional; resource measurements remain useful without it.
    }

    try {
        $computer = Get-CimInstance -ClassName Win32_ComputerSystem
        $computerModel = [string]$computer.Model
    } catch {
        # System identification is optional; resource measurements remain useful without it.
    }

    try {
        $processors = @(Get-CimInstance -ClassName Win32_Processor)
        if ($processors.Count -gt 0) {
            $cpuModel = [string]$processors[0].Name
            $logicalProcessors = [int](($processors | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum)
        }
    } catch {
        # System identification is optional; resource measurements remain useful without it.
    }

    $powerPlanName = $null
    try {
        $powerPlanOutput = (& powercfg.exe /getactivescheme 2>$null | Out-String).Trim()
        if ($powerPlanOutput -match '\(([^()]*)\)\s*$') {
            $powerPlanName = $Matches[1]
        }
    } catch {
        # Power plan is optional on systems where powercfg is unavailable.
    }

    return [ordered]@{
        os_caption = $osCaption
        os_version = $osVersion
        os_build = $osBuild
        computer_model = $computerModel
        cpu_model = $cpuModel
        logical_processors = $logicalProcessors
        total_memory_mb = $ramTotalMb
        power_plan = $powerPlanName
    }
}

function Test-MainWindowResponsive {
    param([IntPtr]$WindowHandle)

    if ($WindowHandle -eq [IntPtr]::Zero) {
        return $null
    }

    $result = [UIntPtr]::Zero
    $sent = [KattriReleaseValidationWindowProbe]::SendMessageTimeout(
        $WindowHandle,
        0,
        [UIntPtr]::Zero,
        [IntPtr]::Zero,
        2,
        1000,
        [ref]$result
    )
    return ($sent -ne [IntPtr]::Zero)
}

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This release monitor currently requires Windows; no cross-platform window responsiveness probe is implemented.'
}

if ($null -eq ('KattriReleaseValidationWindowProbe' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class KattriReleaseValidationWindowProbe
{
    [DllImport("user32.dll", SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(
        IntPtr hWnd,
        uint Msg,
        UIntPtr wParam,
        IntPtr lParam,
        uint flags,
        uint timeout,
        out UIntPtr result);
}
'@
}

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
$systemInfo = Get-HardwareAndSystemInfo
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $repoRoot '.local\release-validation'
}

$isLaunchMode = $PSCmdlet.ParameterSetName.StartsWith('Launch')
$isEightHourMode = $PSCmdlet.ParameterSetName.EndsWith('EightHour')
$requestedDurationSeconds = if ($isEightHourMode) { 8 * 60 * 60 } else { $DurationMinutes * 60 }

$resolvedExecutable = $null
$binaryHash = $null
$productVersion = $null
if ($isLaunchMode) {
    if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
        throw 'The executable path does not resolve to a file.'
    }
    $resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
    $fileInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($resolvedExecutable)
    $productVersion = [string]$fileInfo.ProductVersion
    try {
        $binaryHash = (Get-FileHash -LiteralPath $resolvedExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
    } catch {
        throw 'Could not hash the executable before launch.'
    }
}

if ($isEightHourMode) {
    if (-not [Environment]::UserInteractive) {
        throw 'Eight-hour mode requires an interactive operator confirmation.'
    }
    Write-Host 'The monitor does not drive the app. During the full run, manually repeat browse, search, play, seek, pause, stop, and return-to-home actions.'
    Write-Host 'It records process resources and main-window responsiveness, not the content or actions performed.'
    $confirmation = Read-Host 'Type RUN-8-HOUR-SOAK to start the eight-hour observation'
    if ($confirmation -cne 'RUN-8-HOUR-SOAK') {
        throw 'Eight-hour observation was not confirmed.'
    }
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$runStamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
$csvPath = Join-Path $OutputDirectory "kattri-runtime-$runStamp.csv"
$jsonPath = Join-Path $OutputDirectory "kattri-runtime-$runStamp.json"
if ((Test-Path -LiteralPath $csvPath) -or (Test-Path -LiteralPath $jsonPath)) {
    throw 'A report with this UTC timestamp already exists; wait one second and rerun.'
}

$samples = New-Object 'System.Collections.Generic.List[object]'
$process = $null
$launchedByMonitor = $false
$startupStopwatch = $null
$startupWindowSeconds = $null
$startupOutcome = if ($isLaunchMode) { 'waiting_for_main_window' } else { 'not_measured_attach_mode' }
$monitorWatch = $null
$monitorErrorType = $null
$interrupted = $false
$processExited = $false
$processExitedEarly = $false
$processExitCode = $null
$processExitElapsedSeconds = $null
$monitorStartedAtUtc = $null
$maxUnresponsiveSeconds = 0.0
$unresponsiveSince = $null
$hangEventsOverTenSeconds = 0
$hangEventRecorded = $false
$maxSampleGapSeconds = 0.0
$sampleGapsOverThirtySeconds = 0
$firstSample = $null
$lastSample = $null
$lastCpuSeconds = $null
$lastSampleElapsed = $null
$windowProbeAvailable = $true

try {
    if ($isLaunchMode) {
        $startupStopwatch = [Diagnostics.Stopwatch]::StartNew()
        $process = Start-Process -FilePath $resolvedExecutable -PassThru
        $launchedByMonitor = $true

        while ($startupStopwatch.Elapsed.TotalSeconds -lt $StartupTimeoutSeconds) {
            $process.Refresh()
            if ($process.HasExited) {
                $processExited = $true
                $processExitCode = $process.ExitCode
                $startupOutcome = 'process_exited_before_main_window'
                break
            }

            if ($process.MainWindowHandle -ne [IntPtr]::Zero) {
                $startupWindowSeconds = [Math]::Round($startupStopwatch.Elapsed.TotalSeconds, 3)
                $startupOutcome = 'main_window_handle_observed'
                break
            }
            Start-Sleep -Milliseconds 250
        }

        if (-not $processExited -and $null -eq $startupWindowSeconds) {
            $startupOutcome = 'no_main_window_handle_before_timeout'
        }
    } else {
        $process = Get-Process -Id $ProcessId
        $process.Refresh()
        if ($process.HasExited) {
            throw 'The selected process has already exited.'
        }

        try {
            $modulePath = $process.MainModule.FileName
            if (-not [string]::IsNullOrWhiteSpace($modulePath)) {
                $fileInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo($modulePath)
                $productVersion = [string]$fileInfo.ProductVersion
                $binaryHash = (Get-FileHash -LiteralPath $modulePath -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        } catch {
            $binaryHash = $null
            $productVersion = $null
        }
    }

    $monitorStartedAtUtc = [DateTime]::UtcNow
    $monitorWatch = [Diagnostics.Stopwatch]::StartNew()
    $lastProgressSeconds = 0.0

    while ($monitorWatch.Elapsed.TotalSeconds -lt $requestedDurationSeconds) {
        $process.Refresh()
        if ($process.HasExited) {
            $processExited = $true
            $processExitCode = $process.ExitCode
            $processExitElapsedSeconds = [Math]::Round(($process.ExitTime.ToUniversalTime() - $monitorStartedAtUtc).TotalSeconds, 2)
            $processExitedEarly = $processExitElapsedSeconds -lt $requestedDurationSeconds
            break
        }

        $elapsedSeconds = [Math]::Round($monitorWatch.Elapsed.TotalSeconds, 3)
        if ($null -ne $lastSampleElapsed) {
            $sampleGap = $elapsedSeconds - $lastSampleElapsed
            $maxSampleGapSeconds = [Math]::Max($maxSampleGapSeconds, $sampleGap)
            if ($sampleGap -gt 30.0) {
                $sampleGapsOverThirtySeconds++
            }
        }

        $workingSetMb = [Math]::Round(($process.WorkingSet64 / 1MB), 2)
        $privateMb = [Math]::Round(($process.PrivateMemorySize64 / 1MB), 2)
        $cpuSeconds = $process.TotalProcessorTime.TotalSeconds
        $cpuPercent = 0.0
        if ($null -ne $lastCpuSeconds -and $null -ne $lastSampleElapsed) {
            $wallDelta = $elapsedSeconds - $lastSampleElapsed
            if ($wallDelta -gt 0.0 -and $systemInfo.logical_processors -gt 0) {
                $cpuPercent = [Math]::Max(0.0, (($cpuSeconds - $lastCpuSeconds) / $wallDelta / $systemInfo.logical_processors) * 100.0)
            }
        }

        $handles = $null
        $threads = $null
        try { $handles = [int]$process.HandleCount } catch { }
        try { $threads = [int]$process.Threads.Count } catch { }

        $windowResponding = $null
        $process.Refresh()
        $windowHandle = $process.MainWindowHandle
        if ($windowHandle -ne [IntPtr]::Zero) {
            $pingWatch = [Diagnostics.Stopwatch]::StartNew()
            $windowResponding = Test-MainWindowResponsive -WindowHandle $windowHandle
            $pingWatch.Stop()
            if ($null -eq $windowResponding) {
                $windowProbeAvailable = $false
            } elseif ($windowResponding) {
                $unresponsiveSince = $null
                $hangEventRecorded = $false
            } else {
                if ($null -eq $unresponsiveSince) {
                    $unresponsiveSince = $elapsedSeconds
                    $hangEventRecorded = $false
                }
                $unresponsiveSeconds = [Math]::Max(0.0, $monitorWatch.Elapsed.TotalSeconds - $unresponsiveSince)
                $maxUnresponsiveSeconds = [Math]::Max($maxUnresponsiveSeconds, $unresponsiveSeconds)
                if ($unresponsiveSeconds -ge 10.0 -and -not $hangEventRecorded) {
                    $hangEventsOverTenSeconds++
                    $hangEventRecorded = $true
                }
            }
        } else {
            $windowProbeAvailable = $false
        }

        $sample = [pscustomobject][ordered]@{
            elapsed_seconds = $elapsedSeconds
            working_set_mb = $workingSetMb
            private_mb = $privateMb
            cpu_percent_of_machine = [Math]::Round($cpuPercent, 2)
            handles = $handles
            threads = $threads
            main_window_responding = $windowResponding
        }
        $samples.Add($sample)
        if ($null -eq $firstSample) { $firstSample = $sample }
        $lastSample = $sample
        $lastCpuSeconds = $cpuSeconds
        $lastSampleElapsed = $elapsedSeconds

        if ($elapsedSeconds -ge ($lastProgressSeconds + 300.0)) {
            $lastProgressSeconds = $elapsedSeconds
            Write-Host ("Observed {0:N0} min; {1} samples recorded." -f ($elapsedSeconds / 60.0), $samples.Count)
        }

        $csvRow = $sample | ConvertTo-Csv -NoTypeInformation
        if ($samples.Count -eq 1) {
            [IO.File]::WriteAllLines($csvPath, $csvRow, (New-Object Text.UTF8Encoding($false)))
        } else {
            [IO.File]::AppendAllText($csvPath, [Environment]::NewLine + $csvRow[1], (New-Object Text.UTF8Encoding($false)))
        }

        $remainingMilliseconds = [Math]::Max(0.0, ($requestedDurationSeconds - $monitorWatch.Elapsed.TotalSeconds) * 1000.0)
        if ($remainingMilliseconds -gt 0.0) {
            $sleepMilliseconds = [int][Math]::Min(($SampleIntervalSeconds * 1000), [Math]::Ceiling($remainingMilliseconds))
            Start-Sleep -Milliseconds $sleepMilliseconds
        }
    }

    if (-not $processExited) {
        $process.Refresh()
        if ($process.HasExited) {
            $processExited = $true
            $processExitCode = $process.ExitCode
            $processExitElapsedSeconds = [Math]::Round(($process.ExitTime.ToUniversalTime() - $monitorStartedAtUtc).TotalSeconds, 2)
            $processExitedEarly = $processExitElapsedSeconds -lt $requestedDurationSeconds
        }
    }
} catch [System.Management.Automation.PipelineStoppedException] {
    $interrupted = $true
} catch {
    $monitorErrorType = $_.Exception.GetType().Name
} finally {
    if ($null -ne $monitorWatch) {
        $monitorWatch.Stop()
    }
    if ($null -ne $startupStopwatch) {
        $startupStopwatch.Stop()
    }

    $monitorElapsedSeconds = if ($null -ne $monitorWatch) { [Math]::Round($monitorWatch.Elapsed.TotalSeconds, 2) } else { 0.0 }
    $status = if ($interrupted) {
        'interrupted_partial_observation'
    } elseif ($null -ne $monitorErrorType) {
        'monitor_error'
    } elseif ($processExitedEarly) {
        'process_exited_before_duration'
    } elseif ($monitorElapsedSeconds -ge ($requestedDurationSeconds - 1.0)) {
        'full_duration_observed'
    } else {
        'observation_stopped_early'
    }

    $privateValues = @($samples | ForEach-Object { [double]$_.private_mb })
    $workingSetValues = @($samples | ForEach-Object { [double]$_.working_set_mb })
    $cpuValues = @($samples | ForEach-Object { [double]$_.cpu_percent_of_machine })
    $handleValues = @($samples | Where-Object { $null -ne $_.handles } | ForEach-Object { [double]$_.handles })
    $threadValues = @($samples | Where-Object { $null -ne $_.threads } | ForEach-Object { [double]$_.threads })
    $wallDurationSeconds = 0.0
    if ($null -ne $monitorWatch) {
        $wallDurationSeconds = [Math]::Round($monitorWatch.Elapsed.TotalSeconds, 2)
    }

    $report = [ordered]@{
        schema_version = 1
        status = $status
        started_utc = $runStamp
        requested_duration_seconds = $requestedDurationSeconds
        observed_duration_seconds = $wallDurationSeconds
        sample_interval_seconds = $SampleIntervalSeconds
        sample_count = $samples.Count
        process_name = if ($null -ne $process) { $process.ProcessName } else { $null }
        product_version = $productVersion
        binary_sha256 = $binaryHash
        launched_by_monitor = $launchedByMonitor
        startup = [ordered]@{
            measurement = if ($isLaunchMode) { 'launch_to_main_window_handle' } else { 'not_measured_attach_mode' }
            outcome = $startupOutcome
            seconds = $startupWindowSeconds
            timeout_seconds = if ($isLaunchMode) { $StartupTimeoutSeconds } else { $null }
            first_rendered_frame_measured = $false
        }
        system = $systemInfo
        memory = [ordered]@{
            working_set_mb = Get-Stats -Values $workingSetValues
            private_mb = Get-Stats -Values $privateValues
            private_memory_growth_mb = if ($null -ne $firstSample -and $null -ne $lastSample) { [Math]::Round(($lastSample.private_mb - $firstSample.private_mb), 2) } else { $null }
            private_memory_slope_mb_per_hour = Get-PrivateMemorySlopeMbPerHour -Samples @($samples.ToArray())
        }
        cpu_percent_of_machine = Get-Stats -Values $cpuValues
        handles = Get-Stats -Values $handleValues
        handle_growth = if ($null -ne $firstSample -and $null -ne $lastSample -and $null -ne $firstSample.handles -and $null -ne $lastSample.handles) { [int]($lastSample.handles - $firstSample.handles) } else { $null }
        threads = Get-Stats -Values $threadValues
        thread_growth = if ($null -ne $firstSample -and $null -ne $lastSample -and $null -ne $firstSample.threads -and $null -ne $lastSample.threads) { [int]($lastSample.threads - $firstSample.threads) } else { $null }
        main_window_probe_available = $windowProbeAvailable
        main_window_hang_events_over_10_seconds = $hangEventsOverTenSeconds
        main_window_max_unresponsive_seconds = [Math]::Round($maxUnresponsiveSeconds, 2)
        maximum_sample_gap_seconds = [Math]::Round($maxSampleGapSeconds, 2)
        sample_gaps_over_30_seconds = $sampleGapsOverThirtySeconds
        process_exited = $processExited
        process_exit_elapsed_seconds = $processExitElapsedSeconds
        process_exit_code = $processExitCode
        operator_confirmed_eight_hour_mode = [bool]$isEightHourMode
        workload_actions_automated = $false
        workload_note = if ($isEightHourMode) { 'Operator declared mixed browse, search, play, seek, pause, stop, and home workload; actions are not independently verified.' } else { 'No workload actions are captured by this monitor.' }
        monitor_error_type = $monitorErrorType
        raw_samples_file = [IO.Path]::GetFileName($csvPath)
    }

    $json = $report | ConvertTo-Json -Depth 8
    Set-Content -LiteralPath $jsonPath -Value $json -Encoding UTF8
    Write-Host "Sanitized summary: $jsonPath"
    Write-Host "Sanitized samples: $csvPath"

    if ($null -ne $process) {
        $process.Dispose()
    }
}

if ($interrupted -or $null -ne $monitorErrorType -or $processExitedEarly -or $status -ne 'full_duration_observed') {
    exit 2
}

exit 0
