$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
New-Item -ItemType Directory -Force -Path (Join-Path $repoRoot 'target\validation') | Out-Null
Set-Location -LiteralPath $repoRoot

. (Join-Path $PSScriptRoot 'test_credentials.ps1')
$credential = Get-JellyfinTestCredential
if ($null -eq $credential) {
    throw 'Test credentials are unavailable. Provide process environment credentials or the encrypted local credential file; no request was made.'
}
$username = $credential.UserName
$password = $credential.GetNetworkCredential().Password

$env:JELLYFIN_TEST_USER = $username
$env:JELLYFIN_TEST_PASSWORD = $password
$outputPath = Join-Path $repoRoot 'target\validation\m1-07-09-live-output.txt'
$testExit = 1

try {
    Write-Host 'Running read-only M1-07..09 checks. No playback or server state changes are requested.'
    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $output = @(& cargo test --quiet -p jellyfin-api --test live_readonly readonly_item_series_people -- --ignored --nocapture 2>&1)
    $testExit = $LASTEXITCODE
    $ErrorActionPreference = $savedErrorActionPreference
    $safeOutput = (($output | ForEach-Object { [string]$_ }) -join [Environment]::NewLine).TrimEnd([char[]]"`r`n")
    if (-not [string]::IsNullOrEmpty($username)) {
        $safeOutput = $safeOutput.Replace($username, '<redacted-user>')
    }
    if (-not [string]::IsNullOrEmpty($password)) {
        $safeOutput = $safeOutput.Replace($password, '<redacted-secret>')
    }
    $safeOutput = [regex]::Replace(
        $safeOutput,
        '(?i)(token|password|username|api_key)=\S+',
        '$1=<redacted>'
    )
    Set-Content -LiteralPath $outputPath -Value $safeOutput -Encoding utf8
    Write-Output $safeOutput
    Write-Host "Sanitized output saved to target/validation/m1-07-09-live-output.txt; exit code $testExit."
}
finally {
    Remove-Item Env:JELLYFIN_TEST_USER, Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    $password = $null
    $username = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
}

exit $testExit
