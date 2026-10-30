# Automated click-through QA for Jellyfin Kattri.
#
# Launches the debug build signed in to the configured validation server, then
# drives the real window with Windows UI Automation bounds plus OS mouse input.
# Every step captures a framebuffer screenshot through the debug harness and the
# run ends with a report. This is local QA tooling, not part of the released app.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/qa_clickthrough.ps1
#   ... -SkipPlayer        stop before playback (no libmpv needed)
#   ... -OutDir <dir>      report folder (default .verify/qa-<timestamp>)
#
# Requires an interactive desktop, the stored test credential, and a debug build.

param(
    [switch] $SkipPlayer,
    [string] $OutDir
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'test_credentials.ps1')

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public class QaMouse {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
'@

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $OutDir) { $OutDir = Join-Path $repoRoot ".verify\qa-$stamp" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$report = [System.Collections.Generic.List[string]]::new()
$failures = [System.Collections.Generic.List[string]]::new()
$step = 0

function Note([string] $text) {
    Write-Host $text
    $report.Add($text)
}

function Fail([string] $text) {
    Write-Host "FAIL: $text" -ForegroundColor Red
    $report.Add("FAIL: $text")
    $failures.Add($text)
}

function Get-AppProcess {
    Get-Process jellyfin-kattri -ErrorAction SilentlyContinue | Select-Object -First 1
}

function Get-UiRoot {
    $process = Get-AppProcess
    if (-not $process) { throw 'The application is not running.' }
    [System.Windows.Automation.AutomationElement]::FromHandle($process.MainWindowHandle)
}

function Find-UiElement([string] $name, [switch] $Prefix) {
    $root = Get-UiRoot
    $condition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty, $name)
    $found = $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
    if ($found -or -not $Prefix) { return $found }
    # Fall back to a prefix scan when the label carries a suffix (resume times, counts).
    $all = $root.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition)
    foreach ($node in $all) {
        if ($node.Current.Name -and $node.Current.Name.StartsWith($name)) { return $node }
    }
    $null
}

function Focus-App {
    $process = Get-AppProcess
    if ($process) {
        [QaMouse]::SetForegroundWindow($process.MainWindowHandle) | Out-Null
        try { (New-Object -ComObject WScript.Shell).AppActivate($process.Id) | Out-Null } catch { }
    }
    Start-Sleep -Milliseconds 300
}

function Click-Point([int] $x, [int] $y) {
    Focus-App
    [QaMouse]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 180
    # The first click may only activate an unfocused window; send a second one.
    for ($i = 0; $i -lt 2; $i++) {
        [QaMouse]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 80
        [QaMouse]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 350
    }
}

function Click-Element([string] $name, [switch] $Prefix) {
    $element = Find-UiElement $name -Prefix:$Prefix
    if (-not $element) { Fail "element not found: $name"; return $false }
    $rect = $element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { Fail "element has no bounds: $name"; return $false }
    Click-Point ([int]($rect.X + $rect.Width / 2)) ([int]($rect.Y + $rect.Height / 2))
    return $true
}

function Right-Click-Element([string] $name) {
    $element = Find-UiElement $name -Prefix
    if (-not $element) { Fail "element not found: $name"; return $false }
    $rect = $element.Current.BoundingRectangle
    Focus-App
    [QaMouse]::SetCursorPos([int]($rect.X + $rect.Width / 2), [int]($rect.Y + $rect.Height / 2)) | Out-Null
    Start-Sleep -Milliseconds 180
    [QaMouse]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)
    [QaMouse]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
    return $true
}

function Scroll-At([int] $x, [int] $y, [int] $notches) {
    Focus-App
    [QaMouse]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 120
    $delta = if ($notches -ge 0) { 0x0078 } else { 0xFF88 }
    for ($i = 0; $i -lt [Math]::Abs($notches); $i++) {
        [QaMouse]::mouse_event(0x0800, 0, 0, $delta, [UIntPtr]::Zero)
        Start-Sleep -Milliseconds 60
    }
}

function Send-AppKey([string] $keys) {
    Focus-App
    [System.Windows.Forms.SendKeys]::SendWait($keys)
    Start-Sleep -Milliseconds 150
}

function Wait-ForElement([string] $name, [int] $seconds = 15, [switch] $Prefix) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while ((Get-Date) -lt $deadline) {
        if (Find-UiElement $name -Prefix:$Prefix) { return $true }
        Start-Sleep -Milliseconds 300
    }
    return $false
}

function Get-QaDirectory {
    Get-ChildItem (Join-Path $repoRoot 'target') -Directory -Filter 'debug-qa-*' |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
}

