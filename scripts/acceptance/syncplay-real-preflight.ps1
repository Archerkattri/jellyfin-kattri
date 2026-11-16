param()

$ErrorActionPreference = 'Stop'
$serverUrl = $env:JELLYFIN_TEST_URL
if ([string]::IsNullOrWhiteSpace($serverUrl)) {
    throw 'JELLYFIN_TEST_URL is not set; set it to the authorized Jellyfin server base URL before running this probe.'
}
$serverUrl = $serverUrl.TrimEnd('/')
$priorUrlPresent = Test-Path Env:JELLYFIN_TEST_URL
$priorUrl = $env:JELLYFIN_TEST_URL
$priorUserPresent = Test-Path Env:JELLYFIN_TEST_USER
$priorUser = $env:JELLYFIN_TEST_USER
$priorPasswordPresent = Test-Path Env:JELLYFIN_TEST_PASSWORD
$priorPassword = $env:JELLYFIN_TEST_PASSWORD
$credential = $null
$username = $null
$password = $null
$token = $null
$authResult = $null
$headers = $null
$body = $null

function Get-HttpStatusCode {
    param([System.Management.Automation.ErrorRecord]$ErrorRecord)

    try {
        return [int]$ErrorRecord.Exception.Response.StatusCode
    }
    catch {
        return $null
    }
}

