# Launch the native host with the local synthetic playback sample and no credentials. Windows
# Known Folder APIs determine Kattri's profile directory, so require a pre-created non-default
# test profile instead of relying on APPDATA redirection. `--profile` selects it for this process
# while the saved default/active pointers stay on `default`. `-PreflightOnly` checks setup without
# starting the app. The debug artifacts stay in .local.

param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-z0-9-]{1,32}$')]
    [string] $ProfileName,

    [string] $MediaPath,

    [switch] $PreflightOnly
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$exe = Join-Path $repoRoot 'target\debug\jellyfin-kattri.exe'
$defaultMedia = Join-Path $repoRoot '.local\downloads\jellyfin-test-media\Test Jellyfin 1080p AVC 3M.mp4'
$evidenceRoot = Join-Path $repoRoot '.local\parallel-20261113\acceptance'

if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw 'Debug host is missing. Build it with: cargo build --locked -j 2 -p egui-gl-host-proof --bin jellyfin-kattri'
}
New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
$media = if ([string]::IsNullOrWhiteSpace($MediaPath)) {
    $defaultMedia
} else {
    (Resolve-Path -LiteralPath $MediaPath).Path
}
if (-not (Test-Path -LiteralPath $media -PathType Leaf)) {
    throw 'The local synthetic Jellyfin test clip is missing.'
}
if ($media -ne $defaultMedia) {
    $allowedPrefix = [System.IO.Path]::GetFullPath($evidenceRoot).TrimEnd('\') + '\'
    if (-not $media.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Custom local fixture paths must be inside the private acceptance evidence directory.'
    }
}
if ($ProfileName -eq 'default') {
    throw 'Use a dedicated non-default acceptance profile; the default profile may contain user data.'
}
if (-not $ProfileName.StartsWith('acceptance-', [StringComparison]::Ordinal)) {
    throw 'Use a dedicated profile whose name starts with acceptance-.'
}

$profilesDirectory = Join-Path (
    [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
) 'Jellyfin\Kattri\config\profiles'
$profileSettings = Join-Path (Join-Path $profilesDirectory $ProfileName) 'settings.toml'
$profileIndex = Join-Path $profilesDirectory 'index.toml'
if (-not (Test-Path -LiteralPath $profileSettings -PathType Leaf) -or
    -not (Test-Path -LiteralPath $profileIndex -PathType Leaf)) {
    throw 'The named acceptance profile must already exist in Kattri profile settings.'
}
$indexText = Get-Content -LiteralPath $profileIndex -Raw
$profilesMatch = [regex]::Match($indexText, '(?ms)^profiles\s*=\s*\[(.*?)\]')
$profileToken = '(?m)(^|[,\s])' + [regex]::Escape(('"' + $ProfileName + '"')) + '(?=[,\s]|$)'
if (-not $profilesMatch.Success -or $profilesMatch.Groups[1].Value -notmatch $profileToken) {
    throw 'The named acceptance profile is not listed in the profile index.'
}
if ($indexText -notmatch '(?m)^default\s*=\s*"default"$' -or
    $indexText -notmatch '(?m)^active\s*=\s*"default"$') {
    throw 'The saved default and active profiles must remain default; the explicit --profile override selects the acceptance profile for this process.'
}

. (Join-Path $PSScriptRoot '..\libmpv_runtime.ps1')
$mpvDirectory = Resolve-KattriLibmpvDirectory -RepoRoot $repoRoot -Executable $exe
if (-not $mpvDirectory) {
    throw 'A Windows libmpv runtime matching the debug host was not found.'
}

if ($PreflightOnly) {
    Write-Output 'Preflight passed; no app process was started.'
    Write-Output "Per-process profile override: $ProfileName (saved default/active pointers remain default)."
    Write-Output "Media: $media"
    return
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$runDirectory = Join-Path $evidenceRoot "local-fixture-$stamp"
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null

$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $exe
$startInfo.WorkingDirectory = $repoRoot
$startInfo.UseShellExecute = $false
$startInfo.Arguments = "--profile $ProfileName `"$media`""
$environment = $startInfo.EnvironmentVariables
foreach ($name in @(
    'JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD',
    'KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD',
    'KATTRI_DEBUG_STREAM_ONLY', 'KATTRI_LOG'
)) {
    $environment.Remove($name) | Out-Null
}
$environment['KATTRI_DEBUG_DIR'] = $runDirectory
$environment['KATTRI_DEBUG_STREAM_ONLY'] = '1'
$environment['KATTRI_LOG'] = 'error'
$environment['PATH'] = "$mpvDirectory;$($env:PATH)"

$process = [System.Diagnostics.Process]::Start($startInfo)
if ($null -eq $process) {
    throw 'The native host process did not start.'
}
Start-Sleep -Milliseconds 900
$commandFile = Join-Path $runDirectory 'commands.txt'
[System.IO.File]::WriteAllText(
    $commandFile,
    "tree`nshot`n",
    [System.Text.UTF8Encoding]::new($false)
)
$captureDeadline = [DateTime]::UtcNow.AddSeconds(6)
while ([DateTime]::UtcNow -lt $captureDeadline -and
    (-not (Test-Path (Join-Path $runDirectory 'ui-tree.txt')) -or
        -not (Test-Path (Join-Path $runDirectory 'screenshot.png')))) {
    Start-Sleep -Milliseconds 100
}
Write-Output "Started local fixture playback (pid $($process.Id))."
Write-Output "Evidence directory: $runDirectory"
Write-Output "Profile: $ProfileName"
if ((Test-Path (Join-Path $runDirectory 'ui-tree.txt')) -and
    (Test-Path (Join-Path $runDirectory 'screenshot.png'))) {
    Write-Output 'Initial AccessKit tree and composed-frame screenshot captured.'
} else {
    Write-Output 'The initial debug capture was not ready before the six-second wait ended.'
}
