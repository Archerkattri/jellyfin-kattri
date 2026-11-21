# Read-only probe for an existing Live TV lineup. It never opens a stream or changes server state.
# It reports tuner/channel/guide availability only, so an owner-run tune acceptance can start from
# a server that actually has a configured tuner and guide source.

[CmdletBinding()]
param(
    [string] $OutputPath
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
# Report the workspace version rather than a literal, so the probe cannot claim a stale client version.
$versionLine = Select-String -Path (Join-Path $repoRoot 'Cargo.toml') -Pattern '^version = "([^"]+)"' |
    Select-Object -First 1
$clientVersion = if ($versionLine) { $versionLine.Matches[0].Groups[1].Value } else { '0.0.0' }
$serverUrl = $env:JELLYFIN_TEST_URL
if ([string]::IsNullOrWhiteSpace($serverUrl)) {
    throw 'JELLYFIN_TEST_URL is not set; set it to the authorized Jellyfin server base URL before running this probe.'
}
$serverUrl = $serverUrl.TrimEnd('/')
$evidenceRoot = Join-Path $repoRoot '.local\parallel-20261115\live-tv'
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff')
    $outputPath = Join-Path $evidenceRoot "readonly-probe-$stamp.txt"
}
else {
    $candidatePath = if ([System.IO.Path]::IsPathRooted($OutputPath)) {
        $OutputPath
    }
    else {
        Join-Path $repoRoot $OutputPath
    }
    $outputPath = [System.IO.Path]::GetFullPath($candidatePath)
    $evidenceRoot = Split-Path -Parent $outputPath
    New-Item -ItemType Directory -Path $evidenceRoot -Force | Out-Null
}
if (Test-Path -LiteralPath $outputPath) {
    throw 'Refusing to overwrite an existing Live TV probe report.'
}

$hadUrl = Test-Path Env:JELLYFIN_TEST_URL
$priorUrl = $env:JELLYFIN_TEST_URL
$hadUser = Test-Path Env:JELLYFIN_TEST_USER
$priorUser = $env:JELLYFIN_TEST_USER
$hadPassword = Test-Path Env:JELLYFIN_TEST_PASSWORD
$priorPassword = $env:JELLYFIN_TEST_PASSWORD
$credential = $null
$username = $null
$password = $null
$token = $null
$lines = [System.Collections.Generic.List[string]]::new()
$exitCode = 1

function Get-HttpStatusCode {
    param([System.Management.Automation.ErrorRecord] $ErrorRecord)

    try {
        return [int]$ErrorRecord.Exception.Response.StatusCode
    }
    catch {
        return $null
    }
}

function Protect-LiveTvText([AllowNull()][string] $Text) {
    if ($null -eq $Text) { return '' }
    $safeText = $Text
    foreach ($secret in @($username, $password, $token)) {
        if (-not [string]::IsNullOrEmpty([string] $secret)) {
            $safeText = $safeText.Replace([string] $secret, '[redacted]')
        }
    }
    $safeText = [regex]::Replace($safeText, '(?i)\b(api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|token|password|username)(\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)', '$1$2[redacted]')
    [regex]::Replace($safeText, 'https?://\S+', '[server URL redacted]')
}

