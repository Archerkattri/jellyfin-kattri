# Automated click-through QA for Jellyfin Kattri.
#
# Launches the debug build against the authorized validation server and drives its real window
# with Windows UI Automation plus OS input. Assertions inspect accessible UI state; screenshots
# are retained as evidence and are never treated as proof that an action succeeded.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qa_clickthrough.ps1
#   ... -SkipPlayer        stop before playback (no libmpv needed)
#   ... -OutDir <dir>      report folder (default .verify/qa-<timestamp>)
#
# Requires an interactive desktop and a locally stored test credential.

param(
    [switch] $SkipPlayer,
    [string] $OutDir,
    [switch] $DefineOnly
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $DefineOnly) {
    . (Join-Path $PSScriptRoot 'test_credentials.ps1')

    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public class QaMouse {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool IsIconic(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool ShowWindowAsync(IntPtr hwnd, int command);
    [DllImport("user32.dll")] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] static extern bool GetUserObjectInformation(IntPtr handle, int index, StringBuilder text, uint length, out uint needed);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr handle);
    [DllImport("user32.dll")] static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("kernel32.dll")] static extern uint SetThreadExecutionState(uint flags);
    public static bool InteractiveDesktop() {
        IntPtr desktop=OpenInputDesktop(0,false,1);
        if(desktop==IntPtr.Zero) return false;
        try { uint needed; var name=new StringBuilder(256); return GetUserObjectInformation(desktop,2,name,512,out needed) && name.ToString()=="Default"; }
        finally { CloseDesktop(desktop); }
    }
    public static uint SetExecutionState(uint flags) { return SetThreadExecutionState(flags); }
    public static uint WindowDpi(IntPtr hwnd) { return GetDpiForWindow(hwnd); }
    public static void Focus(IntPtr hwnd) {
        if(!InteractiveDesktop()) throw new InvalidOperationException("Interactive desktop is unavailable; unlock it before QA.");
        if(IsIconic(hwnd)) ShowWindowAsync(hwnd,9);
        SetForegroundWindow(hwnd);
    }
}
'@
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
$script:repoRoot = $repoRoot
$script:OutDir = $OutDir
$script:SkipPlayer = $SkipPlayer
$script:DefineOnly = $DefineOnly

function Note([string] $text) {
    $safeText = Protect-QaText $text
    Write-Host $safeText
    if ($null -ne $script:report) { $script:report.Add($safeText) }
}

function Fail([string] $text) {
    $safeText = Protect-QaText $text
    Write-Host "FAIL: $safeText" -ForegroundColor Red
    if ($null -ne $script:report) { $script:report.Add("FAIL: $safeText") }
    if ($null -ne $script:failures) { $script:failures.Add($safeText) }
}

function Protect-QaText([AllowNull()][string] $Text) {
    if ($null -eq $Text) { return '' }
    $safeText = $Text
    foreach ($secret in @($script:qaSensitiveValues)) {
        if (-not [string]::IsNullOrEmpty([string] $secret)) {
            $safeText = $safeText.Replace([string] $secret, '[redacted]')
        }
    }
    $safeText = [regex]::Replace($safeText, '(?i)\b[a-z][a-z0-9+.-]*://[^\s<>"'']+', '[URL redacted]')
    $safeText = [regex]::Replace(
        $safeText,
        '(?i)\b(api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|token|user(name)?|pass(word|wd)?|passwd|pwd|authorization|secret|credential(s)?)(\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)',
        '$1$5[redacted]')
    $safeText = [regex]::Replace($safeText, '(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+', 'Bearer [redacted]')
    $safeText
}

function Get-AuthorizedServerUrl([string] $ServerUrl, [switch] $ExplicitConfiguration) {
    $uri = $null
    $valid = [Uri]::TryCreate($ServerUrl, [UriKind]::Absolute, [ref] $uri)
    $hasUserInfoDelimiter = $false
    $separatorIndex = -1
    if ($valid -and $uri.Scheme -in @('http', 'https')) {
        $separatorIndex = $ServerUrl.IndexOf('://', [StringComparison]::Ordinal)
        if ($separatorIndex -ge 0) {
            $authorityText = $ServerUrl.Substring($separatorIndex + 3)
            $authority = ($authorityText -split '[/?#]', 2)[0]
            $hasUserInfoDelimiter = $authority.Contains('@')
        }
    }
    if (-not $valid -or
        $uri.Scheme -notin @('http', 'https') -or
        $separatorIndex -lt 0 -or
        [string]::IsNullOrWhiteSpace($uri.Host) -or
        $uri.Query -or
        $uri.Fragment -or
        $uri.UserInfo -or
        $hasUserInfoDelimiter) {
        throw 'JELLYFIN_TEST_URL must be an absolute HTTP(S) URL without userinfo, query, or fragment.'
    }
    if (-not $ExplicitConfiguration -and -not $uri.IsLoopback) {
        throw 'A nonloopback Jellyfin server requires explicit JELLYFIN_TEST_URL configuration.'
    }
    $uri.AbsoluteUri
}

function Resolve-QaServerUrl([AllowNull()][string] $ConfiguredUrl) {
    $explicit = -not [string]::IsNullOrEmpty($ConfiguredUrl)
    $candidate = if ($explicit) { $ConfiguredUrl } else { 'http://127.0.0.1:8096/' }
    Get-AuthorizedServerUrl $candidate -ExplicitConfiguration:$explicit
}

function Get-QaCredentialForUrl([AllowNull()][string] $ConfiguredUrl) {
    $serverUrl = Resolve-QaServerUrl $ConfiguredUrl
    if (-not [string]::IsNullOrWhiteSpace($env:JELLYFIN_TEST_USER) -or
        -not [string]::IsNullOrWhiteSpace($env:JELLYFIN_TEST_PASSWORD)) {
        throw 'Click-through QA requires the encrypted local credential; environment credentials are not accepted.'
    }
    $credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
    [pscustomobject]@{ ServerUrl = $serverUrl; Credential = $credential }
}

