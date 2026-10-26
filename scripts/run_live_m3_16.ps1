param(
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
New-Item -ItemType Directory -Force -Path (Join-Path $repoRoot 'target\validation') | Out-Null
Set-Location -LiteralPath $repoRoot
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $repoRoot 'target\validation\m3-16-live-output.txt'
}
elseif (-not [System.IO.Path]::IsPathRooted($OutputPath)) {
    $OutputPath = Join-Path $repoRoot $OutputPath
}
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $OutputPath) {
    throw "Refusing to overwrite existing validation output: $OutputPath"
}
$outputDirectory = Split-Path -Parent $OutputPath
if (-not (Test-Path -LiteralPath $outputDirectory -PathType Container)) {
    throw "Validation output directory does not exist: $outputDirectory"
}

$hadUser = Test-Path Env:JELLYFIN_TEST_USER
$savedUser = $env:JELLYFIN_TEST_USER
$hadPassword = Test-Path Env:JELLYFIN_TEST_PASSWORD
$savedPassword = $env:JELLYFIN_TEST_PASSWORD
$credential = $null
$username = $null
$password = $null
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

    Write-Host 'Running read-only M3-16 box-set member validation. No server state changes are requested.'
    $savedPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $output = @(& cargo test --quiet -p jellyfin-api --test live_readonly readonly_collection_members_preserve_server_order -- --ignored --nocapture 2>&1)
    $testExit = $LASTEXITCODE
    $ErrorActionPreference = $savedPreference
    $safeLines = foreach ($line in $output) {
        $text = [string]$line
        if (-not [string]::IsNullOrEmpty($username)) { $text = $text.Replace($username, '<redacted-user>') }
        if (-not [string]::IsNullOrEmpty($password)) { $text = $text.Replace($password, '<redacted-secret>') }
        $text = [regex]::Replace($text, '(?i)(token|password|username|api_key)=\S+', '$1=<redacted>')
        [regex]::Replace($text, 'https?://\S+', '<redacted-url>')
    }

    $stream = [System.IO.File]::Open($OutputPath, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try {
        $writer = [System.IO.StreamWriter]::new($stream, [System.Text.UTF8Encoding]::new($false))
        foreach ($line in $safeLines) { $writer.WriteLine($line) }
        $writer.Flush()
    }
    finally {
        if ($null -ne $writer) { $writer.Dispose() }
        else { $stream.Dispose() }
    }
    foreach ($line in $safeLines) { Write-Output $line }
    Write-Host "Sanitized output saved to $OutputPath; exit code $testExit."
}
finally {
    if ($hadUser) { $env:JELLYFIN_TEST_USER = $savedUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($hadPassword) { $env:JELLYFIN_TEST_PASSWORD = $savedPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    $password = $null
    $username = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
}

exit $testExit
