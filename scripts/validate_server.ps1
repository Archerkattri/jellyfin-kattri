param(
    [string] $ServerUrl = $(if ($env:JELLYFIN_TEST_URL) { $env:JELLYFIN_TEST_URL } else { 'http://127.0.0.1:8096/' }),
    [int] $SampleLimit = 12,
    [string] $DeviceProfilePath = $(if ($env:JELLYFIN_TEST_DEVICE_PROFILE) { $env:JELLYFIN_TEST_DEVICE_PROFILE } else { $null })
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'test_credentials.ps1')
$credential = $null

function Get-SafeServerUri([string] $Value) {
    $parsed = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref] $parsed)) {
        throw 'Server URL must be an absolute HTTP(S) URL.'
    }
    if ($parsed.Scheme -notin @('http', 'https') -or $parsed.UserInfo -or $parsed.Query -or $parsed.Fragment) {
        throw 'Server URL must not contain credentials, a query, or a fragment.'
    }
    if ($parsed.Scheme -eq 'http') {
        $hostName = $parsed.Host.TrimStart([char]'[').TrimEnd([char]']')
        $isLoopbackName = $hostName -eq 'localhost' -or $hostName.EndsWith('.localhost', [StringComparison]::OrdinalIgnoreCase)
        $address = $null
        $isPrivateAddress = $false
        if ([System.Net.IPAddress]::TryParse($hostName, [ref] $address)) {
            if ([System.Net.IPAddress]::IsLoopback($address) -or $address.IsIPv6LinkLocal) {
                $isPrivateAddress = $true
            }
            elseif ($address.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork) {
                $octets = $address.GetAddressBytes()
                $isPrivateAddress = $octets[0] -eq 10 -or
                    ($octets[0] -eq 172 -and $octets[1] -ge 16 -and $octets[1] -le 31) -or
                    ($octets[0] -eq 192 -and $octets[1] -eq 168)
            }
            elseif ($address.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetworkV6) {
                $octets = $address.GetAddressBytes()
                $isPrivateAddress = ($octets[0] -band 0xFE) -eq 0xFC
            }
        }
        if (-not ($isLoopbackName -or $isPrivateAddress)) {
            throw 'Plain HTTP is limited to loopback or private/link-local IP addresses.'
        }
    }
    return $parsed
}

function Invoke-SafeJson([string] $Uri, [string] $Method = 'Get', [hashtable] $Headers = @{}, $Body = $null) {
    try {
        $params = @{ Uri = $Uri; Method = $Method; Headers = $Headers; TimeoutSec = 30; MaximumRedirection = 0 }
        if ($null -ne $Body) {
            $params.ContentType = 'application/json'
            $params.Body = ConvertTo-Json -InputObject $Body -Depth 50 -Compress
        }
        return Invoke-RestMethod @params
    }
    catch {
        throw 'A Jellyfin request failed. Response details and URLs were suppressed to protect credentials and library metadata.'
    }
}

function Get-ApiUri([Uri] $Base, [string] $RelativePath) {
    return [Uri]::new($Base, $RelativePath).AbsoluteUri
}

function Get-SafeText($Value, [string] $Fallback = 'unknown') {
    if ($null -eq $Value) { return $Fallback }
    $text = [string] $Value
    if ($text -match '[\r\n\x00-\x1f]') { return $Fallback }
    return $text
}