function Set-QaChildEnvironment(
    [System.Diagnostics.ProcessStartInfo] $ProcessStartInfo,
    [string] $ServerUrl,
    [string] $QaDirectory,
    [string] $UserName,
    [string] $Password,
    [string] $LibmpvDirectory
) {
    $environment = $ProcessStartInfo.EnvironmentVariables
    $environment['KATTRI_DEBUG_DIR'] = $QaDirectory
    $environment['KATTRI_DEBUG_URL'] = $ServerUrl
    $environment['KATTRI_DEBUG_USER'] = $UserName
    $environment['KATTRI_DEBUG_PASSWORD'] = $Password
    $environment['KATTRI_DEBUG_STREAM_ONLY'] = '1'

    # Do not pass the credential-loader inputs to the app; only its one-run debug sign-in values.
    foreach ($name in @('JELLYFIN_TEST_URL', 'JELLYFIN_TEST_USER', 'JELLYFIN_TEST_PASSWORD')) {
        $environment.Remove($name) | Out-Null
    }
    if ($LibmpvDirectory) {
        $environment['PATH'] = "$LibmpvDirectory;$($env:PATH)"
    }
}

function Clear-QaChildEnvironment([System.Diagnostics.ProcessStartInfo] $ProcessStartInfo) {
    foreach ($name in @(
        'KATTRI_DEBUG_URL', 'KATTRI_DEBUG_USER', 'KATTRI_DEBUG_PASSWORD',
        'KATTRI_DEBUG_STREAM_ONLY'
    )) {
        $ProcessStartInfo.EnvironmentVariables.Remove($name) | Out-Null
    }
}

function Set-QaExecutionState([uint32] $Flags) {
    [QaMouse]::SetExecutionState($Flags)
}

function Start-QaKeepAwake {
    # Keep the UI active only for this run; ES_CONTINUOUS alone clears the request afterward.
    if ((Set-QaExecutionState ([uint32] 2147483651)) -eq 0) {
        throw 'Windows could not enable the temporary keep-awake request.'
    }
}

function Stop-QaKeepAwake {
    if ((Set-QaExecutionState ([uint32] 2147483648)) -eq 0) {
        throw 'Windows could not clear the temporary keep-awake request.'
    }
}

function Test-UiNameMatch([string] $Actual, [string] $Expected, [switch] $Prefix) {
    if ($null -eq $Actual -or $null -eq $Expected) { return $false }
    $normalizedActual = [regex]::Replace($Actual.Trim(), '\s+', ' ')
    $normalizedExpected = [regex]::Replace($Expected.Trim(), '\s+', ' ')
    if ($Prefix) {
        if (-not $normalizedActual.StartsWith($normalizedExpected, [StringComparison]::OrdinalIgnoreCase)) {
            return $false
        }
        if ($normalizedActual.Length -eq $normalizedExpected.Length) { return $true }
        $nextCharacter = $normalizedActual.Substring($normalizedExpected.Length, 1)
        return [char]::IsWhiteSpace($nextCharacter) -or
            $nextCharacter -in @(':', '.', ',', ';', '(', ')', '-', '—', '–')
    }
    return [string]::Equals($normalizedActual, $normalizedExpected, [StringComparison]::OrdinalIgnoreCase)
}

function Get-AppProcess {
    if ($null -eq $script:appProcess) { return $null }
    try {
        if ($script:appProcess.HasExited) { return $null }
        return $script:appProcess
    } catch {
        return $null
    }
}

function Get-UiRoot {
    $process = Get-AppProcess
    if (-not $process) { throw 'The QA application is not running.' }
    # Process caches MainWindowHandle, including an early zero while winit starts.
    $process.Refresh()
    if ($process.MainWindowHandle -eq [IntPtr]::Zero) { return $null }
    [System.Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
}

function Find-UiElement([string] $Name, [switch] $Prefix, [switch] $Button) {
    $root = Get-UiRoot
    if (-not $root) { return $null }
    try {
        $condition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
        $found = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
        if ($found) {
            $current = $found.Current
            $matchesButton = -not $Button -or
                $current.ControlType.Id -eq ([System.Windows.Automation.ControlType]::Button).Id
            $enabled = -not $Button -or $current.IsEnabled
            if (-not $current.IsOffscreen -and $matchesButton -and $enabled -and
                (Test-UiNameMatch $current.Name $Name)) {
                return $found
            }
        }
    } catch { }

    # Fall back to normalized matching when UIA's exact property lookup misses whitespace or case.
    try {
        $all = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)
    } catch { return $null }
    $exact = $null
    $prefixMatch = $null
    foreach ($node in $all) {
        try {
            $current = $node.Current
            if ($current.IsOffscreen -or [string]::IsNullOrWhiteSpace($current.Name)) { continue }
            if ($Button) {
                if (-not $current.IsEnabled -or
                    $current.ControlType.Id -ne ([System.Windows.Automation.ControlType]::Button).Id) {
                    continue
                }
            }
            if (Test-UiNameMatch $current.Name $Name) {
                $exact = $node
                break
            }
            if ($Prefix -and $null -eq $prefixMatch -and
                (Test-UiNameMatch $current.Name $Name -Prefix)) {
                $prefixMatch = $node
            }
        } catch { continue }
    }
    if ($exact) { return $exact }
    if ($Prefix) { return $prefixMatch }
    $null
}

function Get-ElementSelected($Element) {
    try {
        $pattern = $Element.GetCurrentPattern(
            [System.Windows.Automation.SelectionItemPattern]::Pattern)
        return [bool] $pattern.Current.IsSelected
    } catch { }
    try {
        $pattern = $Element.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
        return $pattern.Current.ToggleState -eq [System.Windows.Automation.ToggleState]::On
    } catch { return $false }
}

function Get-ElementRangeValue($Element) {
    try {
        $pattern = $Element.GetCurrentPattern([System.Windows.Automation.RangeValuePattern]::Pattern)
        return [double] $pattern.Current.Value
    } catch {
        return $null
    }
}

