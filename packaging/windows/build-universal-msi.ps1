# Builds one Windows Installer package for Jellyfin Kattri that carries the x64 and the
# Windows on ARM payload and installs only the payload that matches the machine.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging/windows/build-universal-msi.ps1 `
#     -Version 1.2.3 `
#     -X64PayloadDir target/payload/windows-x64 `
#     -Arm64PayloadDir target/payload/windows-arm64 `
#     -OutputDir dist
#
# Parameters:
#   -Version          Numeric MSI version (major.minor.patch, optionally .build). Required.
#   -X64PayloadDir    Folder holding the x64 jellyfin-kattri.exe and libmpv-2.dll, laid out
#                     flat. Any further native file in that folder is installed too. Required.
#   -Arm64PayloadDir  The same layout for Windows on ARM. Required.
#   -OutputDir        Folder that receives the MSI and its checksum. Defaults to
#                     target/packaging/windows under the repository root.
#   -LicensePath      Project license installed next to the executable. Defaults to the
#                     repository LICENSE.
#   -IconPath         ICO used for the Add/Remove Programs entry and the Start menu shortcut.
#                     Defaults to packaging/windows/kattri.ico.
#
# Output:
#   <OutputDir>/JellyfinKattri-<Version>-universal.msi
#   <OutputDir>/JellyfinKattri-<Version>-universal.msi.sha256
#
# Auto-architecture behaviour:
#   The MSI is built once and installs only the payload that matches the machine.
#   packaging/windows/wix/kattri-universal.wxs reads PROCESSOR_ARCHITECTURE from the environment
#   key under HKLM\SYSTEM\CurrentControlSet\Control\Session Manager into KATTRI_ARM64 and gives
#   the x64 payload components the condition KATTRI_ARM64<>"ARM64" and the arm64 payload
#   components the condition KATTRI_ARM64="ARM64". This script checks the PE machine of both
#   executables and of both libmpv DLLs before WiX runs, so a swapped or mislabelled payload
#   folder fails here instead of producing a package that installs the wrong architecture.
#
#   The MSI is compiled for the WiX x64 package platform: an x64 package installs on x64 and on
#   Windows on ARM, while an arm64 package is refused on x64 hardware. The script never
#   downloads anything; the payload folders must already hold the verified libmpv files. The
#   libmpv notice and license texts are read from the repository and hash-checked, exactly as
#   packaging/windows/build-msi.ps1 does.
#
# Requires the WiX v4 CLI (wix) on PATH, as packaging/windows/build-msi.ps1 requires it.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$X64PayloadDir,

    [Parameter(Mandatory = $true)]
    [string]$Arm64PayloadDir,

    [string]$OutputDir,

    [string]$LicensePath,

    [string]$IconPath
)

$ErrorActionPreference = 'Stop'

$expectedExecutableName = 'jellyfin-kattri.exe'
$expectedLibmpvName = 'libmpv-2.dll'

# The two payload conditions are also written into the generated extra-payload fragment, so the
# extra native files carry the same mutually exclusive conditions as the executable and the DLL.
$x64PayloadCondition = 'KATTRI_ARM64&lt;&gt;&quot;ARM64&quot;'
$arm64PayloadCondition = 'KATTRI_ARM64=&quot;ARM64&quot;'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$wxsPath = Join-Path $PSScriptRoot 'wix\kattri-universal.wxs'

if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot 'target\packaging\windows'
}
if (-not $LicensePath) {
    $LicensePath = Join-Path $repoRoot 'LICENSE'
}
if (-not $IconPath) {
    $IconPath = Join-Path $PSScriptRoot 'kattri.ico'
}

# Windows Installer accepts only numeric versions with up to four parts.
if ($Version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$') {
    throw "Version '$Version' is not a numeric MSI version (expected major.minor.patch, optionally .build)."
}

foreach ($requiredFile in @($wxsPath, $LicensePath, $IconPath)) {
    if (-not (Test-Path -LiteralPath $requiredFile -PathType Leaf)) {
        throw "Required packaging file was not found: $requiredFile"
    }
}

. (Join-Path $PSScriptRoot 'pe-machine.ps1')