function Save-Shot([string] $name) {
    $qa = Get-QaDirectory
    if (-not $qa) { Fail "no QA directory for screenshot $name"; return }
    $shot = Join-Path $qa.FullName 'screenshot.png'
    Remove-Item $shot -ErrorAction SilentlyContinue
    New-Item -ItemType File -Force -Path (Join-Path $qa.FullName 'screenshot.request') | Out-Null
    $deadline = (Get-Date).AddSeconds(10)
    while (-not (Test-Path $shot) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-Path $shot)) { Fail "screenshot timed out: $name"; return }
    Start-Sleep -Milliseconds 300
    $script:step++
    $target = Join-Path $OutDir ('{0:d2}-{1}.png' -f $script:step, $name)
    Copy-Item -LiteralPath $shot -Destination $target -Force
    $hash = (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash
    if ($script:lastShotHash -eq $hash) {
        Note "warning: identical frame at $name"
    }
    $script:lastShotHash = $hash
    Note "shot: $target"
}

# Opens the first Continue Watching card, by accessible name or by row geometry.
function Open-ContinueWatching {
    $card = Find-UiElement 'Runner' -Prefix
    if ($card) {
        $r = $card.Current.BoundingRectangle
        Click-Point ([int]($r.X + $r.Width / 2)) ([int]($r.Y + $r.Height / 2))
        return $true
    }
    $row = Find-UiElement 'Continue Watching' -Prefix
    if (-not $row) { return $false }
    $r = $row.Current.BoundingRectangle
    if ($r.Width -le 0) { return $false }
    Click-Point ([int]($r.X + 120)) ([int]($r.Y + $r.Height - 70))
    return $true
}

function Assert-Element([string] $name, [switch] $Prefix) {
    if (-not (Wait-ForElement $name 8 -Prefix:$Prefix)) { Fail "missing after action: $name" }
}

# --- Launch -----------------------------------------------------------------
Note "Jellyfin Kattri click-through QA, $(Get-Date -Format u)"
$serverUrl = if ($env:JELLYFIN_TEST_URL) { $env:JELLYFIN_TEST_URL } else { 'http://10.50.0.10:8096/' }
$credential = Get-JellyfinTestCredential -ServerUrl $serverUrl
if ($null -eq $credential) { throw 'No stored test credential. Run scripts\set_test_credentials.ps1 first.' }

$exe = Join-Path $repoRoot 'target\debug\jellyfin-kattri.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "Debug build not found: $exe" }

$plain = $null
try {
    $plain = $credential.GetNetworkCredential().Password
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $exe
    $info.WorkingDirectory = $repoRoot
    $info.UseShellExecute = $false
    $mpvDir = Join-Path $repoRoot 'target\libmpv\windows-x64'
    if (Test-Path -LiteralPath $mpvDir) { $info.EnvironmentVariables['PATH'] = "$mpvDir;$($env:PATH)" }
    $qaDir = Join-Path $repoRoot "target\debug-qa-qascript-$stamp"
    New-Item -ItemType Directory -Force -Path $qaDir | Out-Null
    $info.EnvironmentVariables['KATTRI_DEBUG_DIR'] = $qaDir
    $info.EnvironmentVariables['KATTRI_DEBUG_URL'] = $serverUrl
    $info.EnvironmentVariables['KATTRI_DEBUG_USER'] = $credential.UserName
    $info.EnvironmentVariables['KATTRI_DEBUG_PASSWORD'] = $plain
    [System.Diagnostics.Process]::Start($info) | Out-Null
    Note "launched: $exe"
}
finally {
    $plain = $null
    if ($null -ne $credential) { $credential.Password.Dispose() }
}