function Focus-App {
    $process = Get-AppProcess
    if (-not $process) { throw 'The QA child is not running; refusing OS input.' }
    $process.Refresh()
    $handle = $process.MainWindowHandle
    if ($handle -eq [IntPtr]::Zero) { throw 'The QA child has no window; refusing OS input.' }
    [QaMouse]::Focus($handle)
    try { (New-Object -ComObject WScript.Shell).AppActivate($process.Id) | Out-Null } catch { }
    Start-Sleep -Milliseconds 250
    if (-not (Test-QaForegroundWindow ([QaMouse]::GetForegroundWindow()) $handle)) {
        throw 'The QA window did not gain foreground focus; refusing OS input.'
    }
}

function Test-QaForegroundWindow([IntPtr] $Actual, [IntPtr] $Expected) {
    $Expected -ne [IntPtr]::Zero -and $Actual -eq $Expected
}

function Click-Point([int] $X, [int] $Y) {
    Focus-App
    $root = Get-UiRoot
    if (-not $root) { throw 'The QA window bounds are unavailable; refusing OS input.' }
    $bounds = $root.Current.BoundingRectangle
    if ($X -lt $bounds.X -or $X -ge $bounds.Right -or $Y -lt $bounds.Y -or $Y -ge $bounds.Bottom) {
        throw 'The QA click is outside its window; refusing OS input.'
    }
    [QaMouse]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 120
    [QaMouse]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 80
    [QaMouse]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 350
}

function Click-AppCenter {
    $root = Get-UiRoot
    if (-not $root) { return $false }
    $rect = $root.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { return $false }
    Click-Point ([int]($rect.X + $rect.Width / 2)) ([int]($rect.Y + $rect.Height / 2))
    return $true
}

function Click-Element([string] $Name, [switch] $Prefix, [switch] $Button) {
    $element = Find-UiElement $Name -Prefix:$Prefix -Button:$Button
    if (-not $element) { Fail "element not found: $Name"; return $false }
    $rect = $element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) {
        Fail "element has no visible bounds: $Name"
        return $false
    }
    Click-Point ([int]($rect.X + $rect.Width / 2)) ([int]($rect.Y + $rect.Height / 2))
    return $true
}

function Right-Click-Element([string] $Name, [switch] $Prefix) {
    $element = Find-UiElement $Name -Prefix:$Prefix
    if (-not $element) { Fail "element not found for context click: $Name"; return $false }
    $rect = $element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) {
        Fail "element has no visible bounds for context click: $Name"
        return $false
    }
    Focus-App
    [QaMouse]::SetCursorPos([int]($rect.X + $rect.Width / 2), [int]($rect.Y + $rect.Height / 2)) | Out-Null
    Start-Sleep -Milliseconds 120
    [QaMouse]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)
    [QaMouse]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 350
    return $true
}

function Scroll-At([int] $X, [int] $Y, [int] $Notches) {
    Focus-App
    $root = Get-UiRoot
    if (-not $root) { throw 'The QA window bounds are unavailable; refusing scroll input.' }
    $bounds = $root.Current.BoundingRectangle
    $X = [int]($bounds.X + [Math]::Min([Math]::Max(1, $X), $bounds.Width - 1))
    $Y = [int]($bounds.Y + [Math]::Min([Math]::Max(1, $Y), $bounds.Height - 1))
    [QaMouse]::SetCursorPos($X, $Y) | Out-Null
    Start-Sleep -Milliseconds 120
    $delta = if ($Notches -ge 0) { 0x0078 } else { 0xFF88 }
    for ($i = 0; $i -lt [Math]::Abs($Notches); $i++) {
        [QaMouse]::mouse_event(0x0800, 0, 0, $delta, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 60
    }
}

function Send-AppKey([string] $Keys) {
    Focus-App
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
    Start-Sleep -Milliseconds 180
}

function Wait-ForElement([string] $Name, [int] $Seconds = 15, [switch] $Prefix) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (Find-UiElement $Name -Prefix:$Prefix) { return $true }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

function Wait-ForAnyElement([string[]] $Names, [int] $Seconds = 15, [string[]] $PrefixNames = @()) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        foreach ($name in $Names) {
            if (Find-UiElement $name -Prefix:($PrefixNames -contains $name)) { return $name }
        }
        Start-Sleep -Milliseconds 300
    }
    return $null
}

function Assert-Element([string] $Name, [switch] $Prefix, [string] $Context = 'expected UI state') {
    if (-not (Wait-ForElement $Name 8 -Prefix:$Prefix)) {
        Fail "$Context missing: $Name"
        return $false
    }
    Note "asserted: $Context ($Name)"
    return $true
}

function Assert-AnyElement(
    [string[]] $Names,
    [string] $Context,
    [int] $Seconds = 8,
    [string[]] $PrefixNames = @()
) {
    $matched = Wait-ForAnyElement $Names $Seconds $PrefixNames
    if (-not $matched) {
        Fail "$Context missing; expected one of: $($Names -join ', ')"
        return $false
    }
    Note "asserted: $Context ($matched)"
    return $true
}

function Assert-SelectedElement([string] $Name, [int] $Seconds = 8) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $element = Find-UiElement $Name -Button
        if ($element -and (Get-ElementSelected $element)) {
            Note "asserted: selected navigation state ($Name)"
            return $true
        }
        Start-Sleep -Milliseconds 300
    }
    Fail "navigation state was not selected: $Name"
    return $false
}

function Assert-RangeValueChanged([string] $Name, [double] $Before, [int] $Seconds = 8) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $element = Find-UiElement $Name
        if ($element) {
            $after = Get-ElementRangeValue $element
            if ($null -ne $after -and [Math]::Abs($after - $Before) -ge 0.5) {
                Note "asserted: $Name changed from $([Math]::Round($Before, 1)) to $([Math]::Round($after, 1))"
                return $true
            }
        }
        Start-Sleep -Milliseconds 250
    }
    Fail "expected player state change did not occur: $Name"
    return $false
}