# Reads one payload folder and refuses anything that cannot be the requested platform.
function Get-KattriPayload {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$Directory,

        [Parameter(Mandatory = $true)]
        [ValidateSet('x64', 'arm64')]
        [string]$Platform
    )

    if (-not (Test-Path -LiteralPath $Directory -PathType Container)) {
        throw "The $Platform payload directory was not found: $Directory"
    }
    $payloadRoot = (Resolve-Path -LiteralPath $Directory).Path

    $executablePath = Join-Path $payloadRoot $expectedExecutableName
    if (-not (Test-Path -LiteralPath $executablePath -PathType Leaf)) {
        throw "The $Platform payload directory has no $expectedExecutableName : $executablePath"
    }
    $libmpvPath = Join-Path $payloadRoot $expectedLibmpvName
    if (-not (Test-Path -LiteralPath $libmpvPath -PathType Leaf)) {
        throw "The $Platform payload directory has no $expectedLibmpvName : $libmpvPath"
    }

    # Refuse a mismatched binary before WiX runs, so an x64 file cannot become the arm64 payload.
    Assert-PeMachine -Path $executablePath -Platform $Platform
    $libmpvMachine = Get-PeMachine -Path $libmpvPath
    if ($null -eq $libmpvMachine) {
        throw "$libmpvPath is not a PE image; expected a $Platform DLL."
    }
    if ($libmpvMachine -ne $PeMachineByPlatform[$Platform]) {
        throw ("{0} has PE machine 0x{1:X4}; expected 0x{2:X4} for platform {3}." -f $libmpvPath, $libmpvMachine, $PeMachineByPlatform[$Platform], $Platform)
    }

    $extraFiles = @(Get-ChildItem -LiteralPath $payloadRoot -File |
        Where-Object { $_.Name -ne $expectedExecutableName -and $_.Name -ne $expectedLibmpvName } |
        Sort-Object -Property Name)

    # Progress only, so the pipeline carries the payload object alone.
    Write-Host "Payload $Platform : $payloadRoot ($($extraFiles.Count) extra native file(s))"

    return [pscustomobject]@{
        Platform   = $Platform
        Directory  = $payloadRoot
        Executable = (Resolve-Path -LiteralPath $executablePath).Path
        Libmpv     = (Resolve-Path -LiteralPath $libmpvPath).Path
        ExtraFiles = $extraFiles
    }
}

$x64Payload = Get-KattriPayload -Directory $X64PayloadDir -Platform 'x64'
$arm64Payload = Get-KattriPayload -Directory $Arm64PayloadDir -Platform 'arm64'

$wix = Get-Command wix -ErrorAction SilentlyContinue
if (-not $wix) {
    throw 'The WiX v4 CLI (wix) was not found on PATH. Install WiX v4 and retry.'
}

$licenseFull = (Resolve-Path -LiteralPath $LicensePath).Path
$iconFull = (Resolve-Path -LiteralPath $IconPath).Path

# The pinned libmpv builds ship no license texts, so the verified upstream texts are installed
# next to the executable. This fails loudly if a pinned text changed or is missing.
$libmpvLicenseDirectory = Join-Path $PSScriptRoot 'libmpv-licenses'
. (Join-Path $PSScriptRoot 'libmpv-licenses.ps1')
$null = @(Get-VerifiedLibmpvLicenseFiles)
$libmpvNoticePath = Get-LibmpvNoticePath -RepositoryRoot $repoRoot

# Escapes a value for use inside a WiX XML attribute.
function ConvertTo-WixAttributeValue {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Value
    )

    return [System.Security.SecurityElement]::Escape($Value)
}

# Writes one conditioned ComponentGroup for the native files of a payload folder that are
# neither the executable nor libmpv-2.dll. Their names are only known at build time, so they
# cannot be declared in the static WiX source.
function New-ExtraPayloadFragment {
    param(
        [Parameter(Mandatory = $true)]
        [psobject]$Payload,

        [Parameter(Mandatory = $true)]
        [string]$GroupId,

        [Parameter(Mandatory = $true)]
        [string]$ArchToken,

        [Parameter(Mandatory = $true)]
        [string]$Condition
    )

    $builder = New-Object System.Text.StringBuilder
    $null = $builder.AppendLine(('    <ComponentGroup Id="{0}" Directory="INSTALLFOLDER">' -f $GroupId))
    $index = 0
    foreach ($extraFile in $Payload.ExtraFiles) {
        $index++
        $componentId = '{0}ExtraPayloadComponent{1}' -f $ArchToken, $index
        $fileId = '{0}ExtraPayloadFile{1}' -f $ArchToken, $index
        $source = ConvertTo-WixAttributeValue -Value $extraFile.FullName
        $name = ConvertTo-WixAttributeValue -Value $extraFile.Name
        $null = $builder.AppendLine(('      <Component Id="{0}" Guid="*" Directory="INSTALLFOLDER" Condition="{1}">' -f $componentId, $Condition))
        $null = $builder.AppendLine(('        <File Id="{0}" Source="{1}" Name="{2}" KeyPath="yes" />' -f $fileId, $source, $name))
        $null = $builder.AppendLine('      </Component>')
    }
    $null = $builder.AppendLine('    </ComponentGroup>')

    return $builder.ToString()
}

