# Local test authentication only. SecureString fields in CLIXML use Windows DPAPI.
function Get-JellyfinTestCredential {
    [OutputType([System.Management.Automation.PSCredential])]
    param(
        [string] $CredentialPath = (Join-Path (Split-Path -Parent $PSScriptRoot) '.local\test-auth\jellyfin-test.clixml'),
        [string] $ServerUrl = $(if ($env:JELLYFIN_TEST_URL) { $env:JELLYFIN_TEST_URL } else { 'http://127.0.0.1:8096/' })
    )

    $hasUser = -not [string]::IsNullOrWhiteSpace($env:JELLYFIN_TEST_USER)
    $hasPassword = -not [string]::IsNullOrWhiteSpace($env:JELLYFIN_TEST_PASSWORD)
    if ($hasUser -and $hasPassword) {
        $securePassword = ConvertTo-SecureString $env:JELLYFIN_TEST_PASSWORD -AsPlainText -Force
        return [System.Management.Automation.PSCredential]::new($env:JELLYFIN_TEST_USER, $securePassword)
    }
    if ($hasUser -or $hasPassword) {
        throw 'Provide both Jellyfin test environment credentials together, or leave both unset to use the encrypted local file.'
    }
    if (-not (Test-Path -LiteralPath $CredentialPath -PathType Leaf)) { return $null }
    if ($env:OS -ne 'Windows_NT') {
        throw 'The local encrypted test credentials require the Windows account that created them.'
    }

    $record = $null
    $account = $null
    try {
        if ((Get-Item -LiteralPath $CredentialPath).Length -gt 32768) { throw 'Invalid credential file.' }
        $record = Import-Clixml -LiteralPath $CredentialPath
        if ($record.SchemaVersion -ne 1 -or
            $record.Account -isnot [System.Security.SecureString] -or
            $record.Password -isnot [System.Security.SecureString] -or
            $record.Account.Length -eq 0 -or $record.Password.Length -eq 0) {
            throw 'Invalid credential file.'
        }
        $target = [Uri]::new($ServerUrl, [UriKind]::Absolute)
        $savedTarget = [Uri]::new([string] $record.ServerUrl, [UriKind]::Absolute)
        if ($target.Scheme -notin @('http', 'https') -or $target.UserInfo -or $target.Query -or
            $target.Fragment -or -not [string]::Equals(
                $target.AbsoluteUri.TrimEnd('/'), $savedTarget.AbsoluteUri.TrimEnd('/'),
                [StringComparison]::Ordinal)) {
            throw 'Invalid credential target.'
        }
        $account = [System.Net.NetworkCredential]::new('', $record.Account).Password
        return [System.Management.Automation.PSCredential]::new($account, $record.Password.Copy())
    }
    catch {
        throw 'Local test credentials could not be loaded for this server and Windows account; credential details suppressed.'
    }
    finally {
        $account = $null
        if ($null -ne $record) {
            if ($record.Account -is [System.Security.SecureString]) { $record.Account.Dispose() }
            if ($record.Password -is [System.Security.SecureString]) { $record.Password.Dispose() }
        }
    }
}