function Find-FirstVisibleCard($Container) {
    try {
        $descendants = $Container.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)
    } catch { return $null }
    foreach ($candidate in $descendants) {
        try {
            $current = $candidate.Current
            $rect = $current.BoundingRectangle
            if ($current.IsOffscreen -or -not $current.IsEnabled -or
                $current.ControlType.Id -ne ([System.Windows.Automation.ControlType]::Button).Id -or
                $rect.Width -lt 100 -or $rect.Height -lt 120 -or
                $rect.Width -gt 1000 -or $rect.Height -gt 900) {
                continue
            }
            return $candidate
        } catch { continue }
    }
    $null
}

function Get-QaRowCardTarget(
    [string] $AccessibleName,
    $RowBounds,
    $WindowBounds,
    [uint32] $Dpi,
    $Diagnostic = $null
) {
    if ([string]::IsNullOrWhiteSpace($AccessibleName) -or $null -eq $RowBounds -or $null -eq $WindowBounds) {
        if ($Diagnostic) { $Diagnostic.Value = 'missing accessible name or bounds' }
        return $null
    }
    $normalizedName = [regex]::Replace($AccessibleName.Trim(), '\s+', ' ')
    if (-not (Test-UiNameMatch $normalizedName 'Continue Watching' -Prefix) -or
        [string]::Equals($normalizedName, 'Continue Watching', [StringComparison]::OrdinalIgnoreCase)) {
        if ($Diagnostic) { $Diagnostic.Value = 'accessible name does not identify an item row' }
        return $null
    }
    if ($Dpi -lt 48 -or $Dpi -gt 768 -or $RowBounds.Width -le 0 -or $RowBounds.Height -le 0) {
        if ($Diagnostic) {
            $Diagnostic.Value = "invalid DPI or row bounds: dpi=$Dpi row=$([int]$RowBounds.X),$([int]$RowBounds.Y),$([int]$RowBounds.Width),$([int]$RowBounds.Height)"
        }
        return $null
    }

    # Continue Watching uses 216pt cards at 2:3 aspect: 144pt poster + focus padding,
    # 8pt poster gap, and a 44pt title block. The three interface zoom levels are known.
    $expectedRowHeightPoints = 207.52
    $monitorScale = [double] $Dpi / 96.0
    $bestScale = $null
    $bestHeightError = [double]::PositiveInfinity
    $bestHeightTolerance = 0.0
    $bestExpectedVisibleHeight = 0.0
    $bestObservedVisibleHeight = 0.0
    foreach ($zoom in @(1.0, 1.25, 1.6)) {
        $candidateScale = $monitorScale * $zoom
        $expectedHeight = $expectedRowHeightPoints * $candidateScale
        $expectedVisibleHeight = [Math]::Min(
            $expectedHeight,
            [Math]::Max(0.0, [double] $WindowBounds.Bottom - [double] $RowBounds.Y))
        $observedVisibleHeight = [Math]::Min(
            [double] $RowBounds.Height,
            [Math]::Max(0.0, [double] $WindowBounds.Bottom - [double] $RowBounds.Y))
        $tolerance = [Math]::Max(8.0, $expectedVisibleHeight * 0.08)
        $heightError = [Math]::Abs($observedVisibleHeight - $expectedVisibleHeight)
        if ($heightError -lt $bestHeightError) {
            $bestScale = $candidateScale
            $bestHeightError = $heightError
            $bestHeightTolerance = $tolerance
            $bestExpectedVisibleHeight = $expectedVisibleHeight
            $bestObservedVisibleHeight = $observedVisibleHeight
        }
    }
    if ($null -eq $bestScale -or $bestHeightError -gt $bestHeightTolerance) {
        if ($Diagnostic) {
            $Diagnostic.Value = "row-height mismatch: observed=$([Math]::Round($bestObservedVisibleHeight, 1)) expected=$([Math]::Round($bestExpectedVisibleHeight, 1)) error=$([Math]::Round($bestHeightError, 1)) tolerance=$([Math]::Round($bestHeightTolerance, 1)) dpi=$Dpi"
        }
        return $null
    }

    $rowRight = $RowBounds.X + $RowBounds.Width
    $rowBottom = $RowBounds.Y + $RowBounds.Height
    $windowRight = $WindowBounds.X + $WindowBounds.Width
    $windowBottom = $WindowBounds.Y + $WindowBounds.Height
    $rowIntersectsWindow = $rowRight -gt $WindowBounds.X -and
        $RowBounds.X -lt $windowRight -and $RowBounds.Y -ge ($WindowBounds.Y - 2) -and
        $RowBounds.Y -lt $windowBottom
    $firstCardSlotWidth = 264.0 * $bestScale
    if (-not $rowIntersectsWindow -or $RowBounds.Width -lt $firstCardSlotWidth) {
        if ($Diagnostic) {
            $Diagnostic.Value = "row/window rejection: intersects=$rowIntersectsWindow row_width=$([Math]::Round($RowBounds.Width, 1)) first_slot_width=$([Math]::Round($firstCardSlotWidth, 1)) row=$([int]$RowBounds.X),$([int]$RowBounds.Y),$([int]$RowBounds.Width),$([int]$RowBounds.Height) window=$([int]$WindowBounds.X),$([int]$WindowBounds.Y),$([int]$WindowBounds.Width),$([int]$WindowBounds.Height)"
        }
        return $null
    }

    # The harness has not scrolled or dragged this new row, so its initial offset is zero.
    $targetX = [double] $RowBounds.X + (132.0 * $bestScale)
    $targetY = [double] $RowBounds.Y + (154.8 * $bestScale)
    if ($targetX -lt $RowBounds.X -or $targetX -ge $rowRight -or
        $targetY -lt $RowBounds.Y -or $targetY -ge $rowBottom -or
        $targetX -lt $WindowBounds.X -or $targetX -ge $windowRight -or
        $targetY -lt $WindowBounds.Y -or $targetY -ge $windowBottom) {
        if ($Diagnostic) {
            $Diagnostic.Value = "poster point outside visible bounds: point=$([Math]::Round($targetX, 1)),$([Math]::Round($targetY, 1)) row=$([int]$RowBounds.X),$([int]$RowBounds.Y),$([int]$RowBounds.Width),$([int]$RowBounds.Height) window=$([int]$WindowBounds.X),$([int]$WindowBounds.Y),$([int]$WindowBounds.Width),$([int]$WindowBounds.Height)"
        }
        return $null
    }

    $heightConfidence = 30.0 * (1.0 - ($bestHeightError / $bestHeightTolerance))
    $confidence = [int] [Math]::Round(35.0 + $heightConfidence + 15.0 + 10.0 + 5.0)
    if ($confidence -lt 90) {
        if ($Diagnostic) { $Diagnostic.Value = "confidence below threshold: confidence=$confidence height_error=$([Math]::Round($bestHeightError, 1)) tolerance=$([Math]::Round($bestHeightTolerance, 1))" }
        return $null
    }
    if ($Diagnostic) { $Diagnostic.Value = "accepted: confidence=$confidence scale=$([Math]::Round($bestScale, 3)) point=$([Math]::Round($targetX, 1)),$([Math]::Round($targetY, 1))" }
    [pscustomobject]@{
        X = [int] [Math]::Round($targetX, [MidpointRounding]::AwayFromZero)
        Y = [int] [Math]::Round($targetY, [MidpointRounding]::AwayFromZero)
        Confidence = $confidence
    }
}

