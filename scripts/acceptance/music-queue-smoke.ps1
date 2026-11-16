# Run the music queue against the loopback mock server and opt-in synthetic WAV tracks.
# This is an interactive runtime smoke check; pure queue behavior is also covered by client-core.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^acceptance-[a-z0-9-]{1,21}$')]
    [string] $ProfileName,

    [ValidateRange(1024, 65535)]
    [int] $Port = 18096,

    [switch] $PreflightOnly
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$hostExe = Join-Path $repoRoot 'target\debug\jellyfin-kattri.exe'
$mockExe = Join-Path $repoRoot 'target\debug\mock-jellyfin.exe'
$evidenceRoot = Join-Path $repoRoot '.local\parallel-20261115\music-queue'
$profilesDirectory = Join-Path (
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
) 'Jellyfin\Kattri\config\profiles'
$profileSettings = Join-Path (Join-Path $profilesDirectory $ProfileName) 'settings.toml'
$profileIndex = Join-Path $profilesDirectory 'index.toml'

if ($ProfileName -eq 'default') {
    throw 'Use a dedicated non-default acceptance profile; the default profile may contain user data.'
}
if (-not (Test-Path -LiteralPath $hostExe -PathType Leaf)) {
    throw 'Debug host is missing. Build it with: cargo build --locked -j 2 -p egui-gl-host-proof --bin jellyfin-kattri'
}
if (-not (Test-Path -LiteralPath $mockExe -PathType Leaf)) {
    throw 'Mock server is missing. Build it with: cargo build --locked -j 2 -p mock-jellyfin'
}
if (-not (Test-Path -LiteralPath $profileSettings -PathType Leaf) -or
    -not (Test-Path -LiteralPath $profileIndex -PathType Leaf)) {
    throw 'The named acceptance profile must already exist in Kattri profile settings.'
}
$indexText = Get-Content -LiteralPath $profileIndex -Raw
$profilesMatch = [regex]::Match($indexText, '(?ms)^profiles\s*=\s*\[(.*?)\]')
$profileToken = '(?m)(^|[,\s])' + [regex]::Escape(('"' + $ProfileName + '"')) + '(?=[,\s]|$)'
if (-not $profilesMatch.Success -or $profilesMatch.Groups[1].Value -notmatch $profileToken -or
    $indexText -notmatch '(?m)^default\s*=\s*"default"$' -or
    $indexText -notmatch '(?m)^active\s*=\s*"default"$') {
    throw 'The acceptance profile must be listed while the saved default and active profiles remain default.'
}

. (Join-Path $PSScriptRoot '..\libmpv_runtime.ps1')
$mpvDirectory = Resolve-KattriLibmpvDirectory -RepoRoot $repoRoot -Executable $hostExe
if (-not $mpvDirectory) {
    throw 'A Windows libmpv runtime matching the debug host was not found.'
}

# Keep the launcher and mock contract explicit. These options are intentionally opt-in so the
# mock's normal fixture bytes and catalog remain unchanged for existing tests.
$mockHelp = @(& $mockExe '--help' 2>&1) -join "`n"
foreach ($requiredOption in @('--synthetic-audio-wav', '--audio-items', '--shutdown-file')) {
    if (-not $mockHelp.Contains($requiredOption)) {
        throw "The mock server does not advertise required acceptance option $requiredOption."
    }
}

if ($PreflightOnly) {
    Write-Output 'Preflight passed; no mock or app process was started and no server request was made.'
    Write-Output "Dedicated profile: $ProfileName (saved default/active pointers remain default)."
    Write-Output 'Opt-in synthetic WAV mode and graceful mock shutdown are available.'
    return
}

New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
$stamp = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff')
$runDirectory = Join-Path $evidenceRoot "run-$stamp"
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
$serverUrl = "http://127.0.0.1:$Port/"
$shutdownFile = Join-Path $runDirectory 'mock.shutdown'
$mockLog = Join-Path $runDirectory 'mock.log'
$debugLog = Join-Path $runDirectory 'kattri-debug.log'
$summaryPath = Join-Path $runDirectory 'summary.txt'
$mockProcess = $null
$hostProcess = $null
$resultCode = 1

function New-ChildProcessInfo([string] $FileName, [string[]] $Arguments) {
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $FileName
    $info.WorkingDirectory = $repoRoot
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    # Windows PowerShell 5.1 targets .NET Framework, which has no ArgumentList collection.
    $quoted = foreach ($argument in $Arguments) {
        if ($argument -match '[\s"]') { '"' + ($argument -replace '"', '\"') + '"' } else { $argument }
    }
    $info.Arguments = ($quoted -join ' ')
    $info
}

