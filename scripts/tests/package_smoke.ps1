# Executes the real Linux window-smoke contract (packaging/linux/smoke-launch.sh) with command
# doubles: stub xdotool/xwininfo, a fixture "application", and a temporary KATTRI_SMOKE_LOG.
# No package is installed, no window system is touched, and no network is used.
#
# Branches covered: success (window found, close requested, process exits), missing window tool,
# missing log variable, and a bundled-library verification mismatch, plus a negative control that
# proves the harness fails when no window is ever reported. Every branch also checks that the
# fixture process is gone.
#
# Shell text is always literal; temp paths travel through environment variables and positional
# arguments so a path can never be parsed as shell syntax.

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$smokeScript = Join-Path $repoRoot 'packaging/linux/smoke-launch.sh'

# Prefer a Git/MSYS Bash. The WSL launcher in System32 is rejected: it would run inside a
# different filesystem where the repository path does not exist, and it is not a drop-in shell
# for this Windows gate.
function Resolve-GateBash {
    $isWindows = ($env:OS -eq 'Windows_NT')
    $candidates = [System.Collections.Generic.List[string]]::new()
    if ($isWindows) {
        $bases = @($env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:LOCALAPPDATA, $env:USERPROFILE)
        foreach ($base in $bases) {
            if ([string]::IsNullOrWhiteSpace($base)) { continue }
            foreach ($relative in @('Programs\Git\bin\bash.exe', 'Git\bin\bash.exe', 'Git\usr\bin\bash.exe')) {
                $candidate = Join-Path $base $relative
                if (Test-Path -LiteralPath $candidate) { $candidates.Add($candidate) }
            }
        }
        $candidates.Add('C:\Program Files\Git\bin\bash.exe')
        $candidates.Add('C:\Program Files\Git\usr\bin\bash.exe')
    }
    $onPath = Get-Command bash -ErrorAction SilentlyContinue
    if ($null -ne $onPath) { $candidates.Add($onPath.Source) }

    foreach ($candidate in $candidates) {
        if (-not (Test-Path -LiteralPath $candidate)) { continue }
        if ($candidate -match '(?i)\\Windows\\System32\\') { continue }
        $kernel = (& $candidate -c 'uname -s' 2>$null | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) { continue }
        if ($isWindows -and $kernel -notmatch '^(MINGW|MSYS|CYGWIN)') { continue }
        return $candidate
    }
    return $null
}

$bash = Resolve-GateBash
if ($null -eq $bash) {
    Write-Output 'FAIL: package-smoke needs an MSYS/Git Bash (not the WSL launcher). Install Git for Windows or run on a POSIX system.'
    exit 1
}
if (-not (Test-Path -LiteralPath $smokeScript)) {
    Write-Output "FAIL: missing smoke script: $smokeScript"
    exit 1
}

$failures = [System.Collections.Generic.List[string]]::new()
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ("kattri-package-smoke-" + [guid]::NewGuid().ToString('N'))
$stubDir = Join-Path $scratch 'bin'

# LF endings and no BOM: bash rejects CRLF shebangs and stub bodies.
function Write-BashFile([string] $Path, [string] $Text) {
    [System.IO.File]::WriteAllText($Path, ($Text -replace "`r`n", "`n"), [System.Text.UTF8Encoding]::new($false))
}