function Find-ContinueWatchingRow {
    $root = Get-UiRoot
    if (-not $root) { return $null }
    $process = Get-AppProcess
    if (-not $process) { return $null }
    $process.Refresh()
    $handle = $process.MainWindowHandle
    if ($handle -eq [IntPtr]::Zero) { return $null }
    try {
        $windowBounds = $root.Current.BoundingRectangle
        $all = $root.FindAll(
            [System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)
    } catch { return $null }
    try { $dpi = [QaMouse]::WindowDpi($handle) } catch { $dpi = [uint32] 0 }

    $bestRow = $null
    $candidateCount = 0
    $namedRowCount = 0
    $visibleCardRowCount = 0
    $geometryTargetCount = 0
    $largestRow = 0
    $rowSample = $null
    $firstGeometryDiagnostic = $null
    foreach ($candidate in $all) {
        try {
            $current = $candidate.Current
            $accessibleName = [string] $current.Name
            $candidateCount++
            if ($current.IsOffscreen -or [string]::IsNullOrWhiteSpace($accessibleName) -or
                -not (Test-UiNameMatch $accessibleName 'Continue Watching' -Prefix)) {
                continue
            }
            if ([string]::Equals(
                    [regex]::Replace($accessibleName.Trim(), '\s+', ' '),
                    'Continue Watching',
                    [StringComparison]::OrdinalIgnoreCase)) {
                continue
            }
            $namedRowCount++
            $rowBounds = $current.BoundingRectangle
            if ($null -eq $rowSample) { $rowSample = $rowBounds }
            $largestRow = [Math]::Max($largestRow, [int] $rowBounds.Height)
            $geometryDiagnostic = ''
            $target = Get-QaRowCardTarget $accessibleName $rowBounds $windowBounds $dpi ([ref] $geometryDiagnostic)
            if (-not $target -and -not $firstGeometryDiagnostic) {
                $firstGeometryDiagnostic = $geometryDiagnostic
            }
            $card = Find-FirstVisibleCard $candidate
            if ($target) { $geometryTargetCount++ }
            if ($card) { $visibleCardRowCount++ }
            if ($target -or $card) {
                $replaceBest = -not $bestRow
                if (-not $replaceBest -and $target -and
                    (-not $bestRow.Target -or $target.Confidence -gt $bestRow.Target.Confidence)) {
                    $replaceBest = $true
                }
                if ($replaceBest) {
                    $bestRow = [pscustomobject]@{ Element = $candidate; Target = $target }
                }
            }
        } catch { continue }
    }
    $rowGeometry = if ($rowSample) {
        "$([int]$rowSample.X),$([int]$rowSample.Y),$([int]$rowSample.Width),$([int]$rowSample.Height)"
    } else { 'none' }
    Note "home row target probe: nodes=$candidateCount rows=$namedRowCount cards=$visibleCardRowCount geometry=$geometryTargetCount row_xywh=$rowGeometry dpi=$dpi window_xywh=$([int]$windowBounds.X),$([int]$windowBounds.Y),$([int]$windowBounds.Width),$([int]$windowBounds.Height) geometry_detail=$firstGeometryDiagnostic"
    $bestRow
}

function Click-ContinueWatchingCard([switch] $RightClick) {
    $row = Find-ContinueWatchingRow
    if (-not $row) { return $false }
    $card = Find-FirstVisibleCard $row.Element
    if ($card) {
        $rect = $card.Current.BoundingRectangle
        if ($rect.Width -le 0 -or $rect.Height -le 0) { return $false }
        $x = [int]($rect.X + $rect.Width / 2)
        $y = [int]($rect.Y + $rect.Height / 2)
    } elseif ($row.Target -and $row.Target.Confidence -ge 90) {
        $x = $row.Target.X
        $y = $row.Target.Y
        Note "using Continue Watching row geometry fallback (confidence $($row.Target.Confidence)/100)"
    } else {
        return $false
    }

    if (-not $RightClick) {
        Click-Point $x $y
        return $true
    }
    Focus-App
    $root = Get-UiRoot
    if (-not $root) { return $false }
    $bounds = $root.Current.BoundingRectangle
    if ($x -lt $bounds.X -or $x -ge $bounds.Right -or $y -lt $bounds.Y -or $y -ge $bounds.Bottom) {
        return $false
    }
    [QaMouse]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 150
    [QaMouse]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)
    [QaMouse]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 350
    return $true
}

