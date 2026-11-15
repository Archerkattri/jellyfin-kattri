[CmdletBinding(DefaultParameterSetName = 'LaunchTimed')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchTimed')]
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchEightHour')]
    [ValidateNotNullOrEmpty()]
    [string]$ExecutablePath,

    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchTimed')]
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchEightHour')]
    [Parameter(ParameterSetName = 'AttachTimed')]
    [Parameter(ParameterSetName = 'AttachEightHour')]
    [ValidatePattern('^acceptance-[a-z0-9-]{1,21}$')]
    [string]$ProfileName,

    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchTimed')]
    [Parameter(Mandatory = $true, ParameterSetName = 'LaunchEightHour')]
    [ValidateNotNullOrEmpty()]
    [string]$LocalMediaPath,

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

function Get-FileFingerprint {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return [ordered]@{ exists = $false; length_bytes = $null; sha256 = $null }
    }

    $file = Get-Item -LiteralPath $Path
    if ($file.Length -gt 16MB) {
        throw 'A protected profile-state file exceeds the 16 MiB verification limit.'
    }

    return [ordered]@{
        exists = $true
        length_bytes = [long]$file.Length
        sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

function Assert-NoReparsePointPath {
    param([string[]]$Paths)

    foreach ($path in $Paths) {
        $current = [IO.Path]::GetFullPath($path)
        if ($current.StartsWith('\\', [StringComparison]::Ordinal) -or
            $current.StartsWith('//', [StringComparison]::Ordinal)) {
            throw 'Profile isolation requires local filesystem paths.'
        }

        $root = [IO.Path]::GetPathRoot($current)
        if ([string]::IsNullOrWhiteSpace($root)) {
            throw 'Profile isolation could not determine a filesystem root.'
        }
        $components = $current.Substring($root.Length).Split(
            [char[]]@([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar),
            [System.StringSplitOptions]::RemoveEmptyEntries
        )
        $ancestor = $root
        foreach ($component in $components) {
            $ancestor = [IO.Path]::Combine($ancestor, $component)
            $item = $null
            try {
                $item = Get-Item -LiteralPath $ancestor -Force -ErrorAction Stop
            } catch {
                if ($_.CategoryInfo.Category -notin @('ObjectNotFound', 'PathNotFound')) {
                    throw 'Profile isolation could not inspect a filesystem path.'
                }
            }

            if ($null -ne $item -and
                ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'Profile isolation refuses reparse-point directories or files.'
            }
        }
    }
}

function Assert-LocalMediaDriveType {
    param([IO.DriveType]$DriveType)

    if ($DriveType -ne [IO.DriveType]::Fixed) {
        throw 'Product media must be on a fixed local drive.'
    }
}

function Get-LocalMediaDriveType {
    param([string]$DriveRoot)

    return ([IO.DriveInfo]::new($DriveRoot)).DriveType
}

function Resolve-LocalMediaFile {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw 'Product launch measurement requires a local media file.'
    }

    $lexicalPath = [IO.Path]::GetFullPath($Path)
    if ($Path.Contains('://') -or
        $Path.StartsWith('\\', [StringComparison]::Ordinal) -or
        $Path.StartsWith('//', [StringComparison]::Ordinal) -or
        $lexicalPath.StartsWith('\\', [StringComparison]::Ordinal) -or
        $lexicalPath.StartsWith('//', [StringComparison]::Ordinal)) {
        throw 'Product launch measurement accepts only a local media file, not a URL or network share.'
    }

    $lexicalRoot = [IO.Path]::GetPathRoot($lexicalPath)
    if ([string]::IsNullOrWhiteSpace($lexicalRoot)) {
        throw 'The local acceptance media path has no verifiable drive root.'
    }
    try {
        $driveType = Get-LocalMediaDriveType -DriveRoot $lexicalRoot
    } catch {
        throw 'Could not verify the local acceptance media drive type.'
    }
    Assert-LocalMediaDriveType -DriveType $driveType

    Assert-NoReparsePointPath -Paths @($lexicalPath)
    if (-not (Test-Path -LiteralPath $lexicalPath -PathType Leaf)) {
        throw 'The local acceptance media path must resolve to an existing file.'
    }
    $resolvedPath = (Resolve-Path -LiteralPath $lexicalPath).Path
    $resolvedFullPath = [IO.Path]::GetFullPath($resolvedPath)
    if ($resolvedFullPath.StartsWith('\\', [StringComparison]::Ordinal) -or
        $resolvedFullPath.StartsWith('//', [StringComparison]::Ordinal)) {
        throw 'Product launch measurement accepts only a local media file, not a URL or network share.'
    }

    $resolvedRoot = [IO.Path]::GetPathRoot($resolvedFullPath)
    if ([string]::IsNullOrWhiteSpace($resolvedRoot)) {
        throw 'The resolved media path has no verifiable drive root.'
    }
    try {
        $resolvedDriveType = Get-LocalMediaDriveType -DriveRoot $resolvedRoot
    } catch {
        throw 'Could not verify the resolved media drive type.'
    }
    Assert-LocalMediaDriveType -DriveType $resolvedDriveType
    Assert-NoReparsePointPath -Paths @($resolvedFullPath)
    return $resolvedFullPath
}

function Get-AcceptanceProfileEvidence {
    param([string]$Name)

    if ($Name -notmatch '^acceptance-[a-z0-9-]{1,21}$') {
        throw 'A non-default acceptance profile is required for product runtime measurement.'
    }

    $configDirectory = Join-Path (
        [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
    ) 'Jellyfin\Kattri\config'
    $profilesDirectory = Join-Path $configDirectory 'profiles'
    $profileIndexPath = Join-Path $profilesDirectory 'index.toml'
    $profileDirectory = Join-Path $profilesDirectory $Name
    $profileSettingsPath = Join-Path $profileDirectory 'settings.toml'
    $savedServerListPath = Join-Path $configDirectory 'servers.json'

    Assert-NoReparsePointPath -Paths @(
        $configDirectory,
        $profilesDirectory,
        $profileIndexPath,
        $profileDirectory,
        $profileSettingsPath,
        $savedServerListPath
    )

    if (-not (Test-Path -LiteralPath $profileIndexPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $profileSettingsPath -PathType Leaf)) {
        throw 'The acceptance profile and profile index must already exist.'
    }

    if ((Get-Item -LiteralPath $profileIndexPath).Length -gt 1MB) {
        throw 'The saved profile index exceeds the 1 MiB verification limit.'
    }
    $indexText = Get-Content -LiteralPath $profileIndexPath -Raw
    $profilesMatch = [regex]::Match($indexText, '(?ms)^profiles\s*=\s*\[(.*?)\]')
    $defaultMatch = [regex]::Match($indexText, '(?m)^default\s*=\s*"([^"]+)"\s*$')
    $activeMatch = [regex]::Match($indexText, '(?m)^active\s*=\s*"([^"]+)"\s*$')
    if (-not $profilesMatch.Success -or -not $defaultMatch.Success -or -not $activeMatch.Success) {
        throw 'The saved profile index is missing its profile list or pointers.'
    }
    $listedProfiles = $profilesMatch.Groups[1].Value
    $profileNames = @(
        [regex]::Matches($listedProfiles, '"([a-z0-9-]{1,32})"') |
            ForEach-Object { [string]$_.Groups[1].Value }
    )
    $unparsedProfileList = [regex]::Replace($listedProfiles, '"[a-z0-9-]{1,32}"|[\s,]', '')
    $defaultName = [string]$defaultMatch.Groups[1].Value
    $activeName = [string]$activeMatch.Groups[1].Value
    if ($profileNames.Count -eq 0 -or $unparsedProfileList.Length -ne 0 -or
        @($profileNames | Select-Object -Unique).Count -ne $profileNames.Count -or
        -not ($profileNames -ccontains $Name) -or
        -not ($profileNames -ccontains $defaultName) -or
        -not ($profileNames -ccontains $activeName) -or
        $Name -ceq $defaultName -or $Name -ceq $activeName) {
        throw 'The acceptance profile must be listed separately from the saved default and active profiles.'
    }

    $defaultDirectory = Join-Path $profilesDirectory $defaultName
    $defaultSettingsPath = Join-Path $defaultDirectory 'settings.toml'
    $defaultQueuePath = Join-Path $defaultDirectory 'queue.json'
    $acceptanceQueuePath = Join-Path $profileDirectory 'queue.json'
    Assert-NoReparsePointPath -Paths @(
        $profileSettingsPath,
        $acceptanceQueuePath,
        $defaultDirectory,
        $defaultSettingsPath,
        $defaultQueuePath
    )
    if (-not (Test-Path -LiteralPath $defaultSettingsPath -PathType Leaf)) {
        throw 'The saved default profile settings file is missing.'
    }

    $resolvedProfileDirectory = [IO.Path]::GetFullPath($profileDirectory).TrimEnd('\')
    $resolvedDefaultDirectory = [IO.Path]::GetFullPath($defaultDirectory).TrimEnd('\')
    $acceptanceQueuePath = [IO.Path]::GetFullPath($acceptanceQueuePath)
    $defaultQueuePath = [IO.Path]::GetFullPath($defaultQueuePath)
    if ($resolvedProfileDirectory.Equals($resolvedDefaultDirectory, [StringComparison]::OrdinalIgnoreCase) -or
        $acceptanceQueuePath.Equals($defaultQueuePath, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The acceptance profile must use separate settings and queue paths from default.'
    }

    $stateFiles = [ordered]@{
        profile_index = Get-FileFingerprint -Path $profileIndexPath
        default_settings = Get-FileFingerprint -Path $defaultSettingsPath
        default_queue = Get-FileFingerprint -Path $defaultQueuePath
        saved_server_list = Get-FileFingerprint -Path $savedServerListPath
    }

    return [pscustomobject]@{
        name = $Name
        default_profile_name = $defaultName
        state_files = $stateFiles
    }
}

function Compare-AcceptanceProfileEvidence {
    param([object]$Before)

    if ($null -eq $Before) {
        return $null
    }

    $configDirectory = Join-Path (
        [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
    ) 'Jellyfin\Kattri\config'
    $profilesDirectory = Join-Path $configDirectory 'profiles'
    $defaultDirectory = Join-Path $profilesDirectory $Before.default_profile_name
    $after = [ordered]@{
        profile_index = Get-FileFingerprint -Path (Join-Path $profilesDirectory 'index.toml')
        default_settings = Get-FileFingerprint -Path (Join-Path $defaultDirectory 'settings.toml')
        default_queue = Get-FileFingerprint -Path (Join-Path $defaultDirectory 'queue.json')
        saved_server_list = Get-FileFingerprint -Path (Join-Path $configDirectory 'servers.json')
    }

    $checks = [ordered]@{}
    foreach ($key in $Before.state_files.Keys) {
        $checks[$key + '_unchanged'] =
            ($Before.state_files[$key].exists -eq $after[$key].exists) -and
            ($Before.state_files[$key].length_bytes -eq $after[$key].length_bytes) -and
            ($Before.state_files[$key].sha256 -ceq $after[$key].sha256)
    }
    $checks['all_unchanged'] = @($checks.Values | Where-Object { -not $_ }).Count -eq 0
    return $checks
}

function Get-RunningAcceptanceProfileName {
    param([int]$Id)

    try {
        $processInfo = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $Id"
        if ($null -eq $processInfo -or [string]::IsNullOrWhiteSpace($processInfo.CommandLine)) {
            throw 'The product process command line is unavailable.'
        }
        if ($processInfo.CommandLine -match '(?:^|\s)--profile(?:\s+|=)"?([^"\s]+)"?(?=\s|$)') {
            return [string]$Matches[1]
        }
        throw 'The product process was not launched with an explicit profile.'
    } catch {
        throw 'Could not verify the product process profile argument.'
    }
}

function Close-LaunchedProcessNormally {
    param([Diagnostics.Process]$Process)

    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        $Process.Refresh()
        if ($Process.HasExited) {
            return $true
        }
        if ($Process.MainWindowHandle -ne [IntPtr]::Zero) {
            $null = $Process.CloseMainWindow()
            return $Process.WaitForExit(5000)
        }
        Start-Sleep -Milliseconds 250
    }
    return $false
}

function Get-LaunchedProcessCloseResult {
    param(
        [Diagnostics.Process]$Process,
        [bool]$LaunchedByMonitor,
        [bool]$ObservationComplete
    )

    if (-not $LaunchedByMonitor) {
        return [pscustomobject]@{ outcome = 'not_applicable'; error_type = $null }
    }
    if ($null -eq $Process) {
        return [pscustomobject]@{ outcome = 'process_missing'; error_type = $null }
    }

    try {
        $Process.Refresh()
        if ($Process.HasExited) {
            return [pscustomobject]@{ outcome = 'already_exited'; error_type = $null }
        }
        if (-not $ObservationComplete) {
            return [pscustomobject]@{ outcome = 'not_attempted_after_incomplete_observation'; error_type = $null }
        }
        if (Close-LaunchedProcessNormally -Process $Process) {
            return [pscustomobject]@{ outcome = 'closed_normally'; error_type = $null }
        }
        return [pscustomobject]@{ outcome = 'did_not_close_normally'; error_type = $null }
    } catch {
        return [pscustomobject]@{
            outcome = 'close_error'
            error_type = $_.Exception.GetType().Name
        }
    }
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
$resolvedMediaPath = $null
$profileEvidence = $null
$profileStateAfter = $null
$profileIsolationVerified = $false
$localMediaPathVerified = $false
$normalCloseOutcome = 'not_applicable'
$normalCloseErrorType = $null
if ($isLaunchMode) {
    if (-not (Test-Path -LiteralPath $ExecutablePath -PathType Leaf)) {
        throw 'The executable path does not resolve to a file.'
    }
    $resolvedExecutable = (Resolve-Path -LiteralPath $ExecutablePath).Path
    if ([IO.Path]::GetFileName($resolvedExecutable) -ine 'jellyfin-kattri.exe') {
        throw 'The measured product executable must be jellyfin-kattri.exe.'
    }
    $profileEvidence = Get-AcceptanceProfileEvidence -Name $ProfileName

    $resolvedMediaPath = Resolve-LocalMediaFile -Path $LocalMediaPath
    $localMediaPathVerified = $true

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
        $repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..')).Path
        . (Join-Path $repoRoot 'scripts\libmpv_runtime.ps1')
        $mpvDirectory = Resolve-KattriLibmpvDirectory -RepoRoot $repoRoot -Executable $resolvedExecutable
        if (-not $mpvDirectory) {
            throw 'A Windows libmpv runtime matching the measured executable was not found.'
        }

        $startInfo = [Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $resolvedExecutable
        $startInfo.WorkingDirectory = $repoRoot
        $startInfo.UseShellExecute = $false
        $startInfo.Arguments = '--profile {0} "{1}"' -f $ProfileName, $resolvedMediaPath
        $childEnvironment = $startInfo.EnvironmentVariables
        foreach ($name in @(
            'JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD',
            'KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD',
            'KATTRI_DEBUG_DIR', 'KATTRI_LOG'
        )) {
            $childEnvironment.Remove($name) | Out-Null
        }
        $childEnvironment['KATTRI_DEBUG_STREAM_ONLY'] = '1'
        $childEnvironment['PATH'] = "$mpvDirectory;$($env:PATH)"
        $process = [Diagnostics.Process]::Start($startInfo)
        if ($null -eq $process) {
            throw 'The measured Kattri process did not start.'
        }
        $launchedByMonitor = $true
        try {
            $launchedProfileName = Get-RunningAcceptanceProfileName -Id $process.Id
            if ($launchedProfileName -cne $ProfileName) {
                throw 'The launched product process did not retain the requested acceptance profile argument.'
            }
        } catch {
            $closedNormally = Close-LaunchedProcessNormally -Process $process
            if (-not $closedNormally) {
                Write-Warning ("Could not close the unverified process normally; close its window when available (PID {0})." -f $process.Id)
            }
            throw 'The launched product process could not be verified on the requested acceptance profile.'
        }
        $profileIsolationVerified = $true
        Write-Host "Started isolated local-media Kattri process (PID $($process.Id)); profile verification passed."

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

        if ($process.ProcessName -ieq 'jellyfin-kattri') {
            $attachedProfileName = Get-RunningAcceptanceProfileName -Id $ProcessId
            $profileEvidence = Get-AcceptanceProfileEvidence -Name $attachedProfileName
            $profileIsolationVerified = $true
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
    $completedRequestedObservation =
        (-not $interrupted) -and
        ($null -eq $monitorErrorType) -and
        (-not $processExitedEarly) -and
        ($monitorElapsedSeconds -ge ($requestedDurationSeconds - 1.0))
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

    $closeResult = Get-LaunchedProcessCloseResult `
        -Process $process `
        -LaunchedByMonitor $launchedByMonitor `
        -ObservationComplete $completedRequestedObservation
    $normalCloseOutcome = $closeResult.outcome
    $normalCloseErrorType = $closeResult.error_type
    if ($normalCloseOutcome -eq 'did_not_close_normally') {
        Write-Warning ("Kattri remained open after the bounded normal-close attempt; close its window manually (PID {0})." -f $process.Id)
    } elseif ($normalCloseOutcome -eq 'close_error') {
        Write-Warning ("Kattri could not be closed normally; close its window manually (PID {0}). Error type: {1}." -f $process.Id, $normalCloseErrorType)
    }

    if ($null -ne $process) {
        try {
            $process.Refresh()
            if ($process.HasExited) {
                $processExited = $true
                $processExitCode = $process.ExitCode
                $processExitElapsedSeconds = [Math]::Round(($process.ExitTime.ToUniversalTime() - $monitorStartedAtUtc).TotalSeconds, 2)
            }
        } catch {
            $processExited = $false
        }
    }

    if ($null -ne $profileEvidence) {
        try {
            $profileStateAfter = Compare-AcceptanceProfileEvidence -Before $profileEvidence
        } catch {
            $profileStateAfter = [ordered]@{ all_unchanged = $false; verification_error = $_.Exception.GetType().Name }
        }
    }
    if ($null -ne $profileStateAfter -and -not $profileStateAfter.all_unchanged) {
        $status = 'profile_state_changed'
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
        launched_process_close_outcome = $normalCloseOutcome
        launched_process_close_error_type = $normalCloseErrorType
        acceptance_profile = if ($null -ne $profileEvidence) { $profileEvidence.name } else { $null }
        profile_isolation_verified = $profileIsolationVerified
        profile_state_integrity = $profileStateAfter
        local_media_path_verified = $localMediaPathVerified
        local_media_launch_intent = ($isLaunchMode -and $null -ne $resolvedMediaPath)
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
        workload_note = if ($isLaunchMode) { 'Started with one local-media launch intent. This monitor does not verify startup network activity or capture additional UI actions.' } elseif ($isEightHourMode) { 'Operator declared mixed browse, search, play, seek, pause, stop, and home workload; actions are not independently verified.' } else { 'No workload actions are captured by this monitor.' }
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

if ($interrupted -or $null -ne $monitorErrorType -or $processExitedEarly -or $status -ne 'full_duration_observed' -or
    $normalCloseOutcome -in @('did_not_close_normally', 'close_error')) {
    exit 2
}

exit 0