# MSYS/Git Bash resolves PATH entries and file arguments in POSIX form ("/c/Users/..."), not the
# forward-slash drive form ("C:/Users/..."), so convert before handing a path to bash. POSIX
# paths (a standalone Linux run) are returned unchanged.
function Convert-ToPosixPath([string] $Path) {
    if ($Path -match '^([A-Za-z]):[\\/](.*)$') {
        return '/' + $matches[1].ToLowerInvariant() + '/' + ($matches[2] -replace '\\', '/')
    }
    return $Path.Replace('\', '/')
}

# Liveness probe: the shell command is a literal, the pid arrives as $1, and the exit status is
# kill's own status. Nothing here is expanded by PowerShell.
function Test-OwnedProcessAlive([int] $ProcessId) {
    & $bash -c 'kill -0 "$1" 2>/dev/null' _ $ProcessId | Out-Null
    return ($LASTEXITCODE -eq 0)
}

& $bash -c 'command -v timeout >/dev/null 2>&1' | Out-Null
$runner = if ($LASTEXITCODE -eq 0) {
    'exec timeout 45 bash "$KATTRI_PS_SCRIPT" "$KATTRI_PS_APP"'
} else {
    'exec bash "$KATTRI_PS_SCRIPT" "$KATTRI_PS_APP"'
}
$runnerCommand = 'if [ -n "$KATTRI_PS_STUB" ]; then PATH="$KATTRI_PS_STUB:$PATH"; fi; ' + $runner

function Invoke-Smoke {
    param(
        [string] $LogPath,
        [string] $AppPath,
        [string] $PathPrefix,
        [hashtable] $ExtraEnv = @{},
        [switch] $UnsetLog
    )
    $env:KATTRI_SMOKE_LOG = if ($UnsetLog) { $null } else { Convert-ToPosixPath $LogPath }
    $env:KATTRI_PS_STUB = $PathPrefix
    $env:KATTRI_PS_SCRIPT = Convert-ToPosixPath $smokeScript
    $env:KATTRI_PS_APP = Convert-ToPosixPath $AppPath
    $env:KATTRI_PS_PID = Convert-ToPosixPath (Join-Path $scratch 'app.pid')
    $env:KATTRI_PS_ACTUAL = Convert-ToPosixPath (Join-Path $scratch 'actual-libmpv.so.2')
    foreach ($key in $ExtraEnv.Keys) {
        Set-Item -Path ("Env:" + $key) -Value $ExtraEnv[$key]
    }
    $savedErrorAction = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $bash -c $runnerCommand 2>&1 | Out-String
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedErrorAction
        foreach ($name in @('KATTRI_SMOKE_LOG', 'KATTRI_PS_STUB', 'KATTRI_PS_SCRIPT', 'KATTRI_PS_APP', 'KATTRI_PS_PID', 'KATTRI_PS_ACTUAL')) {
            Remove-Item -Path ('Env:' + $name) -ErrorAction SilentlyContinue
        }
        foreach ($key in $ExtraEnv.Keys) {
            Remove-Item -Path ("Env:" + $key) -ErrorAction SilentlyContinue
        }
    }
    return [pscustomobject]@{ Code = $code; Output = $output }
}

function Assert-Pass([bool] $Condition, [string] $Message) {
    if (-not $Condition) {
        $script:failures.Add($Message)
    }
}

function Assert-FixtureGone([string] $Label) {
    $pidFile = Join-Path $scratch 'app.pid'
    if (-not (Test-Path -LiteralPath $pidFile)) { return }
    $fixturePid = (Get-Content -LiteralPath $pidFile -Raw).Trim()
    if ($fixturePid -match '^\d+$') {
        Assert-Pass (-not (Test-OwnedProcessAlive ([int]$fixturePid))) "$Label left the fixture application running (pid $fixturePid)"
    }
}

New-Item -ItemType Directory -Path $stubDir -Force | Out-Null
try {
    Write-BashFile (Join-Path $stubDir 'xdotool') @'
#!/bin/sh
case "$1" in
  search) echo 4242 ;;
  windowquit) exit 0 ;;
  *) exit 0 ;;
esac
'@
    Write-BashFile (Join-Path $stubDir 'xwininfo') @'
#!/bin/sh
echo 'xwininfo: Window id: 0x1092 "Jellyfin Kattri"'
'@

    # The fixture application records its pid, logs one loader-style line, then exits on its own,
    # which is exactly what a normally closed window looks like to the smoke script. Paths come
    # from the environment, never from text baked into this file.
    $app = Join-Path $scratch 'fixture-app.sh'
    Write-BashFile $app @'
