# Builds the portable (per-user) zip for Jellyfin Kattri and a SHA256 checksum file.
# Run once per target: -Platform x64 for the x64 zip, -Platform arm64 for the ARM64 zip.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging/windows/build-zip.ps1 `
#     -ExePath target/release/jellyfin-kattri.exe -Version 1.2.3 -Platform x64
#
# Parameters:
#   -ExePath      Release executable for the target platform. Required.
#   -Version      Version string written into README.txt and the file names. Required.
#   -Platform     x64 or arm64. Defaults to x64.
#   -OutputDir    Folder that receives the zip and checksum. Defaults to target/packaging/windows
#                 under the repository root.
#   -LicensePath  License files placed in the archive. Defaults to the repository LICENSE.
#
# The zip also carries the libmpv DLL verified by packaging/libmpv/fetch.ps1 and its notice.
# The pinned Windows archive has no license files, so the verified upstream source texts
# are included under licenses/libmpv.
#
# Output:
#   <OutputDir>/JellyfinKattri-<Version>-<Platform>-portable.zip
#   <OutputDir>/JellyfinKattri-<Version>-<Platform>-portable.zip.sha256
#
# The checksum file uses the sha256sum layout: "<hex digest>  <file name>".

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExePath,

    [Parameter(Mandatory = $true)]
    [string]$Version,

    [ValidateSet('x64', 'arm64')]
    [string]$Platform = 'x64',

    [string]$OutputDir,

    [string[]]$LicensePath
)

$ErrorActionPreference = 'Stop'

$productName = 'Jellyfin Kattri'
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')

if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot 'target\packaging\windows'
}
if (-not $LicensePath) {
    $LicensePath = @(Join-Path $repoRoot 'LICENSE')
}

. (Join-Path $PSScriptRoot 'pe-machine.ps1')

$exeFull = (Resolve-Path -LiteralPath $ExePath).Path
$licenseFiles = @($LicensePath | ForEach-Object { (Resolve-Path -LiteralPath $_).Path })

# Refuse a mismatched binary so the arm64 zip never carries an x64 executable.
Assert-PeMachine -Path $exeFull -Platform $Platform

# fetch.ps1 throws unless the pinned libmpv archive matches its SHA-256, so an
# unverified DLL cannot reach the zip. Until a build is pinned this step stops the build.
$libmpvDll = & (Join-Path $PSScriptRoot '..\libmpv\fetch.ps1') -Platform "windows-$Platform"
. (Join-Path $PSScriptRoot 'libmpv-licenses.ps1')
$libmpvLicenseFiles = @(Get-VerifiedLibmpvLicenseFiles)
$libmpvNotice = Get-LibmpvNoticePath -RepositoryRoot $repoRoot

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$outputFull = (Resolve-Path -LiteralPath $OutputDir).Path

$baseName = "JellyfinKattri-$Version-$Platform-portable"
$zipPath = Join-Path $outputFull "$baseName.zip"
$shaPath = "$zipPath.sha256"

# Stage the archive contents in a fresh temporary folder so the archive has a single
# top-level folder and no stray files from earlier runs.
$stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("kattri-zip-" + [guid]::NewGuid().ToString('N'))
$stageDir = Join-Path $stageRoot $baseName

try {
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null

    Copy-Item -LiteralPath $exeFull -Destination $stageDir
    Copy-Item -LiteralPath $libmpvDll -Destination $stageDir
    Copy-Item -LiteralPath $libmpvNotice -Destination (Join-Path $stageDir 'libmpv-NOTICE.txt')
    $nativeLicenseDir = Join-Path $stageDir 'licenses\libmpv'
    New-Item -ItemType Directory -Force -Path $nativeLicenseDir | Out-Null
    foreach ($license in $libmpvLicenseFiles) {
        Copy-Item -LiteralPath $license.Path -Destination (Join-Path $nativeLicenseDir $license.Name)
    }
    foreach ($license in $licenseFiles) {
        Copy-Item -LiteralPath $license -Destination $stageDir
    }

    # The README states where settings go, so a portable user is not surprised by the
    # profile write. The zip never writes into its own folder.
    $readmeLines = @(
        $productName,
        "Version $Version",
        "Platform $Platform",
        '',
        'Portable build: no installer. Run the executable from this folder.',
        'Settings are written to your Windows user profile, not to this folder.'
    )
    Set-Content -LiteralPath (Join-Path $stageDir 'README.txt') -Value $readmeLines -Encoding ascii

    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }
    Compress-Archive -LiteralPath $stageDir -DestinationPath $zipPath -CompressionLevel Optimal

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $package = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $expectedEntries = @(
            @{ Entry = "$baseName/$([System.IO.Path]::GetFileName($exeFull))"; Sha256 = (Get-FileHash -LiteralPath $exeFull -Algorithm SHA256).Hash.ToLowerInvariant() },
            @{ Entry = "$baseName/$([System.IO.Path]::GetFileName($libmpvDll))"; Sha256 = (Get-FileHash -LiteralPath $libmpvDll -Algorithm SHA256).Hash.ToLowerInvariant() },
            @{ Entry = "$baseName/libmpv-NOTICE.txt"; Sha256 = (Get-FileHash -LiteralPath $libmpvNotice -Algorithm SHA256).Hash.ToLowerInvariant() }
        )
        foreach ($license in $licenseFiles) {
            $expectedEntries += @{
                Entry = "$baseName/$([System.IO.Path]::GetFileName($license))"
                Sha256 = (Get-FileHash -LiteralPath $license -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
        foreach ($license in $libmpvLicenseFiles) {
            $expectedEntries += @{
                Entry = "$baseName/licenses/libmpv/$($license.Name)"
                Sha256 = $license.Sha256
            }
        }

        $sha = [System.Security.Cryptography.SHA256]::Create()
        try {
            foreach ($expected in $expectedEntries) {
                $entry = $package.GetEntry($expected.Entry)
                if ($null -eq $entry) {
                    throw "Portable zip is missing package file: $($expected.Entry)"
                }
                $stream = $entry.Open()
                try {
                    $actual = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
                }
                finally {
                    $stream.Dispose()
                }
                if ($actual -ne $expected.Sha256) {
                    throw "Portable zip payload hash mismatch: $($expected.Entry)"
                }
            }
        }
        finally {
            $sha.Dispose()
        }
    }
    finally {
        $package.Dispose()
    }
}
finally {
    if (Test-Path -LiteralPath $stageRoot) {
        Remove-Item -LiteralPath $stageRoot -Recurse -Force
    }
}

$hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
$zipName = Split-Path -Leaf $zipPath
Set-Content -LiteralPath $shaPath -Value "$hash  $zipName" -Encoding ascii

Write-Output "Built portable zip: $zipPath"
Write-Output "Wrote checksum: $shaPath"
