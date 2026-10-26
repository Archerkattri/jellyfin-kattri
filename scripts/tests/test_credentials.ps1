$ErrorActionPreference = 'Stop'
. (Join-Path (Split-Path -Parent $PSScriptRoot) 'test_credentials.ps1')

function Assert-CredentialFailure([scriptblock] $Action) {
    $failed = $false
    try { & $Action | Out-Null }
    catch {
        $failed = $true
        if ($_.Exception.Message -match 'fixture-user|fixture-password|sentinel-account|sentinel-secret') {
            throw 'Credential failure exposed fixture data.'
        }
    }
    if (-not $failed) { throw 'Expected credential loading to fail.' }
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$fixtureDirectory = Join-Path $repoRoot ('.local\test-auth-check-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixtureDirectory | Out-Null
$fixturePath = Join-Path $fixtureDirectory 'fixture.clixml'
$invalidPath = Join-Path $fixtureDirectory 'invalid.clixml'
$oldUser = $env:JELLYFIN_TEST_USER
$oldPassword = $env:JELLYFIN_TEST_PASSWORD
$account = ConvertTo-SecureString 'fixture-user' -AsPlainText -Force
$password = ConvertTo-SecureString 'fixture-password' -AsPlainText -Force
$loaded = $null

try {
    Remove-Item Env:JELLYFIN_TEST_USER,Env:JELLYFIN_TEST_PASSWORD -ErrorAction SilentlyContinue
    if ($null -ne (Get-JellyfinTestCredential -CredentialPath $fixturePath)) {
        throw 'Missing credentials should return no credential.'
    }
    [pscustomobject]@{
        SchemaVersion = 1
        ServerUrl = 'http://127.0.0.1:8096/'
        Account = $account
        Password = $password
    } | Export-Clixml -LiteralPath $fixturePath -Encoding utf8

    $loaded = Get-JellyfinTestCredential -CredentialPath $fixturePath -ServerUrl 'http://127.0.0.1:8096'
    if ($loaded.UserName -cne 'fixture-user' -or $loaded.GetNetworkCredential().Password -cne 'fixture-password') {
        throw 'Encrypted file did not round trip.'
    }
    $loaded.Password.Dispose()
    $loaded = $null
    Assert-CredentialFailure { Get-JellyfinTestCredential -CredentialPath $fixturePath -ServerUrl 'http://127.0.0.2:8096/' }
    Assert-CredentialFailure { Get-JellyfinTestCredential -CredentialPath $fixturePath -ServerUrl 'http://127.0.0.1:8096/?token=sentinel-secret' }

    [pscustomobject]@{ SchemaVersion = 999; Account = 'sentinel-account'; Password = 'sentinel-secret' } |
        Export-Clixml -LiteralPath $invalidPath -Encoding utf8
    Assert-CredentialFailure { Get-JellyfinTestCredential -CredentialPath $invalidPath }

    $env:JELLYFIN_TEST_USER = 'fixture-user'
    Assert-CredentialFailure { Get-JellyfinTestCredential -CredentialPath $fixturePath }
    $env:JELLYFIN_TEST_PASSWORD = 'fixture-password'
    # Explicit environment credentials take precedence even when the file is invalid.
    $loaded = Get-JellyfinTestCredential -CredentialPath $invalidPath
    if ($loaded.UserName -cne 'fixture-user' -or $loaded.GetNetworkCredential().Password -cne 'fixture-password') {
        throw 'Process environment credentials were not used.'
    }
    Write-Output 'PASS: missing file, DPAPI round trip, server binding, credential-bearing target refusal, invalid file, partial environment refusal, environment precedence, sanitized errors.'
}
finally {
    if ($null -ne $loaded) { $loaded.Password.Dispose() }
    $account.Dispose()
    $password.Dispose()
    $env:JELLYFIN_TEST_USER = $oldUser
    $env:JELLYFIN_TEST_PASSWORD = $oldPassword
    $oldUser = $null
    $oldPassword = $null
    foreach ($path in @($fixturePath, $invalidPath)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
    Remove-Item -LiteralPath $fixtureDirectory
}