try {
    # This probe intentionally uses the existing DPAPI credential for its saved target.
    Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue
    Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    $env:JELLYFIN_TEST_URL = $serverUrl
    . (Join-Path $PSScriptRoot '..\test_credentials.ps1')
    $credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
    if ($null -eq $credential) {
        throw 'Encrypted Jellyfin test credentials are unavailable; no authenticated request was made.'
    }

    $username = $credential.UserName
    $password = $credential.GetNetworkCredential().Password
    $baseUrl = $serverUrl.TrimEnd('/')
    $headers = @{
        'X-Emby-Authorization' = 'MediaBrowser Client="Kattri SyncPlay Acceptance", Device="Windows acceptance preflight", DeviceId="syncplay-preflight-readonly", Version="0.7.0"'
    }

    try {
        $publicInfo = Invoke-RestMethod -Method Get -Uri "$baseUrl/System/Info/Public" -Headers $headers -TimeoutSec 15
        Write-Output "PUBLIC_INFO=ok; server_version=$($publicInfo.Version)"
        $publicInfo = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "PUBLIC_INFO=failed; status=$status"
        exit 2
    }

    $body = @{ Username = $username; Pw = $password } | ConvertTo-Json -Compress
    try {
        $authResult = Invoke-RestMethod -Method Post -Uri "$baseUrl/Users/AuthenticateByName" `
            -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 20
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "AUTHENTICATION=failed; status=$status"
        exit 2
    }
    finally {
        $body = $null
        $password = $null
    }

    $token = [string]$authResult.AccessToken
    $userId = [string]$authResult.User.Id
    if ([string]::IsNullOrWhiteSpace($token) -or [string]::IsNullOrWhiteSpace($userId)) {
        Write-Output 'AUTHENTICATION=failed; response_incomplete=true'
        exit 2
    }

    $headers['X-Emby-Token'] = $token
    try {
        $user = Invoke-RestMethod -Method Get -Uri "$baseUrl/Users/Me" -Headers $headers -TimeoutSec 15
        $policy = $user.Policy
        $syncPlayAccess = if ($null -eq $policy.SyncPlayAccess) { 'missing' } else { [string]$policy.SyncPlayAccess }
        $enableSyncPlay = if ($null -eq $policy.PSObject.Properties['EnableSyncPlay']) { 'missing' } else { [string]$policy.EnableSyncPlay }
        Write-Output "AUTHENTICATION=ok; syncplay_access=$syncPlayAccess; enable_syncplay=$enableSyncPlay; is_administrator=$([bool]$policy.IsAdministrator)"
        $user = $null
        $policy = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "USER_POLICY=failed; status=$status"
        exit 2
    }

    try {
        $groups = Invoke-RestMethod -Method Get -Uri "$baseUrl/SyncPlay/List" -Headers $headers -TimeoutSec 15
        $groupCount = @($groups).Count
        Write-Output "SYNCPLAY_GROUP_LIST=ok; group_count=$groupCount"
        $groups = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "SYNCPLAY_GROUP_LIST=failed; status=$status"
    }

    $safeSampleTitles = @('Sintel', 'Tears of Steel', 'Elephants Dream')
    $eligibleSampleCount = 0
    $eligibleDurations = [System.Collections.Generic.List[long]]::new()
    foreach ($sampleTitle in $safeSampleTitles) {
        $searchTerm = [Uri]::EscapeDataString($sampleTitle)
        $itemsUri = "$baseUrl/Items?UserId=$([Uri]::EscapeDataString($userId))&Recursive=true&SearchTerm=$searchTerm&IncludeItemTypes=Movie&Fields=RunTimeTicks,UserData&Limit=30"
        try {
            $items = Invoke-RestMethod -Method Get -Uri $itemsUri -Headers $headers -TimeoutSec 20
            foreach ($item in @($items.Items)) {
                $runTimeTicks = [long]$item.RunTimeTicks
                $unplayed = $item.UserData.Played -eq $false
                $notFavorite = $item.UserData.IsFavorite -eq $false
                if ($item.Type -eq 'Movie' -and $item.Name -ieq $sampleTitle -and
                    $runTimeTicks -ge 7200000000 -and $unplayed -and $notFavorite) {
                    $eligibleSampleCount++
                    $eligibleDurations.Add([long][Math]::Floor($runTimeTicks / 10000000))
                }
            }
            $items = $null
        }
        catch {
            $status = Get-HttpStatusCode -ErrorRecord $_
            Write-Output "SAFE_SAMPLE_READ=failed; status=$status"
            break
        }
    }
    $maxEligibleDuration = if ($eligibleDurations.Count -gt 0) {
        ($eligibleDurations | Measure-Object -Maximum).Maximum
    }
    else { 0 }
    Write-Output "SAFE_SAMPLE_CANDIDATES=$eligibleSampleCount; minimum_duration_seconds=720; longest_candidate_seconds=$maxEligibleDuration"

    try {
        $folders = Invoke-RestMethod -Method Get -Uri "$baseUrl/Library/MediaFolders" -Headers $headers -TimeoutSec 20
        $testFolders = @($folders.Items | Where-Object { $_.Name -match '(?i)test|sample|acceptance' })
        $eligibleInTestFolders = 0
        $longestTestFolderDuration = 0L
        foreach ($folder in $testFolders) {
            for ($startIndex = 0; $startIndex -lt 1000; $startIndex += 100) {
                $folderId = [Uri]::EscapeDataString([string]$folder.Id)
                $itemsUri = "$baseUrl/Items?UserId=$([Uri]::EscapeDataString($userId))&ParentId=$folderId&Recursive=true&IncludeItemTypes=Movie&Fields=RunTimeTicks,UserData,OfficialRating&Limit=100&StartIndex=$startIndex"
                $items = Invoke-RestMethod -Method Get -Uri $itemsUri -Headers $headers -TimeoutSec 20
                foreach ($item in @($items.Items)) {
                    $runTimeTicks = [long]$item.RunTimeTicks
                    $unplayed = $item.UserData.Played -eq $false
                    $notFavorite = $item.UserData.IsFavorite -eq $false
                    $safeRating = [string]$item.OfficialRating -in @('G', 'PG', 'TV-G', 'TV-PG', 'TV-Y', 'TV-Y7')
                    if ($item.Type -eq 'Movie' -and $runTimeTicks -ge 7200000000 -and
                        $unplayed -and $notFavorite -and $safeRating) {
                        $eligibleInTestFolders++
                        $durationSeconds = [long][Math]::Floor($runTimeTicks / 10000000)
                        if ($durationSeconds -gt $longestTestFolderDuration) {
                            $longestTestFolderDuration = $durationSeconds
                        }
                    }
                }
                $totalCount = [int]$items.TotalRecordCount
                $items = $null
                if ($startIndex + 100 -ge $totalCount) { break }
            }
        }
        Write-Output "DEDICATED_TEST_SAMPLE_LIBRARIES=$($testFolders.Count); SAFE_UNPLAYED_MOVIES=$eligibleInTestFolders; longest_candidate_seconds=$longestTestFolderDuration"
        $folders = $null
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "DEDICATED_TEST_SAMPLE_READ=failed; status=$status"
    }

    try {
        $safeCatalogCount = 0
        $boundedMovieCount = 0
        $longestSafeDuration = 0L
        $shortestSafeDuration = [long]::MaxValue
        $catalogLimit = 5000
        for ($startIndex = 0; $startIndex -lt $catalogLimit; $startIndex += 100) {
            $itemsUri = "$baseUrl/Items?UserId=$([Uri]::EscapeDataString($userId))&Recursive=true&IncludeItemTypes=Movie&Fields=RunTimeTicks,UserData,OfficialRating&Limit=100&StartIndex=$startIndex"
            $items = Invoke-RestMethod -Method Get -Uri $itemsUri -Headers $headers -TimeoutSec 20
            foreach ($item in @($items.Items)) {
                $boundedMovieCount++
                $runTimeTicks = [long]$item.RunTimeTicks
                $durationSeconds = [long][Math]::Floor($runTimeTicks / 10000000)
                $unplayed = $item.UserData.Played -eq $false
                $notFavorite = $item.UserData.IsFavorite -eq $false
                $safeRating = [string]$item.OfficialRating -in @('G', 'PG', 'TV-G', 'TV-PG', 'TV-Y', 'TV-Y7')
                if ($runTimeTicks -ge 6600000000 -and $unplayed -and $notFavorite -and $safeRating) {
                    $safeCatalogCount++
                    if ($durationSeconds -lt $shortestSafeDuration) { $shortestSafeDuration = $durationSeconds }
                    if ($durationSeconds -gt $longestSafeDuration) { $longestSafeDuration = $durationSeconds }
                }
            }
            $totalCount = [int]$items.TotalRecordCount
            $items = $null
            if ($startIndex + 100 -ge $totalCount -or $startIndex + 100 -ge $catalogLimit) { break }
        }
        if ($safeCatalogCount -eq 0) { $shortestSafeDuration = 0 }
        Write-Output "SAFE_CATALOG_MOVIES_SCANNED=$boundedMovieCount; ELIGIBLE_UNPLAYED_NONFAVORITE_G_OR_PG_11MIN_PLUS=$safeCatalogCount; shortest_candidate_seconds=$shortestSafeDuration; longest_candidate_seconds=$longestSafeDuration"
    }
    catch {
        $status = Get-HttpStatusCode -ErrorRecord $_
        Write-Output "SAFE_CATALOG_READ=failed; status=$status"
    }
}
finally {
    if ($null -ne $credential) { $credential.Password.Dispose() }
    $credential = $null
    $username = $null
    $password = $null
    $token = $null
    $authResult = $null
    $headers = $null
    $body = $null
    if ($priorUrlPresent) { $env:JELLYFIN_TEST_URL = $priorUrl }
    else { Remove-Item Env:JELLYFIN_TEST_URL -ErrorAction SilentlyContinue }
    if ($priorUserPresent) { $env:JELLYFIN_TEST_USER = $priorUser }
    else { Remove-Item Env:JELLYFIN_TEST_USER -ErrorAction SilentlyContinue }
    if ($priorPasswordPresent) { $env:JELLYFIN_TEST_PASSWORD = $priorPassword }
    else { Remove-Item Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue }
}