try {
    # The encrypted credential file is target-bound; set the authorized URL before loading it.
    $env:JELLYFIN_TEST_URL = $serverUrl
    Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue
    Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    . (Join-Path $PSScriptRoot '..\test_credentials.ps1')
    $credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
    if ($null -eq $credential) {
        throw 'The existing encrypted read-only test credential is unavailable; no request was made.'
    }
    $username = $credential.UserName
    $password = $credential.GetNetworkCredential().Password

    $headers = @{
        'X-Emby-Authorization' = 'MediaBrowser Client="Kattri Live TV Acceptance", Device="Windows acceptance preflight", DeviceId="live-tv-preflight-readonly", Version="$clientVersion"'
    }
    try {
        $publicInfo = Invoke-RestMethod -Method Get -Uri "$serverUrl/System/Info/Public" -Headers $headers -TimeoutSec 15
        $lines.Add("PUBLIC_INFO=ok; server_version=$($publicInfo.Version)")
        $publicInfo = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("PUBLIC_INFO=failed; status=$status")
        throw 'The server is not reachable for a read-only Live TV probe.'
    }

    $authBody = @{ Username = $username; Pw = $password } | ConvertTo-Json -Compress
    try {
        $authResult = Invoke-RestMethod -Method Post -Uri "$serverUrl/Users/AuthenticateByName" `
            -Headers $headers -ContentType 'application/json' -Body $authBody -TimeoutSec 20
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("AUTHENTICATION=failed; status=$status")
        throw 'Authentication failed for the read-only Live TV probe.'
    }
    finally {
        $authBody = $null
        $password = $null
    }
    $token = [string]$authResult.AccessToken
    $userId = [string]$authResult.User.Id
    if ([string]::IsNullOrWhiteSpace($token) -or [string]::IsNullOrWhiteSpace($userId)) {
        $lines.Add('AUTHENTICATION=failed; response_incomplete=true')
        throw 'The authentication response was incomplete.'
    }
    $headers['X-Emby-Token'] = $token
    $lines.Add('AUTHENTICATION=ok')
    $authResult = $null

    try {
        $info = Invoke-RestMethod -Method Get -Uri "$serverUrl/LiveTv/Info" -Headers $headers -TimeoutSec 20
        $tunerCount = @($info.TunerHosts).Count
        $listingProviders = @($info.ListingProviders).Count
        $enabled = if ($null -eq $info.PSObject.Properties['IsEnabled']) { 'missing' } else { [string]$info.IsEnabled }
        $lines.Add("LIVE_TV_INFO=ok; enabled=$enabled; tuner_hosts=$tunerCount; listing_providers=$listingProviders")
        $info = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("LIVE_TV_INFO=failed; status=$status")
    }

    try {
        $channels = Invoke-RestMethod -Method Get -Uri "$serverUrl/LiveTv/Channels?Limit=25" -Headers $headers -TimeoutSec 20
        $channelCount = if ($null -eq $channels.TotalRecordCount) { @($channels.Items).Count } else { [int]$channels.TotalRecordCount }
        $lines.Add("CHANNELS=ok; total=$channelCount; sampled=$(@($channels.Items).Count)")
        $channels = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("CHANNELS=failed; status=$status")
    }

    try {
        $items = Invoke-RestMethod -Method Get -Uri "$serverUrl/Items?UserId=$([Uri]::EscapeDataString($userId))&Recursive=true&IncludeItemTypes=LiveTvChannel&Limit=25" -Headers $headers -TimeoutSec 20
        $itemCount = if ($null -eq $items.TotalRecordCount) { @($items.Items).Count } else { [int]$items.TotalRecordCount }
        $lines.Add("CHANNEL_ITEMS=ok; total=$itemCount")
        $items = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("CHANNEL_ITEMS=failed; status=$status")
    }

    try {
        $programs = Invoke-RestMethod -Method Get -Uri "$serverUrl/LiveTv/Programs?Limit=25" -Headers $headers -TimeoutSec 20
        $programCount = if ($null -eq $programs.TotalRecordCount) { @($programs.Items).Count } else { [int]$programs.TotalRecordCount }
        $lines.Add("GUIDE_PROGRAMS=ok; total=$programCount; sampled=$(@($programs.Items).Count)")
        $programs = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        $lines.Add("GUIDE_PROGRAMS=failed(fallback Items); status=$status")
        try {
            $items = Invoke-RestMethod -Method Get -Uri "$serverUrl/Items?UserId=$([Uri]::EscapeDataString($userId))&Recursive=true&IncludeItemTypes=LiveTvProgram&Limit=25" -Headers $headers -TimeoutSec 20
            $itemCount = if ($null -eq $items.TotalRecordCount) { @($items.Items).Count } else { [int]$items.TotalRecordCount }
            $lines.Add("GUIDE_PROGRAM_ITEMS=ok; total=$itemCount")
            $items = $null
        }
        catch {
            $fallback = Get-HttpStatusCode -ErrorRecord $_
            $lines.Add("GUIDE_PROGRAM_ITEMS=failed; status=$fallback")
        }
    }

    $exitCode = 0
}
catch {
    $lines.Add((Protect-LiveTvText "Live TV preflight could not run: $($_.Exception.Message)"))
    $exitCode = 1
}
finally {
    if ($hadUrl) { $env:JELLYFIN_TEST_URL = $priorUrl }
    else { Remove-Item Env:JELLYFIN_TEST_URL -ErrorAction SilentlyContinue }
    if ($hadUser) { $env:JELLYFIN_TEST_USER = $priorUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($hadPassword) { $env:JELLYFIN_TEST_PASSWORD = $priorPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
    if ($null -ne $credential -and $null -ne $credential.Password) { $credential.Password.Dispose() }
    $password = $null
    $username = $null
    $token = $null
    $headers = $null
    $credential = $null
}

if ($lines.Count -eq 0) { $lines.Add('No probe output was captured.') }
[System.IO.File]::WriteAllLines($outputPath, $lines, [System.Text.UTF8Encoding]::new($false))
foreach ($line in $lines) { Write-Output $line }
Write-Output "Sanitized read-only output saved to $outputPath; exit code $exitCode."
exit $exitCode
