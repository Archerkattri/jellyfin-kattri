# Starts the debug host signed in to the pre-authorized Jellyfin test account, with a
# non-default acceptance profile and server session reporting disabled. Credentials stay in
# process memory and the child gets only the one-run debug sign-in values.

param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^acceptance-[a-z0-9-]{1,21}$')]
    [string] $ProfileName,

    [switch] $PreflightOnly
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$exe = Join-Path $repoRoot 'target\debug\jellyfin-kattri.exe'
$evidenceRoot = Join-Path $repoRoot '.local\parallel-20261113\acceptance'
$serverUrl = $env:JELLYFIN_TEST_URL
$profilesDirectory = Join-Path (
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
) 'Jellyfin\Kattri\config\profiles'
$profileSettings = Join-Path (Join-Path $profilesDirectory $ProfileName) 'settings.toml'
$profileIndex = Join-Path $profilesDirectory 'index.toml'

if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw 'Debug host is missing. Build it with: cargo build --locked -j 2 -p egui-gl-host-proof --bin jellyfin-kattri'
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
    throw 'The acceptance profile must be listed and the saved default/active profiles must remain default.'
}

. (Join-Path $PSScriptRoot '..\libmpv_runtime.ps1')
$mpvDirectory = Resolve-KattriLibmpvDirectory -RepoRoot $repoRoot -Executable $exe
if (-not $mpvDirectory) {
    throw 'A Windows libmpv runtime matching the debug host was not found.'
}

if ($PreflightOnly) {
    Write-Output 'Preflight passed; no credentials were loaded and no app or server request was started.'
    Write-Output "Per-process profile override: $ProfileName (saved default/active pointers remain default)."
    return
}
if ([string]::IsNullOrWhiteSpace($serverUrl)) {
    throw 'Set JELLYFIN_TEST_URL to the authorized server before loading its bound encrypted credential.'
}

$priorUrl = $env:JELLYFIN_TEST_URL
$hadUrl = Test-Path Env:JELLYFIN_TEST_URL
$priorUser = $env:JELLYFIN_TEST_USER
$hadUser = Test-Path Env:JELLYFIN_TEST_USER
$priorPassword = $env:JELLYFIN_TEST_PASSWORD
$hadPassword = Test-Path Env:JELLYFIN_TEST_PASSWORD
$credential = $null
$userName = $null
$plainPassword = $null
$process = $null
try {
    $env:JELLYFIN_TEST_URL = $serverUrl
    Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue
    Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    . (Join-Path $PSScriptRoot '..\test_credentials.ps1')
    $credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
    if ($null -eq $credential) {
        throw 'The bound encrypted test credential is unavailable; no server request was made.'
    }
    $userName = $credential.UserName
    $plainPassword = $credential.GetNetworkCredential().Password

    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    $runDirectory = Join-Path $evidenceRoot "server-profile-$stamp"
    New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $exe
    $startInfo.WorkingDirectory = $repoRoot
    $startInfo.UseShellExecute = $false
    $startInfo.Arguments = "--profile $ProfileName"
    $environment = $startInfo.EnvironmentVariables
    foreach ($name in @(
        'JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD',
        'KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD',
        'KATTRI_DEBUG_STREAM_ONLY', 'KATTRI_LOG'
    )) {
        $environment.Remove($name) | Out-Null
    }
    $environment['KATTRI_DEBUG_DIR'] = $runDirectory
    $environment['KATTRI_DEBUG_URL'] = $serverUrl
    $environment['KATTRI_DEBUG_USER'] = $userName
    $environment['KATTRI_DEBUG_PASSWORD'] = $plainPassword
    $environment['KATTRI_DEBUG_STREAM_ONLY'] = '1'
    $environment['KATTRI_LOG'] = 'error'
    $environment['PATH'] = "$mpvDirectory;$($env:PATH)"

    $process = [System.Diagnostics.Process]::Start($startInfo)
    if ($null -eq $process) {
        throw 'The debug host process did not start.'
    }
    Start-Sleep -Milliseconds 1200
    [System.IO.File]::WriteAllText(
        (Join-Path $runDirectory 'commands.txt'),
        "tree`nshot`n",
        [System.Text.UTF8Encoding]::new($false)
    )
    Write-Output "Started stream-only acceptance host (pid $($process.Id))."
    Write-Output "Profile: $ProfileName"
    Write-Output "Evidence directory: $runDirectory"
    Write-Output 'Session reporting is disabled for this process.'
} finally {
    if ($hadUrl) { $env:JELLYFIN_TEST_URL = $priorUrl }
    else { Remove-Item Env:JELLYFIN_TEST_URL -ErrorAction SilentlyContinue }
    if ($hadUser) { $env:JELLYFIN_TEST_USER = $priorUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($hadPassword) { $env:JELLYFIN_TEST_PASSWORD = $priorPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    $plainPassword = $null
    $userName = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
}