try {
    $mockInfo = New-ChildProcessInfo $mockExe @(
        '--port', [string] $Port,
        '--audio-items', '8',
        '--synthetic-audio-wav',
        '--shutdown-file', $shutdownFile
    )
    $mockInfo.RedirectStandardOutput = $true
    $mockInfo.RedirectStandardError = $true
    $mockProcess = [System.Diagnostics.Process]::Start($mockInfo)
    if ($null -eq $mockProcess) { throw 'The local mock server process did not start.' }

    $ready = $false
    $readyDeadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $readyDeadline -and -not $mockProcess.HasExited) {
        try {
            $null = Invoke-RestMethod -Uri ($serverUrl + 'System/Info/Public') -TimeoutSec 2
            $ready = $true
            break
        }
        catch {
            Start-Sleep -Milliseconds 150
        }
    }
    if (-not $ready) {
        $mockProcess.Refresh()
        $mockError = if ($mockProcess.HasExited) { $mockProcess.StandardError.ReadToEnd() } else { '' }
        throw "The loopback mock did not become ready. $mockError"
    }

    $hostInfo = New-ChildProcessInfo $hostExe @('--profile', $ProfileName)
    $environment = $hostInfo.EnvironmentVariables
    foreach ($name in @(
        'JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD',
        'KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD',
        'KATTRI_DEBUG_STREAM_ONLY', 'KATTRI_DEBUG_DIR', 'KATTRI_LOG'
    )) {
        $environment.Remove($name) | Out-Null
    }
    $environment['KATTRI_DEBUG_URL'] = $serverUrl.TrimEnd('/')
    $environment['KATTRI_DEBUG_USER'] = 'demo'
    $environment['KATTRI_DEBUG_PASSWORD'] = 'demo'
    $environment['KATTRI_DEBUG_STREAM_ONLY'] = '1'
    $environment['KATTRI_DEBUG_DIR'] = $runDirectory
    $environment['KATTRI_LOG'] = 'error'
    $environment['PATH'] = "$mpvDirectory;$($env:PATH)"
    $hostProcess = [System.Diagnostics.Process]::Start($hostInfo)
    if ($null -eq $hostProcess) { throw 'The local acceptance host process did not start.' }

    Write-Output 'Local music queue smoke run started.'
    Write-Output "Evidence directory: $runDirectory"
    Write-Output "Profile: $ProfileName"
    Write-Output 'The app connects only to the loopback mock with the mock demo account.'
    Write-Output 'Stream-only validation is enabled; playback session reporting is disabled.'
    Write-Output ''
    Write-Output 'Exercise these cases in the app:'
    Write-Output '  1. Start the synthetic music list with Shuffle off and Repeat off; confirm list order.'
    Write-Output '  2. Toggle Shuffle; confirm the current item stays selected and the shuffled order is playable.'
    Write-Output '  3. Test Repeat all at the end, then Repeat one across a natural track end.'
    Write-Output '  4. Test Previous once after the restart threshold and once near the start of a track.'
    Write-Output '  5. With Repeat off, let a track end and confirm the next queued item starts.'
    Write-Output ''
    Write-Output 'Close the app normally when finished. This helper will then audit the mock and shut it down gracefully.'

    while (-not $hostProcess.HasExited) {
        $null = Read-Host 'Close the Kattri window normally, then press Enter here'
        $hostProcess.Refresh()
        if (-not $hostProcess.HasExited) {
            Write-Output 'Kattri is still running; close its window before continuing.'
        }
    }

    $requests = @()
    $sessions = @()
    $auditError = $null
    try {
        $requests = @(Invoke-RestMethod -Uri ($serverUrl + '__mock/requests') -TimeoutSec 5)
        $sessions = @(Invoke-RestMethod -Uri ($serverUrl + '__mock/sessions') -TimeoutSec 5)
    }
    catch {
        $auditError = 'Could not read the mock server audit endpoints after the app closed.'
    }
    $watchedWrites = @($requests | Where-Object {
        $_.Method -in @('POST', 'DELETE') -and $_.Path -match '^/UserPlayedItems(?:/|$)'
    })
    $playbackReports = @($sessions)
    $auditPassed = $null -eq $auditError -and $watchedWrites.Count -eq 0 -and $playbackReports.Count -eq 0
    $lines = @(
        'Local music queue acceptance run',
        "UTC: $([DateTime]::UtcNow.ToString('o'))",
        "Profile: $ProfileName",
        'Server: loopback mock',
        'Synthetic WAV fixture: enabled',
        'KATTRI_DEBUG_STREAM_ONLY: enabled',
        "Mock requests observed: $($requests.Count)",
        "Watched-state writes observed: $($watchedWrites.Count)",
        "Playback session reports observed: $($playbackReports.Count)",
        "Server mutation audit: $(if ($auditPassed) { 'PASS' } else { 'FAIL' })"
    )
    if ($null -ne $auditError) { $lines += $auditError }
    [System.IO.File]::WriteAllLines($summaryPath, $lines, [System.Text.UTF8Encoding]::new($false))
    foreach ($line in $lines) { Write-Output $line }
    $resultCode = if ($auditPassed) { 0 } else { 1 }
}
finally {
    if ($null -ne $hostProcess) {
        $hostProcess.Refresh()
        while (-not $hostProcess.HasExited) {
            Write-Output 'The Kattri window is still open; close it normally so the local mock can shut down.'
            Start-Sleep -Seconds 2
            $hostProcess.Refresh()
        }
        $hostProcess.Dispose()
    }
    if ($null -ne $mockProcess) {
        if (-not $mockProcess.HasExited) {
            [System.IO.File]::WriteAllText($shutdownFile, 'stop', [System.Text.UTF8Encoding]::new($false))
            if (-not $mockProcess.WaitForExit(10000)) {
                Write-Output 'The mock did not acknowledge graceful shutdown; it was left running for manual closure.'
            }
        }
        if ($mockProcess.HasExited) {
            $stdout = $mockProcess.StandardOutput.ReadToEnd()
            $stderr = $mockProcess.StandardError.ReadToEnd()
            [System.IO.File]::WriteAllText(
                $mockLog,
                ($stdout + [Environment]::NewLine + $stderr),
                [System.Text.UTF8Encoding]::new($false)
            )
        }
        $mockProcess.Dispose()
    }
}

exit $resultCode
