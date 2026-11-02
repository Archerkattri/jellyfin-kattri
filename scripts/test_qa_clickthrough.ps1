$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'qa_clickthrough.ps1') -DefineOnly

$script:testCount = 0
$script:testFailures = [System.Collections.Generic.List[string]]::new()
$script:uiNames = @()
$script:mockSelected = $false
$script:mockNotes = [System.Collections.Generic.List[string]]::new()
$script:failures = [System.Collections.Generic.List[string]]::new()
$script:report = [System.Collections.Generic.List[string]]::new()
$script:lastShotHash = $null
$script:credentialReads = 0
$script:credentialServerUrl = $null

$priorJellyfinTestUser = $env:JELLYFIN_TEST_USER
$priorJellyfinTestPassword = $env:JELLYFIN_TEST_PASSWORD
$env:JELLYFIN_TEST_USER = $null
$env:JELLYFIN_TEST_PASSWORD = $null

function Assert-Test([bool] $Condition, [string] $Message) {
    $script:testCount++
    if (-not $Condition) { $script:testFailures.Add($Message) }
}

function Assert-Equal($Expected, $Actual, [string] $Message) {
    Assert-Test ([object]::Equals($Expected, $Actual)) "$Message (expected '$Expected', got '$Actual')"
}

function Assert-Throws([scriptblock] $Action, [string] $Message) {
    $threw = $false
    try { & $Action } catch { $threw = $true }
    Assert-Test $threw $Message
}

function Note([string] $Text) { $script:mockNotes.Add($Text) }
function Fail([string] $Text) {
    $script:failures.Add($Text)
    $script:report.Add("FAIL: $Text")
}
function Find-UiElement([string] $Name, [switch] $Prefix) {
    foreach ($candidate in $script:uiNames) {
        if (Test-UiNameMatch $candidate $Name -Prefix:$Prefix) {
            return [pscustomobject]@{ Current = [pscustomobject]@{ Name = $candidate } }
        }
    }
    return $null
}
function Get-ElementSelected($Element) { return [bool] $script:mockSelected }
function Get-JellyfinTestCredential([string] $ServerUrl) {
    $script:credentialReads++
    $script:credentialServerUrl = $ServerUrl
    [pscustomobject]@{ UserName = 'offline-user'; Password = 'offline-password' }
}

function Reset-AssertionState([string[]] $Names) {
    $script:uiNames = $Names
    $script:failures.Clear()
    $script:report.Clear()
    $script:mockNotes.Clear()
    $script:mockSelected = $false
}

# Public default stays on loopback. A private server requires explicit configuration.
Assert-Equal 'http://127.0.0.1:8096/' (Resolve-QaServerUrl $null) 'unset URL defaults to loopback'
Assert-Equal 'http://localhost:8096/' (Resolve-QaServerUrl 'http://localhost:8096/') 'explicit loopback URL is accepted'
Assert-Equal 'http://192.168.56.23:8096/' `
    (Resolve-QaServerUrl 'http://192.168.56.23:8096/') 'explicit private server URL is accepted'
Assert-Equal 'https://192.168.56.23:8096/' `
    (Resolve-QaServerUrl 'https://192.168.56.23:8096/') 'explicit HTTPS private server URL is accepted'
Assert-Throws { Get-AuthorizedServerUrl 'http://192.168.56.23:8096/' } `
    'nonloopback URL without explicit configuration is rejected'
foreach ($url in @(
    'ftp://192.168.56.23:8096/',
    'http:192.168.56.23:8096/',
    'not-a-url',
    'http://user@192.168.56.23:8096/',
    'http://@192.168.56.23:8096/',
    'http://192.168.56.23:8096/?unexpected=value',
    'http://192.168.56.23:8096/#fragment'
)) {
    Assert-Throws { Resolve-QaServerUrl $url } "rejects invalid configured URL '$url'"
}

# The encrypted credential loader must not run until URL validation succeeds.
Assert-Throws { Get-QaCredentialForUrl 'http://192.168.56.23:8096/?bad=1' } `
    'invalid URL is rejected before credential loading'
Assert-Equal 0 $script:credentialReads 'invalid URL causes zero credential reads'
$auth = Get-QaCredentialForUrl 'http://192.168.56.23:8096/'
Assert-Equal 1 $script:credentialReads 'valid configured URL loads the encrypted credential once'
Assert-Equal 'http://192.168.56.23:8096/' $script:credentialServerUrl `
    'validated URL is passed to the credential binding check'
Assert-Equal 'http://192.168.56.23:8096/' $auth.ServerUrl 'credential result carries validated server URL'
$env:JELLYFIN_TEST_USER = 'offline-user'
$env:JELLYFIN_TEST_PASSWORD = 'offline-password'
Assert-Throws { Get-QaCredentialForUrl 'http://192.168.56.23:8096/' } `
    'environment credentials cannot bypass encrypted credential binding'
Assert-Equal 1 $script:credentialReads 'rejected environment credentials do not reach credential loader'
$env:JELLYFIN_TEST_USER = $null
$env:JELLYFIN_TEST_PASSWORD = $null

