# Enroll an encrypted local test account for the opt-in validation runners.
# Run it yourself in a terminal; it prompts for the server URL, user name and password.
# User name and password are read as SecureString (masked) and saved Windows-DPAPI
# encrypted, bound to this Windows account and the server URL, in
# .local/test-auth/jellyfin-test.clixml (Git-ignored). Nothing is printed or returned.
# scripts/test_credentials.ps1 (Get-JellyfinTestCredential) reads the same file.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\set_test_credentials.ps1

$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'DPAPI encryption needs Windows.' }

$repoRoot = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $repoRoot '.local\test-auth'
$path = Join-Path $dir 'jellyfin-test.clixml'

$default = 'http://127.0.0.1:8096/'
$entered = Read-Host "Jellyfin server URL [$default]"
if ([string]::IsNullOrWhiteSpace($entered)) { $entered = $default }
$uri = [Uri]::new($entered, [UriKind]::Absolute)
if ($uri.Scheme -notin @('http', 'https') -or $uri.UserInfo -or $uri.Query -or $uri.Fragment) {
    throw 'Server URL must be http or https with no user info, query or fragment.'
}
$serverUrl = $uri.AbsoluteUri

$account = Read-Host 'User name (hidden)' -AsSecureString
$password = Read-Host 'Password (hidden)' -AsSecureString
if ($account.Length -eq 0 -or $password.Length -eq 0) { throw 'User name and password are required.' }

New-Item -ItemType Directory -Force -Path $dir | Out-Null
$user = "$env:USERDOMAIN\$env:USERNAME"
& icacls.exe $dir /inheritance:r /grant:r "${user}:(OI)(CI)F" 'SYSTEM:(OI)(CI)F' | Out-Null

$record = [pscustomobject]@{
    SchemaVersion = 1
    ServerUrl     = $serverUrl
    Account       = $account
    Password      = $password
}
$record | Export-Clixml -LiteralPath $path
$account.Dispose()
$password.Dispose()

Write-Host "Saved encrypted test credentials for $serverUrl"
Write-Host "Nothing was displayed. Run scripts\run_live_m1_20.ps1 to use them."
