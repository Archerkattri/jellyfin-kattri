param(
    [string] $OutputPath
)

$initialErrorActionPreference = $ErrorActionPreference
$returnResultToCaller = $MyInvocation.InvocationName -eq '.'
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    New-Item -ItemType Directory -Force -Path (Join-Path $repoRoot 'target\validation') | Out-Null
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    $outputPath = Join-Path $repoRoot "target\validation\m1-01-07-live-output-$stamp.txt"
}
else {
    $candidatePath = if ([System.IO.Path]::IsPathRooted($OutputPath)) {
        $OutputPath
    }
    else {
        Join-Path $repoRoot $OutputPath
    }
    $outputPath = [System.IO.Path]::GetFullPath($candidatePath)
}
if (Test-Path -LiteralPath $outputPath) {
    throw "Refusing to overwrite existing validation output: $outputPath"
}
$outputDirectory = Split-Path -Parent $outputPath
if (-not (Test-Path -LiteralPath $outputDirectory -PathType Container)) {
    throw "Validation output directory does not exist: $outputDirectory"
}

$hadTestUser = Test-Path Env:JELLYFIN_TEST_USER
$savedTestUser = $env:JELLYFIN_TEST_USER
$hadTestPassword = Test-Path Env:JELLYFIN_TEST_PASSWORD
$savedTestPassword = $env:JELLYFIN_TEST_PASSWORD
$credential = $null
$username = $null
$password = $null
$savedPreference = $ErrorActionPreference
$allLines = @()
$testExit = 1

try {
    . (Join-Path $PSScriptRoot 'test_credentials.ps1')
    $credential = Get-JellyfinTestCredential
    if ($null -eq $credential) {
        throw 'Test credentials are unavailable. Provide process environment credentials or the encrypted local credential file; no request was made.'
    }
    $username = $credential.UserName
    $password = $credential.GetNetworkCredential().Password
    $env:JELLYFIN_TEST_USER = $username
    $env:JELLYFIN_TEST_PASSWORD = $password

    Write-Host 'Running the selected authenticated read-only M1-01..07 tests. No playback or server state changes are requested.'
    $ErrorActionPreference = 'Continue'
    $output = @(& cargo test --quiet -p jellyfin-api --test live_readonly readonly_home_and_playback_negotiation -- --ignored --nocapture 2>&1)
    $testExit = $LASTEXITCODE
    $ErrorActionPreference = $savedPreference
    foreach ($line in $output) {
        $text = [string]$line
        if (-not [string]::IsNullOrEmpty($username)) { $text = $text.Replace($username, '<redacted-user>') }
        if (-not [string]::IsNullOrEmpty($password)) { $text = $text.Replace($password, '<redacted-secret>') }
        $text = [regex]::Replace($text, '(?i)(token|password|username|api_key)=\S+', '$1=<redacted>')
        $text = [regex]::Replace($text, 'https?://\S+', '<redacted-url>')
        $allLines += $text
    }

    $fileStream = [System.IO.File]::Open(
        $outputPath,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $writer = [System.IO.StreamWriter]::new($fileStream, [System.Text.UTF8Encoding]::new($false))
        foreach ($line in $allLines) { $writer.WriteLine($line) }
        $writer.Flush()
    }
    finally {
        if ($null -ne $writer) { $writer.Dispose() }
        else { $fileStream.Dispose() }
    }
    foreach ($line in $allLines) { Write-Output $line }
    Write-Host "Sanitized output saved to $outputPath; exit code $testExit."
}
finally {
    if ($hadTestUser) { $env:JELLYFIN_TEST_USER = $savedTestUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($hadTestPassword) { $env:JELLYFIN_TEST_PASSWORD = $savedTestPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    $password = $null
    $username = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
    $ErrorActionPreference = $initialErrorActionPreference
}

if ($returnResultToCaller) { return $testExit }
exit $testExit