try {
    if ($SampleLimit -lt 1 -or $SampleLimit -gt 50) { throw 'SampleLimit must be between 1 and 50.' }
    $base = Get-SafeServerUri $ServerUrl
    $base = [Uri]::new($base.AbsoluteUri.TrimEnd('/') + '/')

    # Always exercise the public endpoint without credentials.
    $publicInfo = Invoke-SafeJson (Get-ApiUri $base 'System/Info/Public')
    $version = Get-SafeText $publicInfo.Version
    $product = Get-SafeText $publicInfo.ProductName 'Jellyfin Server'
    Write-Output "Anonymous public-info request: OK ($product $version)."

    $credential = Get-JellyfinTestCredential -ServerUrl $base.AbsoluteUri
    if ($null -eq $credential) {
        Write-Output 'Authenticated checks skipped. Provide process environment credentials or the encrypted local credential file for read-only library and PlaybackInfo checks.'
        exit 0
    }

    $auth = Invoke-SafeJson (Get-ApiUri $base 'Users/AuthenticateByName') 'Post' @{} @{
        Username = $credential.UserName
        Pw = $credential.GetNetworkCredential().Password
    }
    $token = [string] $auth.AccessToken
    $userId = [string] $auth.User.Id
    if ([string]::IsNullOrWhiteSpace($token) -or [string]::IsNullOrWhiteSpace($userId)) {
        throw 'Authentication response was incomplete; details were suppressed.'
    }
    $authHeader = @{ Authorization = "MediaBrowser Client=`"Jellyfin Kattri validator`", Device=`"validation`", DeviceId=`"kattri-validation`", Version=`"1`", Token=`"$token`"" }
    $token = $null

    # Library names, user identity, item names, IDs, and raw URLs are never output.
    $views = Invoke-SafeJson (Get-ApiUri $base ("Users/$([Uri]::EscapeDataString($userId))/Views")) 'Get' $authHeader
    $viewCount = @($views.Items).Count
    $query = "Items?userId=$([Uri]::EscapeDataString($userId))&recursive=true&startIndex=0&limit=$SampleLimit&Fields=MediaSources"
    $items = Invoke-SafeJson (Get-ApiUri $base $query) 'Get' $authHeader
    $sample = @($items.Items | Select-Object -First $SampleLimit)

    $profile = $null
    if ($DeviceProfilePath) {
        if (-not (Test-Path -LiteralPath $DeviceProfilePath -PathType Leaf)) { throw 'The requested device-profile file was not found.' }
        $profileJson = Get-Content -LiteralPath $DeviceProfilePath -Raw | ConvertFrom-Json
        if ($profileJson.deviceProfile) { $profile = $profileJson.deviceProfile }
        elseif ($profileJson.DeviceProfile) { $profile = $profileJson.DeviceProfile }
        elseif ($profileJson.DirectPlayProfiles -or $profileJson.TranscodingProfiles) { $profile = $profileJson }
        else { throw 'The device-profile file must contain a Jellyfin DeviceProfile object; details were suppressed.' }
    }

    $records = [System.Collections.Generic.List[object]]::new()
    foreach ($item in $sample) {
        $itemId = [string] $item.Id
        if ([string]::IsNullOrWhiteSpace($itemId)) { continue }
        $requestBody = @{ EnableDirectPlay = $true; EnableDirectStream = $true; EnableTranscoding = $true }
        if ($profile) { $requestBody.DeviceProfile = $profile }
        $playback = Invoke-SafeJson (Get-ApiUri $base "Items/$([Uri]::EscapeDataString($itemId))/PlaybackInfo") 'Post' $authHeader $requestBody
        foreach ($source in @($playback.MediaSources)) {
            $streams = @($source.MediaStreams)
            $videoCodec = ($streams | Where-Object { $_.Type -eq 'Video' } | Select-Object -First 1).Codec
            $audioCodec = ($streams | Where-Object { $_.Type -eq 'Audio' } | Select-Object -First 1).Codec
            $safeContainer = Get-SafeText $source.Container
            $safeVideo = Get-SafeText $videoCodec
            $safeAudio = Get-SafeText $audioCodec
            # Inspect potentially sensitive values in memory only; never emit them.
            $sensitiveValues = @($source.Path, $source.TranscodingUrl, $source.OpenToken)
            $hasCredentialUrl = $false
            foreach ($value in $sensitiveValues) {
                if ($null -ne $value -and ([string]$value -match '(?i)(api_key|token|password|username)=')) { $hasCredentialUrl = $true }
            }
            if ($hasCredentialUrl) { throw 'PlaybackInfo returned a credential-bearing source URL; output suppressed.' }
            $records.Add([pscustomobject]@{
                Container = $safeContainer
                VideoCodec = $safeVideo
                AudioCodec = $safeAudio
                DirectPlay = [bool] $source.SupportsDirectPlay
                DirectStream = [bool] $source.SupportsDirectStream
                Transcode = [bool] $source.SupportsTranscoding
                RequiresHeaders = (@($source.RequiredHttpHeaders.PSObject.Properties).Count -gt 0)
                HasTranscodeUrl = -not [string]::IsNullOrWhiteSpace([string] $source.TranscodingUrl)
            })
        }
    }

    $groups = @($records | Group-Object Container, VideoCodec, AudioCodec | ForEach-Object {
        $parts = $_.Name -split ', '
        [pscustomobject]@{ Container = $parts[0]; VideoCodec = $parts[1]; AudioCodec = $parts[2]; Count = $_.Count }
    })
    $offers = [pscustomobject]@{
        DirectPlay = @($records | Where-Object DirectPlay).Count
        DirectStream = @($records | Where-Object DirectStream).Count
        Transcode = @($records | Where-Object Transcode).Count
        SourcesRequiringHeaders = @($records | Where-Object RequiresHeaders).Count
        SourcesOfferingTranscodeUrl = @($records | Where-Object HasTranscodeUrl).Count
    }
    $report = [pscustomobject]@{
        Server = $base.GetLeftPart([UriPartial]::Authority) + $base.AbsolutePath.TrimEnd('/')
        ServerVersion = $version
        Libraries = $viewCount
        SampledItems = $sample.Count
        NegotiatedSources = $records.Count
        ProfileSupplied = [bool] $profile
        CapabilityOffers = $offers
        CodecContainerSummary = $groups
    }
    Write-Output ($report | ConvertTo-Json -Depth 8)
}
catch {
    Write-Error $_.Exception.Message
    exit 1
}
finally {
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
    $token = $null
    $authHeader = $null
    $auth = $null
    $profileJson = $null
}
