# Builds the per-user MSI for Jellyfin Kattri with the WiX v4 CLI.
# The MSI installs for the current user under %LOCALAPPDATA% and needs no elevation (D11).
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging/windows/build-msi.ps1 `
#     -ExePath target/release/jellyfin-kattri.exe -Version 1.2.3 -Platform x64
#
# Parameters:
#   -ExePath      Release executable for the target platform. Required.
#   -Version      Numeric MSI version (major.minor.patch, optionally .build). Required.
#   -Platform     x64 or arm64. Defaults to x64. Selects the WiX -arch value; the arm64
#                 build must use an arm64 executable built on or for Windows on ARM.
#   -OutputDir    Folder that receives the MSI. Defaults to target/packaging/windows
#                 under the repository root, which is covered by the existing target/ ignore rule.
#   -LicensePath  License file installed next to the executable. Defaults to the repository LICENSE.
#
# Output: <OutputDir>/JellyfinKattri-<Version>-<Platform>.msi
#
# Requires the WiX v4 CLI (wix.exe) on PATH. The script stops if it is missing.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath,

    [Parameter(Mandatory = $true)]
    [string]$Version,

    [ValidateSet('x64', 'arm64')]
    [string]$Platform = 'x64',

    [string]$OutputDir,

    [string]$LicensePath
)

$ErrorActionPreference = 'Stop'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$wxsPath = Join-Path $PSScriptRoot 'wix\kattri.wxs'

if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot 'target\packaging\windows'
}
if (-not $LicensePath) {
    $LicensePath = Join-Path $repoRoot 'LICENSE'
}

# Windows Installer accepts only numeric versions with up to four parts.
if ($Version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$') {
    throw "Version '$Version' is not a numeric MSI version (expected major.minor.patch, optionally .build)."
}

. (Join-Path $PSScriptRoot 'pe-machine.ps1')

$exeFull = (Resolve-Path -LiteralPath $ExePath).Path
$licenseFull = (Resolve-Path -LiteralPath $LicensePath).Path
$exeName = Split-Path -Leaf $exeFull

# Refuse a mismatched binary before WiX runs, so an x64 exe cannot become an arm64 MSI.
Assert-PeMachine -Path $exeFull -Platform $Platform

$wix = Get-Command wix -ErrorAction SilentlyContinue
if (-not $wix) {
    throw 'The WiX v4 CLI (wix) was not found on PATH. Install WiX v4 and retry.'
}

# The verified libmpv DLL is passed to WiX as LibmpvPath. kattri.wxs does not install it yet,
# so the MSI does not contain libmpv until that file adds a component for the define.
$libmpvDll = & (Join-Path $PSScriptRoot '..\libmpv\fetch.ps1') -Platform "windows-$Platform"

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$msiPath = Join-Path (Resolve-Path $OutputDir).Path "JellyfinKattri-$Version-$Platform.msi"

$arch = $Platform
& $wix.Source build `
    -arch $arch `
    -d "Version=$Version" `
    -d "ExePath=$exeFull" `
    -d "ExeName=$exeName" `
    -d "LicensePath=$licenseFull" `
    -d "LibmpvPath=$libmpvDll" `
    -out $msiPath `
    $wxsPath

if ($LASTEXITCODE -ne 0) {
    throw "wix build failed with exit code $LASTEXITCODE."
}

Write-Output "Built MSI: $msiPath"
