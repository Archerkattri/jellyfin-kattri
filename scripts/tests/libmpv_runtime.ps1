$ErrorActionPreference = 'Stop'
. (Join-Path (Split-Path -Parent $PSScriptRoot) 'libmpv_runtime.ps1')

function Assert-Runtime([bool] $Condition, [string] $Message) {
    if (-not $Condition) { throw $Message }
}

function Write-PeFixture([string] $Path, [int] $Machine) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Path) | Out-Null
    $bytes = [byte[]]::new(128)
    $bytes[0] = 0x4D; $bytes[1] = 0x5A; $bytes[0x3C] = 0x40
    $bytes[0x40] = 0x50; $bytes[0x41] = 0x45
    [BitConverter]::GetBytes([uint16]$Machine).CopyTo($bytes, 0x44)
    [System.IO.File]::WriteAllBytes($Path, $bytes)
}

$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ('kattri-runtime-' + [guid]::NewGuid().ToString('N'))
$priorOverride = $env:KATTRI_LIBMPV_DIR
try {
    $env:KATTRI_LIBMPV_DIR = $null
    $exe = Join-Path $scratch 'target\debug\jellyfin-kattri.exe'
    Write-PeFixture $exe 0x8664
    $wrong = Join-Path $scratch '.local\libmpv\windows-x64\libmpv-2.dll'
    Write-PeFixture $wrong 0xAA64
    Assert-Runtime ($null -eq (Resolve-KattriLibmpvDirectory $scratch $exe)) 'Wrong-architecture runtime must be refused.'

    $durable = Join-Path $scratch '.local\libmpv\verified-install\dev'
    Write-PeFixture (Join-Path $durable 'libmpv-2.dll') 0x8664
    Assert-Runtime ((Resolve-KattriLibmpvDirectory $scratch $exe) -eq $durable) 'Durable runtime must resolve without target/libmpv.'
    $legacy = Join-Path $scratch 'target\libmpv\windows-x64'
    Write-PeFixture (Join-Path $legacy 'mpv-2.dll') 0x8664
    Assert-Runtime ((Resolve-KattriLibmpvDirectory $scratch $exe) -eq $durable) 'Durable runtime should precede legacy target storage.'

    $bundled = Split-Path -Parent $exe
    Write-PeFixture (Join-Path $bundled 'mpv.dll') 0x8664
    Assert-Runtime ((Resolve-KattriLibmpvDirectory $scratch $exe) -eq $bundled) 'Bundled runtime should take precedence.'

    $env:KATTRI_LIBMPV_DIR = $legacy
    Assert-Runtime ((Resolve-KattriLibmpvDirectory $scratch $exe) -eq $legacy) 'Explicit override should take precedence.'
    $env:KATTRI_LIBMPV_DIR = Split-Path -Parent $wrong
    $refused = $false
    try { Resolve-KattriLibmpvDirectory $scratch $exe | Out-Null } catch { $refused = $true }
    Assert-Runtime $refused 'Invalid explicit override must fail instead of silently selecting another runtime.'

    $env:KATTRI_LIBMPV_DIR = $null
    $armExe = Join-Path $scratch 'target\arm64\jellyfin-kattri.exe'
    Write-PeFixture $armExe 0xAA64
    Assert-Runtime ((Resolve-KattriLibmpvDirectory $scratch $armExe) -eq (Split-Path -Parent $wrong)) 'Executable architecture, not PowerShell architecture, chooses the runtime.'
    $malformed = Join-Path $scratch 'bad.dll'
    [System.IO.File]::WriteAllBytes($malformed, [byte[]]::new(12))
    Assert-Runtime ($null -eq (Get-KattriPeMachine $malformed)) 'Truncated PE must be refused.'
    Write-Output 'PASS: durable runtime resolution, precedence, explicit override, architecture isolation, malformed PE.'
}
finally {
    $env:KATTRI_LIBMPV_DIR = $priorOverride
    # A uniquely named test directory; verify its parent before removing fixture files.
    $resolvedScratch = [System.IO.Path]::GetFullPath($scratch)
    $tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd('\')
    if ((Split-Path -Parent $resolvedScratch) -eq $tempRoot -and
        (Split-Path -Leaf $resolvedScratch).StartsWith('kattri-runtime-')) {
        Remove-Item -LiteralPath $resolvedScratch -Recurse -Force
    }
}
