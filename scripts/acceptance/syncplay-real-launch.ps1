# Per-client SyncPlay acceptance launcher. The host writes the per-client position traces that
# syncplay-real-analyze.ps1 reads when KATTRI_DEBUG_DIR is present in the child environment
# together with KATTRI_DEBUG_STREAM_ONLY=1 and KATTRI_DEBUG_SYNCPLAY_STATUS=1. This script clears
# inherited KATTRI_DEBUG_* names and then sets a per-profile KATTRI_DEBUG_DIR, so each client
# writes its own position-trace-<pid>.csv and debug log under .verify\syncplay-real\<profile>.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{64}$')]
    [string]$ExpectedSha256,

    [Parameter(Mandatory = $true)]
    [switch]$WindowSlotReleased,

    [ValidateRange(15, 120)]
    [int]$WindowTimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$exe = Join-Path $repoRoot 'target\debug\jellyfin-kattri.exe'
$serverUrl = $env:JELLYFIN_TEST_URL
if ([string]::IsNullOrWhiteSpace($serverUrl)) {
    throw 'JELLYFIN_TEST_URL is not set; set it to the authorized Jellyfin server base URL before launching.'
}
$serverUrl = $serverUrl.TrimEnd('/')
$profileNames = @('acceptance-syncplay-a', 'acceptance-syncplay-b')
$expectedActive = $null
$expectedDefault = $null
$profileSettingsPaths = @()
$defaultQueuePath = $null
$profileIndexPath = $null
$savedServersPath = $null
$deviceIdPath = $null
$credential = $null
$userName = $null
$plainPassword = $null
$priorUrlPresent = Test-Path Env:JELLYFIN_TEST_URL
$priorUrl = $env:JELLYFIN_TEST_URL
$priorUserPresent = Test-Path Env:JELLYFIN_TEST_USER
$priorUser = $env:JELLYFIN_TEST_USER
$priorPasswordPresent = Test-Path Env:JELLYFIN_TEST_PASSWORD
$priorPassword = $env:JELLYFIN_TEST_PASSWORD
$startedProcesses = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()