function Click-TopNavigation([string] $Name) {
    if (Find-UiElement $Name -Button) { return (Click-Element $Name -Button) }
    $more = Find-UiElement 'More' -Prefix -Button
    if (-not $more) { Fail "navigation item not visible and More menu missing: $Name"; return $false }
    if (-not (Click-Element 'More' -Prefix -Button)) { return $false }
    if (-not (Wait-ForElement $Name 5)) { Fail "navigation menu item missing: $Name"; return $false }
    Click-Element $Name -Button
}

function Get-QaDirectory {
    $script:qaDirectory
}

function Save-Shot([string] $Name) {
    $qa = Get-QaDirectory
    if (-not $qa) { Fail "no QA directory for screenshot $Name"; return }
    $shot = Join-Path $qa 'screenshot.png'
    Remove-Item -LiteralPath $shot -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $qa 'video-screenshot.png') -ErrorAction SilentlyContinue
    New-Item -ItemType File -Force -Path (Join-Path $qa 'screenshot.request') | Out-Null
    $deadline = (Get-Date).AddSeconds(10)
    while (-not (Test-Path -LiteralPath $shot) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
    }
    if (-not (Test-Path -LiteralPath $shot)) { Fail "screenshot timed out: $Name"; return }
    Start-Sleep -Milliseconds 200
    $script:step++
    $slug = $Name -replace '[^A-Za-z0-9._-]', '-'
    $target = Join-Path $script:OutDir ('{0:d2}-{1}.png' -f $script:step, $slug)
    Copy-Item -LiteralPath $shot -Destination $target -Force
    $videoShot = Join-Path $qa 'video-screenshot.png'
    if (Test-Path -LiteralPath $videoShot -PathType Leaf) {
        $videoTarget = Join-Path $script:OutDir ('{0:d2}-{1}-video.png' -f $script:step, $slug)
        Copy-Item -LiteralPath $videoShot -Destination $videoTarget
        Note "raw video framebuffer evidence: $videoTarget"
    }
    $hash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
    Record-ShotHash $Name $hash
    Note "shot evidence: $target"
}

function Record-ShotHash([string] $Name, [string] $Hash) {
    if ($script:lastShotHash -eq $Hash) {
        Note "warning: identical frame at $Name (valid for static screens and scrolls)"
    }
    $script:lastShotHash = $Hash
}

function Save-FailureShot {
    try {
        $qa = Get-QaDirectory
        if ($qa -and (Get-AppProcess)) {
            $shot = Join-Path $qa 'screenshot.png'
            Remove-Item -LiteralPath $shot -ErrorAction SilentlyContinue
            New-Item -ItemType File -Force -Path (Join-Path $qa 'screenshot.request') | Out-Null
            $deadline = (Get-Date).AddSeconds(5)
            while (-not (Test-Path -LiteralPath $shot) -and (Get-Date) -lt $deadline) {
                Start-Sleep -Milliseconds 200
            }
            if (Test-Path -LiteralPath $shot) {
                Copy-Item -LiteralPath $shot -Destination (Join-Path $script:OutDir 'failure-state.png') -Force
                $script:report.Add('failure-state screenshot evidence: failure-state.png')
                return
            }
        }
        $lastShot = Get-ChildItem -LiteralPath $script:OutDir -File -Filter '*.png' -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^\d{2}-.*\.png$' -and $_.Name -notmatch '-video\.png$' } |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($lastShot) {
            Copy-Item -LiteralPath $lastShot.FullName -Destination (Join-Path $script:OutDir 'last-known-state.png') -Force
            $script:report.Add('last-known UI screenshot evidence: last-known-state.png')
        } else {
            $script:report.Add('failure-state screenshot unavailable: no application screenshot was captured')
        }
    } catch {
        $script:report.Add("failure screenshot evidence unavailable: $(Protect-QaText $_.Exception.Message)")
    }
}

function Open-ContinueWatching {
    Click-ContinueWatchingCard
}

function Resolve-QaPlayerRuntime([string] $Executable) {
    $helper = Join-Path $script:repoRoot 'scripts\libmpv_runtime.ps1'
    if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
        throw "libmpv runtime helper is missing: $helper"
    }
    . $helper
    $directory = Resolve-KattriLibmpvDirectory -RepoRoot $script:repoRoot -Executable $Executable
    if ([string]::IsNullOrWhiteSpace([string] $directory)) {
        throw 'No machine-matched Windows libmpv runtime was found; use -SkipPlayer to run UI-only QA.'
    }
    [string] $directory
}

function Preserve-QaLogs {
    $qa = Get-QaDirectory
    if (-not $qa) { return }
    foreach ($log in Get-ChildItem -LiteralPath $qa -File -Filter '*.log' -ErrorAction SilentlyContinue) {
        $errorCount = @(Select-String -LiteralPath $log.FullName -Pattern 'panic|ERROR|error=' -CaseSensitive:$false).Count
        if ($errorCount -gt 0) {
            Note "debug log has $errorCount diagnostic error marker(s); raw contents omitted to keep credentials and server URLs out of QA artifacts"
        }
    }
    $metrics = Join-Path $qa 'qa-metrics.txt'
    if (Test-Path -LiteralPath $metrics -PathType Leaf) {
        Copy-Item -LiteralPath $metrics -Destination (Join-Path $script:OutDir 'qa-metrics.txt')
        Note 'numeric performance evidence: qa-metrics.txt'
    }
}

function Initialize-QaOutputDirectory([string] $Path) {
    $resolved = [System.IO.Path]::GetFullPath($Path)
    if (Test-Path -LiteralPath $resolved) {
        if (-not (Test-Path -LiteralPath $resolved -PathType Container) -or
            @(Get-ChildItem -LiteralPath $resolved -Force | Select-Object -First 1).Count -gt 0) {
            throw 'Refusing to overwrite an existing nonempty QA output directory.'
        }
    } else {
        New-Item -ItemType Directory -Path $resolved | Out-Null
    }
    $resolved
}

