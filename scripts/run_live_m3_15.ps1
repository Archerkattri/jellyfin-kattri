param(
    [switch] $AllowPlaylistWrites
)

# Creates, edits and deletes only a uniquely named temporary test playlist.
# Use an authorized test account and explicitly opt in with -AllowPlaylistWrites.
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $AllowPlaylistWrites) {
    throw 'Refusing to run: this check writes a temporary playlist. Re-run with -AllowPlaylistWrites.'
}

$hadUser = Test-Path Env:JELLYFIN_TEST_USER
$savedUser = $env:JELLYFIN_TEST_USER
$hadPassword = Test-Path Env:JELLYFIN_TEST_PASSWORD
$savedPassword = $env:JELLYFIN_TEST_PASSWORD
$credential = $null
$username = $null
$password = $null
$pushedLocation = $false
$testExit = 1

try {
    . (Join-Path $PSScriptRoot 'test_credentials.ps1')
    $credential = Get-JellyfinTestCredential
    if ($null -eq $credential) {
        throw 'Test credentials are unavailable; no request was made.'
    }
    $username = $credential.UserName
    $password = $credential.GetNetworkCredential().Password
    $env:JELLYFIN_TEST_USER = $username
    $env:JELLYFIN_TEST_PASSWORD = $password

    Push-Location -LiteralPath $repoRoot
    $pushedLocation = $true
    Write-Host 'Running the M3-15 playlist write check. It creates one kattri-test-m3-15-* playlist, changes it, and deletes it.'
    $savedPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $output = @(& cargo test --quiet -p jellyfin-api --test live_playlist_writes live_playlist_write_check -- --ignored --nocapture 2>&1)
    $testExit = $LASTEXITCODE
    $ErrorActionPreference = $savedPreference

    foreach ($line in $output) {
        $text = [string]$line
        if (-not [string]::IsNullOrEmpty($username)) { $text = $text.Replace($username, '<redacted-user>') }
        if (-not [string]::IsNullOrEmpty($password)) { $text = $text.Replace($password, '<redacted-secret>') }
        $text = [regex]::Replace($text, '(?i)(token|password|username|api_key)=\S+', '$1=<redacted>')
        $text = [regex]::Replace($text, 'https?://\S+', '<redacted-url>')
        Write-Output $text
    }
    Write-Host "M3-15 live check finished with exit code $testExit."
}
finally {
    if ($pushedLocation) { Pop-Location }
    if ($hadUser) { $env:JELLYFIN_TEST_USER = $savedUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($hadPassword) { $env:JELLYFIN_TEST_PASSWORD = $savedPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    $password = $null
    $username = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
}

exit $testExit