# The app receives only the one-run debug sign-in values and the exact stream-only opt-in.
$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.UseShellExecute = $false
$environment = $startInfo.EnvironmentVariables
$environment['JELLYFIN_TEST_URL'] = 'inherited-value'
$environment['JELLYFIN_TEST_USER'] = 'inherited-value'
$environment['JELLYFIN_TEST_PASSWORD'] = 'inherited-value'
$environment['KATTRI_DEBUG_STREAM_ONLY'] = '0'
Set-QaChildEnvironment $startInfo 'http://192.168.56.23:8096/' 'C:\qa-output' 'offline-user' 'offline-password' 'C:\machine-matched-mpv'
Assert-Equal '1' $environment['KATTRI_DEBUG_STREAM_ONLY'] 'stream-only environment flag is exact'
Assert-Equal 'http://192.168.56.23:8096/' $environment['KATTRI_DEBUG_URL'] 'authorized server is passed to debug sign-in'
Assert-Equal 'offline-user' $environment['KATTRI_DEBUG_USER'] 'debug username is passed to the child'
Assert-Equal 'offline-password' $environment['KATTRI_DEBUG_PASSWORD'] 'debug password is passed to the child'
Assert-Test ($environment['PATH'].StartsWith('C:\machine-matched-mpv;', [StringComparison]::OrdinalIgnoreCase)) `
    'resolved runtime directory is prepended to the child PATH'
foreach ($name in @('JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD')) {
    Assert-Test (-not $environment.ContainsKey($name)) "child environment omits $name loader input"
}

# Accessible-name matching and post-action assertions exercise both success and failure paths.
Assert-Test (Test-UiNameMatch 'Resume at 12:34' 'Resume at' -Prefix) 'prefix matching handles data-dependent labels'
Assert-Test (-not (Test-UiNameMatch 'Resume at 12:34' 'Play')) 'exact matching rejects a different control'

Reset-AssertionState @('Resume at 12:34')
Assert-Test (Assert-AnyElement @('Resume at', 'Play', 'Start over') 'details playback action' 1) `
    'any-of assertion accepts the present data-dependent playback control'
Assert-Equal 0 $script:failures.Count 'successful UI assertion records no failure'

Reset-AssertionState @('Home', 'Settings')
Assert-Test (Assert-Element 'Settings' -Context 'navigation action' ) 'required UI assertion accepts a present state'
Assert-Equal 0 $script:failures.Count 'present UI state records no failure'

Reset-AssertionState @('Home')
Assert-Test (-not (Assert-AnyElement @('Pause', 'Stop playback') 'player controls' 1)) `
    'any-of assertion rejects a missing expected player state'
Assert-Equal 1 $script:failures.Count 'missing UI state is recorded as a failure'

Reset-AssertionState @('Movies')
$script:mockSelected = $true
Assert-Test (Assert-SelectedElement 'Movies' 1) 'selected navigation state is accepted'
Assert-Equal 0 $script:failures.Count 'selected navigation state records no failure'

Reset-AssertionState @('Movies')
$script:mockSelected = $false
Assert-Test (-not (Assert-SelectedElement 'Movies' 1)) 'unselected navigation state is rejected'
Assert-Equal 1 $script:failures.Count 'unselected navigation state records a failure'

# Identical screenshots remain warnings, which is valid for static pages and scrolls.
Reset-AssertionState @()
$script:lastShotHash = 'same-frame-hash'
Record-ShotHash 'settings-general-scrolled' 'same-frame-hash'
Assert-Equal 0 $script:failures.Count 'identical static screenshot is not a failure'
Assert-Test ($script:mockNotes.Count -eq 1 -and $script:mockNotes[0] -like 'warning:*valid for static screens and scrolls*') `
    'identical screenshot is retained as a contextual warning'

$outputFixture = Join-Path ([System.IO.Path]::GetTempPath()) ('kattri-qa-output-' + [guid]::NewGuid().ToString('N'))
$outputSentinel = Join-Path $outputFixture 'REPORT.txt'
try {
    Assert-Equal ([System.IO.Path]::GetFullPath($outputFixture)) (Initialize-QaOutputDirectory $outputFixture) 'fresh output directory is created'
    [System.IO.File]::WriteAllText($outputSentinel, 'preserved evidence')
    Assert-Throws { Initialize-QaOutputDirectory $outputFixture | Out-Null } 'existing QA evidence is refused'
    Assert-Equal 'preserved evidence' ([System.IO.File]::ReadAllText($outputSentinel)) 'existing QA evidence stays intact'
} finally {
    if (Test-Path -LiteralPath $outputSentinel) { Remove-Item -LiteralPath $outputSentinel }
    if (Test-Path -LiteralPath $outputFixture) { Remove-Item -LiteralPath $outputFixture }
}

Assert-Test (Test-QaForegroundWindow ([IntPtr] 42) ([IntPtr] 42)) 'input requires the QA foreground window'
Assert-Test (-not (Test-QaForegroundWindow ([IntPtr] 99) ([IntPtr] 42))) 'another foreground window refuses input'
Assert-Test (-not (Test-QaForegroundWindow ([IntPtr]::Zero) ([IntPtr]::Zero))) 'missing windows refuse input'

if ($script:testFailures.Count -gt 0) {
    $script:testFailures | ForEach-Object { Write-Host "FAIL: $_" -ForegroundColor Red }
    Write-Host "RESULT: FAIL ($($script:testFailures.Count) failed, $($script:testCount) assertions)"
    exit 1
}

Write-Host "RESULT: PASS ($($script:testCount) offline assertions)"
$env:JELLYFIN_TEST_USER = $priorJellyfinTestUser
$env:JELLYFIN_TEST_PASSWORD = $priorJellyfinTestPassword