function Get-FileFingerprint {
    param([string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return 'missing'
    }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Close-StartedWindowsNormally {
    param([System.Collections.Generic.List[System.Diagnostics.Process]]$Processes)

    foreach ($process in $Processes) {
        try {
            $process.Refresh()
            if (-not $process.HasExited -and $process.MainWindowHandle -ne [IntPtr]::Zero) {
                [void]$process.CloseMainWindow()
            }
        }
        catch {
            # The final summary reports the still-running PID; it is never force-killed.
        }
    }
    foreach ($process in $Processes) {
        try { [void]$process.WaitForExit(15000) } catch { }
    }
}

if (-not $WindowSlotReleased) {
    throw 'The coordinated local display slot must be explicitly confirmed before launching.'
}
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw 'The canonical debug host does not exist.'
}
$binaryHash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($binaryHash -cne $ExpectedSha256.ToLowerInvariant()) {
    throw 'The debug host does not match the announced canonical SHA-256.'
}
if (@(Get-Process -Name 'jellyfin-kattri' -ErrorAction SilentlyContinue).Count -ne 0) {
    throw 'A Kattri process already exists; refusing to risk single-instance or profile cross-talk.'
}

$configDirectory = Join-Path $env:APPDATA 'Jellyfin\Kattri\config'
$profilesDirectory = Join-Path $configDirectory 'profiles'
$profileIndexPath = Join-Path $profilesDirectory 'index.toml'
$savedServersPath = Join-Path $configDirectory 'servers.json'
$deviceIdPath = Join-Path $env:LOCALAPPDATA 'Jellyfin\Kattri\config\device-id'
if (-not (Test-Path -LiteralPath $profileIndexPath -PathType Leaf)) {
    throw 'The existing profile index is unavailable.'
}
$indexBefore = Get-Content -LiteralPath $profileIndexPath -Raw
$defaultMatch = [regex]::Match($indexBefore, '(?m)^default\s*=\s*"([^\"]+)"\s*$')
$activeMatch = [regex]::Match($indexBefore, '(?m)^active\s*=\s*"([^\"]+)"\s*$')
$profilesMatch = [regex]::Match($indexBefore, '(?ms)^profiles\s*=\s*\[(.*?)\]')
if (-not $defaultMatch.Success -or -not $activeMatch.Success -or -not $profilesMatch.Success) {
    throw 'The existing profile index could not be verified.'
}
$expectedDefault = $defaultMatch.Groups[1].Value
$expectedActive = $activeMatch.Groups[1].Value
$listedProfiles = $profilesMatch.Groups[1].Value
foreach ($name in $profileNames) {
    $quotedName = '"' + $name + '"'
    if ($listedProfiles -notmatch [regex]::Escape($quotedName)) {
        throw "Required acceptance profile is not listed: $name"
    }
    $settingsPath = Join-Path (Join-Path $profilesDirectory $name) 'settings.toml'
    if (-not (Test-Path -LiteralPath $settingsPath -PathType Leaf)) {
        throw "Required acceptance profile settings are unavailable: $name"
    }
    if ((Get-Item -LiteralPath $settingsPath).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "Refusing a reparse-point acceptance profile: $name"
    }
    $profileSettingsPaths += $settingsPath
    if ($name -ceq $expectedDefault -or $name -ceq $expectedActive) {
        throw 'Acceptance profiles must stay separate from the saved default and active profiles.'
    }
}
$defaultQueuePath = Join-Path (Join-Path $profilesDirectory $expectedDefault) 'queue.json'
if (-not (Test-Path -LiteralPath $deviceIdPath -PathType Leaf)) {
    throw 'The existing installation device-ID file is unavailable.'
}
$deviceBytesBefore = [System.IO.File]::ReadAllBytes($deviceIdPath)
$deviceText = [System.Text.Encoding]::UTF8.GetString($deviceBytesBefore).Trim()
$parsedDeviceId = [Guid]::Empty
if (-not [Guid]::TryParse($deviceText, [ref]$parsedDeviceId) -or $parsedDeviceId -eq [Guid]::Empty) {
    throw 'The existing installation device ID is not a valid GUID.'
}
$deviceBytesBefore = $null
$deviceText = $null
$parsedDeviceId = [Guid]::Empty

$baseline = [ordered]@{
    profile_index = Get-FileFingerprint -Path $profileIndexPath
    default_queue = Get-FileFingerprint -Path $defaultQueuePath
    saved_servers = Get-FileFingerprint -Path $savedServersPath
    device_id = Get-FileFingerprint -Path $deviceIdPath
}

# Confirm the existing saved-server list already contains the authorized target. Successful
# sign-in then hits the app's duplicate-address no-op and does not add a global entry.
$savedTargetPresent = $false
if (Test-Path -LiteralPath $savedServersPath -PathType Leaf) {
    $savedServers = Get-Content -LiteralPath $savedServersPath -Raw | ConvertFrom-Json
    foreach ($entry in @($savedServers.servers)) {
        if ([string]$entry.base_url -ceq $serverUrl) {
            $savedTargetPresent = $true
        }
    }
}
if (-not $savedTargetPresent) {
    throw 'The authorized server is not already in the saved-server list; refusing to add a global server entry.'
}

. (Join-Path $PSScriptRoot '..\test_credentials.ps1')
. (Join-Path $PSScriptRoot '..\libmpv_runtime.ps1')
$mpvDirectory = Resolve-KattriLibmpvDirectory -RepoRoot $repoRoot -Executable $exe
if (-not $mpvDirectory) {
    throw 'A verified libmpv runtime matching the canonical debug host was not found.'
}

try {
    Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue
    Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    $env:JELLYFIN_TEST_URL = $serverUrl
    $credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
    if ($null -eq $credential) {
        throw 'The existing encrypted credential is unavailable for its saved target.'
    }
    $userName = $credential.UserName
    $plainPassword = $credential.GetNetworkCredential().Password

    $deviceIds = @([Guid]::NewGuid().ToString(), [Guid]::NewGuid().ToString())
    if ($deviceIds[0] -eq $deviceIds[1]) {
        throw 'Could not generate two distinct process device IDs.'
    }

    for ($index = 0; $index -lt $profileNames.Count; $index++) {
        $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $exe
        $startInfo.WorkingDirectory = $repoRoot
        $startInfo.UseShellExecute = $false
        $startInfo.Arguments = "--profile $($profileNames[$index])"

        $environment = $startInfo.EnvironmentVariables
        $inheritedDebugNames = @($environment.Keys | Where-Object { $_ -like 'KATTRI_DEBUG_*' })
        foreach ($name in $inheritedDebugNames) { $environment.Remove($name) | Out-Null }
        foreach ($name in @('JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD', 'KATTRI_LIBMPV_DIR')) {
            $environment.Remove($name) | Out-Null
        }

        $environment['KATTRI_DEBUG_URL'] = $serverUrl
        $environment['KATTRI_DEBUG_USER'] = $userName
        $environment['KATTRI_DEBUG_PASSWORD'] = $plainPassword
        $environment['KATTRI_DEBUG_STREAM_ONLY'] = '1'
        $environment['KATTRI_DEBUG_SYNCPLAY_STATUS'] = '1'
        $environment['KATTRI_DEBUG_DEVICE_ID'] = $deviceIds[$index]
        # Per-client debug directory: the host writes position-trace-<pid>.csv here, which
        # syncplay-real-analyze.ps1 consumes. Keeping one directory per profile also keeps the
        # two debug logs separate.
        $traceDirectory = Join-Path (Join-Path $repoRoot '.verify\syncplay-real') $profileNames[$index]
        New-Item -ItemType Directory -Force -Path $traceDirectory | Out-Null
        $environment['KATTRI_DEBUG_DIR'] = $traceDirectory
        $environment['KATTRI_LOG'] = 'off'
        $environment['PATH'] = "$mpvDirectory;$($env:PATH)"

        $process = [System.Diagnostics.Process]::Start($startInfo)
        if ($null -eq $process) {
            throw "Kattri failed to start for $($profileNames[$index])."
        }
        $startedProcesses.Add($process)
        Write-Output "LAUNCH_STARTED profile=$($profileNames[$index]); pid=$($process.Id); window_present=false"
    }

    $deadline = [DateTime]::UtcNow.AddSeconds($WindowTimeoutSeconds)
    $windowReady = @($false, $false)
    while ([DateTime]::UtcNow -lt $deadline -and -not ($windowReady[0] -and $windowReady[1])) {
        for ($index = 0; $index -lt $startedProcesses.Count; $index++) {
            $process = $startedProcesses[$index]
            $process.Refresh()
            if ($process.HasExited) {
                throw "Kattri exited during launch for $($profileNames[$index]); exit_code=$($process.ExitCode)."
            }
            $windowReady[$index] = $process.MainWindowHandle -ne [IntPtr]::Zero
        }
        if (-not ($windowReady[0] -and $windowReady[1])) { Start-Sleep -Milliseconds 500 }
    }
    if (-not ($windowReady[0] -and $windowReady[1])) {
        throw 'Both process windows did not appear before the bounded startup timeout.'
    }

    $afterIndex = Get-Content -LiteralPath $profileIndexPath -Raw
    $afterDefault = [regex]::Match($afterIndex, '(?m)^default\s*=\s*"([^\"]+)"\s*$').Groups[1].Value
    $afterActive = [regex]::Match($afterIndex, '(?m)^active\s*=\s*"([^\"]+)"\s*$').Groups[1].Value
    $checks = [ordered]@{
        profile_index_unchanged = (Get-FileFingerprint -Path $profileIndexPath) -ceq $baseline.profile_index
        default_pointer_unchanged = $afterDefault -ceq $expectedDefault
        active_pointer_unchanged = $afterActive -ceq $expectedActive
        default_queue_unchanged = (Get-FileFingerprint -Path $defaultQueuePath) -ceq $baseline.default_queue
        saved_servers_unchanged = (Get-FileFingerprint -Path $savedServersPath) -ceq $baseline.saved_servers
        global_device_id_unchanged = (Get-FileFingerprint -Path $deviceIdPath) -ceq $baseline.device_id
    }
    if (@($checks.Values | Where-Object { -not $_ }).Count -gt 0) {
        throw 'A protected saved-state fingerprint changed during client startup.'
    }

    Write-Output "LAUNCH_READY profile_a_pid=$($startedProcesses[0].Id); profile_b_pid=$($startedProcesses[1].Id); distinct_device_ids=true; stream_only_reporting_disabled=true; syncplay_status_opt_in=true; binary_sha256=$binaryHash"
    Write-Output 'SAVED_STATE_CHECKS=pass; profile_index/default_pointer/active_pointer/default_queue/saved_server_list/global_device_id unchanged'
    Write-Output 'No Kattri logs, credential files, tokens, item titles, item IDs, or credentialed URLs were written by this launcher.'
}
catch {
    Close-StartedWindowsNormally -Processes $startedProcesses
    throw
}
finally {
    foreach ($process in $startedProcesses) {
        try { $process.Dispose() } catch { }
    }
    $environment = $null
    $startInfo = $null
    $deviceBytesBefore = $null
    $deviceText = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
    $plainPassword = $null
    $userName = $null
    $deviceIds = $null
    if ($priorUrlPresent) { $env:JELLYFIN_TEST_URL = $priorUrl }
    else { Remove-Item Env:JELLYFIN_TEST_URL -ErrorAction SilentlyContinue }
    if ($priorUserPresent) { $env:JELLYFIN_TEST_USER = $priorUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($priorPasswordPresent) { $env:JELLYFIN_TEST_PASSWORD = $priorPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
}
