$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'qa_clickthrough.ps1') -DefineOnly

$script:testCount = 0
$script:testFailures = [System.Collections.Generic.List[string]]::new()
$script:uiNames = @()
$script:mockSelectedNames = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$script:mockNotes = [System.Collections.Generic.List[string]]::new()
$script:failures = [System.Collections.Generic.List[string]]::new()
$script:report = [System.Collections.Generic.List[string]]::new()
$script:lastShotHash = $null
$script:credentialReads = 0
$script:credentialServerUrl = $null
$script:executionStateCalls = [System.Collections.Generic.List[uint32]]::new()
$script:executionStateResult = [uint32] 2147483648
$script:qaSensitiveValues = @()

$priorJellyfinTestUser = $env:JELLYFIN_TEST_USER
$priorJellyfinTestPassword = $env:JELLYFIN_TEST_PASSWORD
$env:JELLYFIN_TEST_USER = $null
$env:JELLYFIN_TEST_PASSWORD = $null

function Assert-Test([bool] $Condition, [string] $Message) {
    $script:testCount++
    if (-not $Condition) { $script:testFailures.Add($Message) }
}

function Assert-Equal($Expected, $Actual, [string] $Message) {
    Assert-Test ([object]::Equals($Expected, $Actual)) $Message
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
function Find-UiElement([string] $Name, [switch] $Prefix, [switch] $Button) {
    foreach ($candidate in $script:uiNames) {
        if (Test-UiNameMatch $candidate $Name -Prefix:$Prefix) {
            return [pscustomobject]@{
                Current = [pscustomobject]@{
                    Name = $candidate
                    IsSelected = $script:mockSelectedNames.Contains($candidate)
                }
            }
        }
    }
    return $null
}
function Get-ElementSelected($Element) { return [bool] $Element.Current.IsSelected }
function Set-QaExecutionState([uint32] $Flags) {
    $script:executionStateCalls.Add($Flags)
    return $script:executionStateResult
}
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
    $script:mockSelectedNames.Clear()
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
    Assert-Throws { Resolve-QaServerUrl $url } 'rejects invalid configured URL'
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
Assert-Test ($environment['KATTRI_DEBUG_STREAM_ONLY'] -ceq '1') 'stream-only environment flag is exact'
Assert-Test ($environment['KATTRI_DEBUG_URL'] -ceq 'http://192.168.56.23:8096/') 'authorized server is passed to debug sign-in'
Assert-Test ($environment['KATTRI_DEBUG_USER'] -ceq 'offline-user') 'debug username is passed to the child'
Assert-Test ($environment['KATTRI_DEBUG_PASSWORD'] -ceq 'offline-password') 'debug password is passed to the child'
Assert-Test ($environment['PATH'].StartsWith('C:\machine-matched-mpv;', [StringComparison]::OrdinalIgnoreCase)) `
    'resolved runtime directory is prepended to the child PATH'
foreach ($name in @('JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD')) {
    Assert-Test (-not $environment.ContainsKey($name)) "child environment omits $name loader input"
}
Clear-QaChildEnvironment $startInfo
foreach ($name in @('KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD', 'KATTRI_DEBUG_STREAM_ONLY')) {
    Assert-Test (-not $environment.ContainsKey($name)) "parent launch configuration clears $name after start"
}

# The temporary execution-state request is released with ES_CONTINUOUS in every cleanup path.
Start-QaKeepAwake
Assert-Equal ([uint32] 2147483651) $script:executionStateCalls[0] 'keep-awake requests continuous system and display requirements'
Stop-QaKeepAwake
Assert-Equal ([uint32] 2147483648) $script:executionStateCalls[1] 'keep-awake cleanup clears the temporary request'
$script:executionStateResult = [uint32] 0
Assert-Throws { Start-QaKeepAwake } 'keep-awake setup failure is reported'
Assert-Throws { Stop-QaKeepAwake } 'keep-awake cleanup failure is reported'
$script:executionStateResult = [uint32] 2147483648

# Failure text and copied evidence must not expose credentials, bearer tokens, or server URLs.
$script:qaSensitiveValues = @('offline-user', 'offline-password')
$apiKeyQueryPrefix = 'api_' + 'key='
$tokenQueryPrefix = 'to' + 'ken='
$protectedText = Protect-QaText (
    'offline-user offline-password https://example.invalid/Items?' + $apiKeyQueryPrefix +
        'url-token ' + $tokenQueryPrefix + 'token-value pass=pass-value Bearer bearer-value'
)
Assert-Test ($protectedText -notmatch 'offline-user|offline-password|https?://|url-token|token-value|pass-value|bearer-value') `
    'failure text removes credentials, tokens, and server URLs'

$logFixture = Join-Path ([System.IO.Path]::GetTempPath()) ('kattri-qa-log-' + [guid]::NewGuid().ToString('N'))
$logQaDirectory = Join-Path $logFixture 'qa'
$logOutputDirectory = Join-Path $logFixture 'out'
try {
    New-Item -ItemType Directory -Path $logQaDirectory, $logOutputDirectory | Out-Null
    [System.IO.File]::WriteAllText(
        (Join-Path $logQaDirectory 'debug.log'),
        ('ERROR request https://example.invalid/Items?' + $apiKeyQueryPrefix +
            'token-value password=offline-password'))
    $script:qaDirectory = $logQaDirectory
    $script:OutDir = $logOutputDirectory
    $script:mockNotes.Clear()
    Preserve-QaLogs
    Assert-Test (-not (Test-Path -LiteralPath (Join-Path $logOutputDirectory 'debug.log'))) `
        'raw application logs are not copied into QA evidence'
    Assert-Test (($script:mockNotes -join "`n") -notmatch 'https?://|token-value|offline-password') `
        'diagnostic log summaries contain no URL or credential data'

    $script:qaDirectory = $null
    $script:appProcess = $null
    $script:report.Clear()
    $lastKnownShot = Join-Path $logOutputDirectory '01-home.png'
    [System.IO.File]::WriteAllText($lastKnownShot, 'offline screenshot fixture')
    Save-FailureShot
    Assert-Test (Test-Path -LiteralPath (Join-Path $logOutputDirectory 'last-known-state.png')) `
        'a prior UI screenshot is retained when failure capture is unavailable'
    Assert-Test ($script:report -contains 'last-known UI screenshot evidence: last-known-state.png') `
        'the report labels the fallback screenshot as last known state'
} finally {
    if (Test-Path -LiteralPath $logFixture) { Remove-Item -LiteralPath $logFixture -Recurse -Force }
    $script:qaDirectory = $null
    $script:OutDir = $null
    $script:qaSensitiveValues = @()
}

# Accessible-name matching and post-action assertions exercise both success and failure paths.
Assert-Test (Test-UiNameMatch 'Resume at 12:34' 'Resume at' -Prefix) 'prefix matching handles data-dependent labels'
Assert-Test (Test-UiNameMatch '  Resume   at 12:34  ' 'Resume at' -Prefix) `
    'selector matching tolerates whitespace variation'
Assert-Test (-not (Test-UiNameMatch 'Resume at 12:34' 'Play')) 'exact matching rejects a different control'
Assert-Test (-not (Test-UiNameMatch 'Playlists' 'Play' -Prefix)) `
    'prefix selector rejects a different longer control name'

$qaWindowBounds = [pscustomobject]@{ X = 0; Y = 0; Width = 1920; Height = 1080; Right = 1920; Bottom = 1080 }
$qaClippedRowBounds = [pscustomobject]@{ X = 0; Y = 818; Width = 1920; Height = 311 }
$qaClippedTarget = Get-QaRowCardTarget 'Continue Watching, 7 items, focused item' `
    $qaClippedRowBounds $qaWindowBounds 144
Assert-Test ($null -ne $qaClippedTarget) `
    'poster row geometry accepts its bottom-clipped on-screen portion'
if ($qaClippedTarget) {
    Assert-Test ($qaClippedTarget.Confidence -ge 90) `
        'poster row click target retains the required confidence'
    Assert-Test ($qaClippedTarget.X -eq 198 -and $qaClippedTarget.Y -eq 1050) `
        'poster click target uses the visible first poster center'
}
$qaVisibleRowBounds = [pscustomobject]@{ X = 0; Y = 400; Width = 1920; Height = 311 }
$qaVisibleTarget = Get-QaRowCardTarget 'Continue Watching, 7 items' `
    $qaVisibleRowBounds $qaWindowBounds 144
Assert-Test ($null -ne $qaVisibleTarget -and $qaVisibleTarget.Y -eq 632) `
    'poster geometry handles a fully visible row'
$qaBadDpiDiagnostic = ''
$qaBadDpiTarget = Get-QaRowCardTarget 'Continue Watching, 7 items' `
    $qaVisibleRowBounds $qaWindowBounds 0 ([ref] $qaBadDpiDiagnostic)
Assert-Test ($null -eq $qaBadDpiTarget -and $qaBadDpiDiagnostic -like 'invalid DPI*') `
    'poster geometry reports why invalid runtime bounds were rejected'

Reset-AssertionState @('Resume at 12:34')
Assert-Test (Assert-AnyElement @('Resume at', 'Play', 'Start over') 'details playback action' 1 `
    -PrefixNames @('Resume at')) `
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
$script:mockSelectedNames.Add('Movies') | Out-Null
Assert-Test (Assert-SelectedElement 'Movies' 1) 'selected navigation state is accepted'
Assert-Equal 0 $script:failures.Count 'selected navigation state records no failure'
$script:uiNames = @('Movies', 'Home')
Assert-Test (-not (Assert-SelectedElement 'Home' 1)) 'selection state is read from the matched element only'
Assert-Equal 1 $script:failures.Count 'another selected navigation element does not satisfy the assertion'

Reset-AssertionState @('Movies')
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
