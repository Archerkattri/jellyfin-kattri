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
    $outputPath = Join-Path $repoRoot "target\validation\m1-20-live-output-$stamp.txt"
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
$pushedLocation = $false
$writer = $null
$cases = @(
    [pscustomobject]@{ Name = 'M1-01..07 live read-only'; Args = @('test', '--quiet', '-p', 'jellyfin-api', '--test', 'live_readonly', 'readonly_home_and_playback_negotiation', '--', '--ignored', '--nocapture') },
    [pscustomobject]@{ Name = 'M1-07..09 live read-only'; Args = @('test', '--quiet', '-p', 'jellyfin-api', '--test', 'live_readonly', 'readonly_item_series_people', '--', '--ignored', '--nocapture') },
    [pscustomobject]@{ Name = 'M1-10..11 live read-only'; Args = @('test', '--quiet', '-p', 'jellyfin-egui-client', '--test', 'live_m1_10_11', '--', '--ignored', '--nocapture') }
)
$allLines = [System.Collections.Generic.List[string]]::new()
$overallExit = 0

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

    Push-Location -LiteralPath $repoRoot
    $pushedLocation = $true
    Write-Host 'Running selected ignored read-only M1 live suites. They perform public info, sign-in, GET reads, and PlaybackInfo only; they do not start playback or report sessions.'
    foreach ($case in $cases) {
        $label = [string]$case.Name
        $cargoArgs = [string[]]$case.Args
        $savedPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        $lines = @(& cargo @cargoArgs 2>&1)
        $exitCode = $LASTEXITCODE
        $ErrorActionPreference = $savedPreference

        $allLines.Add("=== $label (exit $exitCode) ===")
        foreach ($line in $lines) {
            $text = [string]$line
            if (-not [string]::IsNullOrEmpty($username)) {
                $text = $text.Replace($username, '<redacted-user>')
            }
            if (-not [string]::IsNullOrEmpty($password)) {
                $text = $text.Replace($password, '<redacted-secret>')
            }
            $text = [regex]::Replace($text, '(?i)(token|password|username|api_key)=\S+', '$1=<redacted>')
            $text = [regex]::Replace($text, 'https?://\S+', '<redacted-url>')
            $allLines.Add($text)
        }
        if ($exitCode -ne 0) { $overallExit = $exitCode }
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
    Write-Host "Sanitized output saved to $outputPath; exit code $overallExit."
}
finally {
    if ($pushedLocation) { Pop-Location }
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

if ($returnResultToCaller) { return $overallExit }
exit $overallExit