function Invoke-QaClickthrough {
    $script:stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    if (-not $script:OutDir) {
        $script:OutDir = Join-Path $script:repoRoot ".verify\qa-$script:stamp"
    }
    $script:OutDir = Initialize-QaOutputDirectory $script:OutDir
    $script:report = [System.Collections.Generic.List[string]]::new()
    $script:failures = [System.Collections.Generic.List[string]]::new()
    $script:step = 0
    $script:lastShotHash = $null
    $script:appProcess = $null
    $script:qaDirectory = $null
    $credential = $null
    $plain = $null
    $script:qaSensitiveValues = @(
        @($env:JELLYFIN_TEST_USER, $env:JELLYFIN_TEST_PASSWORD) |
            Where-Object { -not [string]::IsNullOrEmpty([string] $_) }
    )
    $keepAwakeActive = $false

    Note "Jellyfin Kattri click-through QA, $([DateTime]::UtcNow.ToString('u'))"
    Note 'authorization: configured server; playback stream-only; server session reporting disabled'
    try {
        # A display sleep mid-run can drop foreground focus; release this request in finally.
        Start-QaKeepAwake
        $keepAwakeActive = $true
        $configuredServerUrl = $env:JELLYFIN_TEST_URL
        $serverUrl = Resolve-QaServerUrl $configuredServerUrl
        $script:qaSensitiveValues = @($script:qaSensitiveValues) + @($serverUrl)
        $exe = Join-Path $script:repoRoot 'target\debug\jellyfin-kattri.exe'
        if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Debug build not found: $exe" }

        # Resolve and validate libmpv before loading credentials or starting the app.
        $mpvDirectory = $null
        if (-not $script:SkipPlayer) { $mpvDirectory = Resolve-QaPlayerRuntime $exe }

        $auth = Get-QaCredentialForUrl $configuredServerUrl
        $serverUrl = $auth.ServerUrl
        $credential = $auth.Credential
        if ($null -eq $credential) {
            throw 'No stored test credential. Run scripts\set_test_credentials.ps1 first.'
        }

        $plain = $credential.GetNetworkCredential().Password
        $newSensitiveValues = @($credential.UserName, $plain) |
            Where-Object { -not [string]::IsNullOrEmpty([string] $_) }
        $script:qaSensitiveValues = @($script:qaSensitiveValues) + @($newSensitiveValues)
        $newSensitiveValues = @()
        $info = New-Object System.Diagnostics.ProcessStartInfo
        $info.FileName = $exe
        $info.WorkingDirectory = $script:repoRoot
        $info.UseShellExecute = $false
        $script:qaDirectory = Join-Path $script:repoRoot "target\debug-qa-qascript-$script:stamp"
        New-Item -ItemType Directory -Force -Path $script:qaDirectory | Out-Null
        Set-QaChildEnvironment $info $serverUrl $script:qaDirectory $credential.UserName $plain $mpvDirectory
        $script:appProcess = [System.Diagnostics.Process]::Start($info)
        if (-not $script:appProcess) { throw 'The debug application failed to start.' }
        Clear-QaChildEnvironment $info
        $plain = $null
        $serverUrl = $null
        $configuredServerUrl = $null
        Note "launched: $exe (pid $($script:appProcess.Id))"

        if (-not (Wait-ForElement 'Home' 45)) { throw 'The Home navigation state never appeared.' }
        Assert-SelectedElement 'Home' 10 | Out-Null
        if (-not (Wait-ForElement 'Continue Watching' 45 -Prefix)) { throw 'Home rows never loaded.' }
        Assert-Element 'Stream-only validation — session reporting is disabled.' -Context 'stream-only authorization' | Out-Null
        Start-Sleep -Seconds 2
        Save-Shot 'home'

        # --- Top navigation: assert each clicked destination became the selected page. ---
        foreach ($page in @('Movies', 'Series', 'Collections', 'Playlists', 'Downloads', 'Watch Together')) {
            if (Click-TopNavigation $page) {
                Start-Sleep -Seconds 1
                Assert-SelectedElement $page | Out-Null
                Save-Shot ("nav-" + ($page -replace ' ', '-').ToLowerInvariant())
            }
        }

        if (Click-TopNavigation 'Settings') {
            Assert-SelectedElement 'Settings' | Out-Null
            Assert-Element 'Choose a category' -Prefix -Context 'settings category list' | Out-Null
            Save-Shot 'settings'
            foreach ($category in @('General', 'Server', 'Playback', 'Audio', 'Subtitles', 'Video', 'Input', 'About')) {
                if (Click-Element $category) {
                    Start-Sleep -Milliseconds 700
                    Assert-Element $category -Context 'settings category page' | Out-Null
                    Save-Shot ("settings-" + $category.ToLowerInvariant())
                    Scroll-At 500 500 3
                    Start-Sleep -Milliseconds 300
                    # A static settings page can legitimately render the same frame after scrolling.
                    Save-Shot ("settings-" + $category.ToLowerInvariant() + '-scrolled')
                    Send-AppKey '{ESC}'
                    Start-Sleep -Milliseconds 350
                    # The Input page can be capturing a key binding, where the first Escape cancels
                    # the capture instead of leaving the page. A second Escape is the real back.
                    if (-not (Find-UiElement 'Choose a category' -Prefix)) { Send-AppKey '{ESC}' }
                    Assert-Element 'Choose a category' -Prefix -Context 'settings category return' | Out-Null
                }
            }
            Send-AppKey '{ESC}'
            # Settings can return to Home while the Watch Together button remains visible.
            Click-TopNavigation 'Watch Together' | Out-Null
            Assert-SelectedElement 'Watch Together' | Out-Null
        }

        if (Click-TopNavigation 'Home') { Assert-SelectedElement 'Home' | Out-Null }

        # --- Search: use a synthetic query and assert the accessible input value. ---
        $query = 'qa-query-that-should-not-match'
        if (Click-Element 'Search the Jellyfin server') {
            Send-AppKey $query
            Start-Sleep -Seconds 2
            $searchField = Find-UiElement 'Search the Jellyfin server'
            $searchValue = $null
            if ($searchField) {
                try {
                    $valuePattern = $searchField.GetCurrentPattern(
                        [System.Windows.Automation.ValuePattern]::Pattern)
                    $searchValue = [string] $valuePattern.Current.Value
                } catch { }
            }
            if (-not [string]::Equals($searchValue, $query, [StringComparison]::OrdinalIgnoreCase)) {
                Fail 'search input did not retain the synthetic query'
            }
            else { Note 'asserted: search input retained the synthetic query' }
            Save-Shot 'search'
            Send-AppKey '^a{DEL}{ESC}'
            Assert-Element 'Continue Watching' -Prefix -Context 'home after search clear' | Out-Null
        }

        # --- Details and context menu: assert generic controls, not a media title. ---
        if (Open-ContinueWatching) {
            Start-Sleep -Seconds 2
            Assert-AnyElement @('Resume at', 'Play', 'Start over') 'item details playback action' `
                -PrefixNames @('Resume at') | Out-Null
            Save-Shot 'details'
            Send-AppKey '{ESC}'
        } else {
            Fail 'Continue Watching card was not available for details QA'
        }

        if (Click-TopNavigation 'Home') {
            Assert-SelectedElement 'Home' | Out-Null
            Start-Sleep -Seconds 1
            if (Click-ContinueWatchingCard -RightClick) {
                Assert-Element 'Open' -Context 'item context menu' | Out-Null
                Save-Shot 'item-menu'
                Send-AppKey '{ESC}'
            } else {
                Fail 'Continue Watching row had no high-confidence card target for context-menu QA'
            }
        }

        # --- Playback: the child has session reporting disabled and each state is asserted. ---
        if (-not $script:SkipPlayer) {
            if (Open-ContinueWatching) {
                Start-Sleep -Seconds 2
                $play = Find-UiElement 'Resume at' -Prefix
                if (-not $play) { $play = Find-UiElement 'Play' }
                if (-not $play) { $play = Find-UiElement 'Start over' }
                if ($play) {
                    $rect = $play.Current.BoundingRectangle
                    Click-Point ([int]($rect.X + $rect.Width / 2)) ([int]($rect.Y + $rect.Height / 2))
                    Start-Sleep -Seconds 8
                    if (-not (Click-AppCenter)) { Fail 'player window bounds were unavailable to reveal controls' }
                    Assert-Element 'Pause' -Context 'playing transport state' | Out-Null
                    Assert-Element 'Stream-only validation — session reporting is disabled.' -Context 'playback reporting suppression' | Out-Null
                    Save-Shot 'player-playing'

                    $position = Find-UiElement 'Playback position'
                    $beforeSeek = if ($position) { Get-ElementRangeValue $position } else { $null }
                    Send-AppKey ' '
                    Assert-Element 'Play' -Context 'paused transport state' | Out-Null
                    Save-Shot 'player-paused'
                    if ($null -ne $beforeSeek) {
                        Send-AppKey '{RIGHT}'
                        Assert-RangeValueChanged 'Playback position' $beforeSeek | Out-Null
                    } else {
                        Fail 'player seek control did not expose a range value'
                    }
                    Save-Shot 'player-after-seek'
                    Send-AppKey ' '
                    Assert-Element 'Pause' -Context 'resumed transport state' | Out-Null

                    if (Click-Element 'Enter fullscreen') {
                        Assert-Element 'Leave fullscreen' -Context 'fullscreen player state' | Out-Null
                        Save-Shot 'player-fullscreen'
                        Click-Element 'Leave fullscreen' | Out-Null
                        Assert-Element 'Enter fullscreen' -Context 'windowed player state' | Out-Null
                    }

                    Send-AppKey 'x'
                    Assert-AnyElement @('Resume at', 'Play', 'Start over') 'details state after stopping playback' `
                        -PrefixNames @('Resume at') | Out-Null
                    Save-Shot 'after-stop'
                } else {
                    Fail 'play control was not available on the details page'
                }
            } else {
                Fail 'Continue Watching card was not available for playback QA'
            }
        }

        $process = Get-AppProcess
        if (-not $process) {
            Fail 'the QA application exited during the run'
        } else {
            Note 'application still running'
        }
    } catch {
        Fail $_.Exception.Message
    } finally {
        if ($keepAwakeActive) {
            try { Stop-QaKeepAwake } catch { Fail 'Windows could not clear the temporary keep-awake request' }
        }
        if ($script:failures.Count -gt 0) { Save-FailureShot }
        $process = Get-AppProcess
        if ($process) {
            try {
                $null = $process.CloseMainWindow()
                if (-not $process.WaitForExit(3000)) { $process.Kill(); $process.WaitForExit() }
            } catch {
                try { $process.Kill() } catch { }
            }
        }
        try { Preserve-QaLogs } catch {
            $script:report.Add("log evidence could not be summarized: $(Protect-QaText $_.Exception.Message)")
        }
        if ($null -ne $credential) {
            try {
                if ($null -ne $credential.Password) { $credential.Password.Dispose() }
            } catch { $script:report.Add('credential cleanup reported an error') }
        }
        $credential = $null
        $auth = $null
        if ($null -ne $info) {
            try { Clear-QaChildEnvironment $info } catch { Fail 'launch environment cleanup reported an error' }
        }
        $plain = $null
        $serverUrl = $null
        $configuredServerUrl = $null
        $script:report.Add('')
        if ($script:failures.Count -eq 0) {
            $script:report.Add("RESULT: PASS ($($script:step) screenshots; UI state assertions passed)")
            $script:qaResult = 0
        } else {
            $script:report.Add("RESULT: FAIL ($($script:failures.Count) failures, $($script:step) screenshots)")
            $script:qaResult = 1
        }
        $safeReport = @($script:report | ForEach-Object { Protect-QaText $_ })
        $script:qaSensitiveValues = @()
        $safeReport | Set-Content -LiteralPath (Join-Path $script:OutDir 'REPORT.txt') -Encoding utf8
        Write-Host ''
        $safeReport | Select-Object -Last 3 | ForEach-Object { Write-Host $_ }
    }
    return $script:qaResult
}

if (-not $DefineOnly) {
    exit (Invoke-QaClickthrough)
}