$hasX64ExtraPayload = @($x64Payload.ExtraFiles).Count -gt 0
$hasArm64ExtraPayload = @($arm64Payload.ExtraFiles).Count -gt 0

$stageRoot = $null
$extraFragmentPath = $null

try {
    if ($hasX64ExtraPayload -or $hasArm64ExtraPayload) {
        # The generated fragment lives in a temporary folder so no build product lands in the
        # source tree or in OutputDir, which the release job uploads as artifacts.
        $stageRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('kattri-universal-msi-' + [guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Force -Path $stageRoot | Out-Null
        $extraFragmentPath = Join-Path $stageRoot 'kattri-universal-extra-payload.wxs'

        $fragments = @()
        if ($hasX64ExtraPayload) {
            $fragments += New-ExtraPayloadFragment -Payload $x64Payload -GroupId 'X64ExtraPayloadComponents' -ArchToken 'X64' -Condition $x64PayloadCondition
        }
        if ($hasArm64ExtraPayload) {
            $fragments += New-ExtraPayloadFragment -Payload $arm64Payload -GroupId 'Arm64ExtraPayloadComponents' -ArchToken 'Arm64' -Condition $arm64PayloadCondition
        }

        $fragmentLines = @(
            '<?xml version="1.0" encoding="UTF-8"?>'
            '<!-- Generated by packaging/windows/build-universal-msi.ps1. Declares the native payload'
            '     files that are neither jellyfin-kattri.exe nor libmpv-2.dll for one architecture.'
            '     Every run regenerates this file; do not edit it. -->'
            '<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">'
            '  <Fragment>'
        ) + $fragments + @(
            '  </Fragment>'
            '</Wix>'
        )
        Set-Content -LiteralPath $extraFragmentPath -Value $fragmentLines -Encoding UTF8
    }

    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    $outputFull = (Resolve-Path -LiteralPath $OutputDir).Path
    $msiName = "JellyfinKattri-$Version-universal.msi"
    $msiPath = Join-Path $outputFull $msiName
    if (Test-Path -LiteralPath $msiPath) {
        Remove-Item -LiteralPath $msiPath -Force
    }

    $buildArguments = @(
        'build',
        '-arch', 'x64',
        '-d', "Version=$Version",
        '-d', "ExeName=$expectedExecutableName",
        '-d', "X64ExePath=$($x64Payload.Executable)",
        '-d', "X64LibmpvPath=$($x64Payload.Libmpv)",
        '-d', "Arm64ExePath=$($arm64Payload.Executable)",
        '-d', "Arm64LibmpvPath=$($arm64Payload.Libmpv)",
        '-d', "LicensePath=$licenseFull",
        '-d', "LibmpvNoticePath=$libmpvNoticePath",
        '-d', "LibmpvLicenseDirectory=$libmpvLicenseDirectory",
        '-d', "IconPath=$iconFull",
        '-d', "X64ExtraPayload=$([int]$hasX64ExtraPayload)",
        '-d', "Arm64ExtraPayload=$([int]$hasArm64ExtraPayload)",
        '-out', $msiPath
    )

    $sources = @($wxsPath)
    if ($extraFragmentPath) {
        $sources += $extraFragmentPath
    }

    & $wix.Source @buildArguments @sources

    if ($LASTEXITCODE -ne 0) {
        throw "wix build failed with exit code $LASTEXITCODE."
    }
    if (-not (Test-Path -LiteralPath $msiPath -PathType Leaf)) {
        throw "wix build reported success but produced no MSI: $msiPath"
    }

    $hash = (Get-FileHash -LiteralPath $msiPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$msiPath.sha256" -Value "$hash  $msiName" -Encoding ascii

    Write-Output "Built universal MSI: $msiPath"
    Write-Output "Wrote checksum: $msiPath.sha256"
}
finally {
    if ($stageRoot -and (Test-Path -LiteralPath $stageRoot)) {
        Remove-Item -LiteralPath $stageRoot -Recurse -Force
    }
}