try {
    if (-not (Wait-ForElement 'Home' 45)) { throw 'The home screen never appeared.' }
    if (-not (Wait-ForElement 'Continue Watching' 45 -Prefix)) { throw 'Home rows never loaded.' }
    Start-Sleep -Seconds 2
    Note 'home loaded'
    Save-Shot 'home'

    # --- Top navigation ---
    foreach ($page in @('All items', 'Movies', 'Series', 'Collections', 'Playlists', 'Downloads', 'Watch Together')) {
        if (Click-Element $page) {
            Start-Sleep -Seconds 1
            Save-Shot ("nav-" + ($page -replace ' ', '-').ToLowerInvariant())
        }
    }

    if (Click-Element 'Settings') {
        Assert-Element 'Choose a category' -Prefix
        Save-Shot 'settings'
        foreach ($category in @('General', 'Server', 'Playback', 'Audio', 'Subtitles', 'Video', 'Input', 'About')) {
            if (Click-Element $category) {
                Start-Sleep -Milliseconds 700
                Save-Shot ("settings-" + $category.ToLowerInvariant())
                Scroll-At 500 500 3
                Start-Sleep -Milliseconds 300
                Save-Shot ("settings-" + $category.ToLowerInvariant() + '-scrolled')
                Send-AppKey '{ESC}'
                Start-Sleep -Milliseconds 300
            }
        }
        Send-AppKey '{ESC}'
        Start-Sleep -Milliseconds 400
    }

    # --- Search ---
    if (Click-Element 'Search the Jellyfin server') {
        Send-AppKey 'runner'
        Start-Sleep -Seconds 2
        Save-Shot 'search'
        Send-AppKey '^a{DEL}{ESC}'
    }

    # --- Item details and context menu ---
    if (Open-ContinueWatching) {
        Start-Sleep -Seconds 2
        Save-Shot 'details'
        Send-AppKey '{ESC}'
    } else {
        Fail 'Continue Watching card not found on the home page'
    }

    if (Click-Element 'Home') {
        Start-Sleep -Seconds 2
        $row = Find-UiElement 'Continue Watching' -Prefix
        $menuShot = $false
        if ($row) {
            $r = $row.Current.BoundingRectangle
            if ($r.Width -gt 0) {
                Focus-App
                [QaMouse]::SetCursorPos([int]($r.X + 120), [int]($r.Y + $r.Height - 70)) | Out-Null
                Start-Sleep -Milliseconds 150
                [QaMouse]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)
                [QaMouse]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
                $menuShot = $true
            }
        }
        if ($menuShot) {
            Start-Sleep -Milliseconds 700
            Save-Shot 'item-menu'
            Send-AppKey '{ESC}'
        } else {
            Note 'note: card context menu skipped'
        }
    }

    # --- Playback ---
    if (-not $SkipPlayer) {
        if (Open-ContinueWatching) {
            Start-Sleep -Seconds 2
            $resume = Find-UiElement 'Resume at' -Prefix
            $play = if ($resume) { $resume } else { Find-UiElement 'Start over' -Prefix }
            if (-not $play) { $play = Find-UiElement 'Play' }
            if ($play) {
                $rect = $play.Current.BoundingRectangle
                Click-Point ([int]($rect.X + $rect.Width / 2)) ([int]($rect.Y + $rect.Height / 2))
                Start-Sleep -Seconds 8
                Save-Shot 'player'
                # Bring the OSD back, then exercise pause, seek, volume and fullscreen.
                Click-Point 960 900
                Start-Sleep -Milliseconds 700
                Save-Shot 'player-controls'
                Send-AppKey ' '
                Start-Sleep -Milliseconds 700
                Save-Shot 'player-paused'
                Send-AppKey ' '
                Send-AppKey '{RIGHT}'
                Send-AppKey '{LEFT}'
                Start-Sleep -Milliseconds 500
                Scroll-At 960 900 -2
                Save-Shot 'player-after-seek'
                Send-AppKey '{F11}'
                Start-Sleep -Seconds 1
                Save-Shot 'player-fullscreen'
                Send-AppKey '{F11}'
                Start-Sleep -Milliseconds 700
                Send-AppKey 'x'
                Start-Sleep -Seconds 1
                Save-Shot 'after-stop'
            } else {
                Fail 'play control not found on the details page'
            }
        }
    }

    # --- Final health check ---
    $process = Get-AppProcess
    if (-not $process) {
        Fail 'the application exited during the run'
    } else {
        Note 'application still running'
    }
    $qa = Get-QaDirectory
    if ($qa) {
        $log = Join-Path $qa.FullName 'kattri-debug.log'
        if (Test-Path $log) {
            $errors = Select-String -Path $log -Pattern 'panic|ERROR|error=' -CaseSensitive:$false
            if ($errors) {
                $errors | Select-Object -First 8 | ForEach-Object { Note ("log: " + $_.Line) }
            }
            Copy-Item -LiteralPath $log -Destination (Join-Path $OutDir 'kattri-debug.log') -Force
        }
    }
}
catch {
    Fail $_.Exception.Message
}
finally {
    $process = Get-AppProcess
    if ($process) { Stop-Process -Id $process.Id -Force }
    $report.Add('')
    if ($failures.Count -eq 0) {
        $report.Add("RESULT: PASS ($step screenshots)")
        $exit = 0
    } else {
        $report.Add("RESULT: FAIL ($($failures.Count) failures, $step screenshots)")
        $exit = 1
    }
    $report | Set-Content -LiteralPath (Join-Path $OutDir 'REPORT.txt') -Encoding utf8
    Write-Host ''
    $report | Select-Object -Last 3 | ForEach-Object { Write-Host $_ }
    exit $exit
}