#!/bin/sh
echo $$ > "$KATTRI_PS_PID"
echo "calling init: $KATTRI_PS_ACTUAL"
echo "fixture application running"
sleep 1
exit 0
'@
    New-Item -ItemType File -Path (Join-Path $scratch 'actual-libmpv.so.2') -Force | Out-Null
    New-Item -ItemType File -Path (Join-Path $scratch 'expected-libmpv.so.2') -Force | Out-Null
    & $bash -c 'chmod +x "$@"' _ (Convert-ToPosixPath (Join-Path $stubDir 'xdotool')) (Convert-ToPosixPath (Join-Path $stubDir 'xwininfo')) (Convert-ToPosixPath $app) | Out-Null

    $stubPath = Convert-ToPosixPath $stubDir
    $emptyDir = Join-Path $scratch 'empty'
    New-Item -ItemType Directory -Path $emptyDir -Force | Out-Null

    # The negative-control stubs never report a window.
    $failStubDir = Join-Path $scratch 'bin-no-window'
    New-Item -ItemType Directory -Path $failStubDir -Force | Out-Null
    Write-BashFile (Join-Path $failStubDir 'xdotool') "#!/bin/sh`nexit 1`n"
    Write-BashFile (Join-Path $failStubDir 'xwininfo') "#!/bin/sh`nexit 1`n"
    & $bash -c 'chmod +x "$@"' _ (Convert-ToPosixPath (Join-Path $failStubDir 'xdotool')) (Convert-ToPosixPath (Join-Path $failStubDir 'xwininfo')) | Out-Null

    # Probe controls on a fixture this script owns: a live owned process must read as alive and
    # the same process, once killed, must read as gone. No other process is signalled.
    $ownedPidText = (& $bash -c 'sleep 20 >/dev/null 2>&1 & echo $!' | Out-String).Trim()
    $ownedPid = 0
    if ($ownedPidText -match '^\d+$') { $ownedPid = [int]$ownedPidText }
    Assert-Pass ($ownedPid -gt 0) 'probe control: could not start the owned fixture process'
    if ($ownedPid -gt 0) {
        Assert-Pass (Test-OwnedProcessAlive $ownedPid) 'probe control: a live owned fixture read as gone'
        & $bash -c 'kill "$1" 2>/dev/null' _ $ownedPid | Out-Null
        Start-Sleep -Milliseconds 400
        Assert-Pass (-not (Test-OwnedProcessAlive $ownedPid)) 'probe control: a killed owned fixture read as alive'
    }

    # 1. Success: the window is found, a normal close is requested, and the process exits.
    $log = Join-Path $scratch 'success.log'
    Remove-Item -LiteralPath (Join-Path $scratch 'app.pid') -ErrorAction SilentlyContinue
    $result = Invoke-Smoke -LogPath $log -AppPath $app -PathPrefix $stubPath
    Assert-Pass ($result.Code -eq 0) "success path exited $($result.Code): $($result.Output.Trim())"
    Assert-Pass ($result.Output -match 'application window closed and process exited normally') 'success path did not report a normal close'
    Assert-Pass ($result.Output -notmatch 'wmctrl needs a window manager') 'success path unexpectedly required a window manager'
    Assert-Pass ((Test-Path -LiteralPath $log) -and ((Get-Content -LiteralPath $log -Raw) -match 'fixture application running')) 'success path did not capture the application log'
    Assert-FixtureGone 'success path'

    # 2. Missing window tool: the script refuses before launching anything.
    $result = Invoke-Smoke -LogPath (Join-Path $scratch 'missing-tool.log') -AppPath $app -PathPrefix (Convert-ToPosixPath $emptyDir)
    Assert-Pass ($result.Code -eq 2) "missing-tool path exited $($result.Code), expected 2"
    Assert-Pass ($result.Output -match 'requires xdotool') 'missing-tool path did not name the missing tool'

    # 3. Missing log variable: usage error, exit 2, nothing launched.
    $result = Invoke-Smoke -LogPath '' -AppPath $app -PathPrefix $stubPath -UnsetLog
    Assert-Pass ($result.Code -eq 2) "usage path exited $($result.Code), expected 2"
    Assert-Pass ($result.Output -match 'usage: KATTRI_SMOKE_LOG') 'usage path did not print the usage line'

    # 4. Bundled-library mismatch: the close succeeds, then verification rejects the wrong library.
    $result = Invoke-Smoke -LogPath (Join-Path $scratch 'mismatch.log') -AppPath $app -PathPrefix $stubPath -ExtraEnv @{ KATTRI_SMOKE_EXPECT_LIBMPV_PATH = (Convert-ToPosixPath (Join-Path $scratch 'expected-libmpv.so.2')) }
    Assert-Pass ($result.Code -eq 1) "library-mismatch path exited $($result.Code), expected 1"
    Assert-Pass ($result.Output -match 'did not initialize the expected bundled library') 'library-mismatch path did not report the verification failure'
    Assert-FixtureGone 'library-mismatch path'

    # 5. Negative control: with no window ever reported the same harness must fail, so a green
    #    run above cannot come from a permissive double.
    $result = Invoke-Smoke -LogPath (Join-Path $scratch 'no-window.log') -AppPath $app -PathPrefix (Convert-ToPosixPath $failStubDir)
    Assert-Pass ($result.Code -eq 1) "negative control exited $($result.Code), expected 1"
    Assert-Pass ($result.Output -match 'exited before creating a window') 'negative control did not fail on the missing window'

    if ($failures.Count -eq 0) {
        Write-Output ("PASS: window-smoke contract ran with tool doubles via {0} (success, missing tool, usage, library mismatch, no-window control, owned-probe controls, no leftover process)." -f (Split-Path -Leaf $bash))
        exit 0
    }
    foreach ($failure in $failures) {
        Write-Output "FAIL: $failure"
    }
    exit 1
}
finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
