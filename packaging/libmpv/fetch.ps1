# Downloads the pinned libmpv archive for one platform, verifies its SHA-256, and copies
# the shared library to target/libmpv/<platform>/. Run only when a build needs libmpv;
# a plain check never calls it.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging/libmpv/fetch.ps1 -Platform windows-x64
#
# Output: the path of the verified library, as the only pipeline output.
#
# Refuses (throws) when the platform is not pinned in manifest.toml, when a field is
# missing or malformed, when the URL is not https, or when the SHA-256 does not match.
# Nothing is extracted from an archive that fails verification.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet('windows-x64', 'windows-arm64', 'macos-universal', 'linux-x64')]
    [string]$Platform,

    [string]$ManifestPath
)

$ErrorActionPreference = 'Stop'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
if (-not $ManifestPath) {
    $ManifestPath = Join-Path $PSScriptRoot 'manifest.toml'
}

# Reads the flat string keys of [platforms.<Name>]. Other tables and comments are skipped.
function Read-LibmpvPin {
    param([string]$Path, [string]$Name)

    $section = $null
    $values = @{}
    foreach ($line in Get-Content -LiteralPath $Path -Encoding UTF8) {
        $trim = $line.Trim()
        if ($trim -eq '' -or $trim.StartsWith('#')) { continue }
        if ($trim -match '^\[([^\]]+)\]$') { $section = $Matches[1]; continue }
        if ($section -eq "platforms.$Name" -and $trim -match '^([a-z0-9_]+)\s*=\s*"([^"]*)"$') {
            $values[$Matches[1]] = $Matches[2]
        }
    }
    if ($values.Count -eq 0) {
        throw "manifest.toml has no [platforms.$Name] table; refusing."
    }
    return $values
}

$pin = Read-LibmpvPin -Path $ManifestPath -Name $Platform

if ($pin['status'] -ne 'pinned') {
    throw "libmpv for $Platform is not pinned: $($pin['todo'])"
}
foreach ($key in 'url', 'archive_name', 'sha256', 'library_member') {
    if (-not $pin[$key]) {
        throw "manifest.toml [platforms.$Platform] has no $key; refusing."
    }
}
if ($pin['url'] -notmatch '^https://') {
    throw "manifest.toml url for $Platform is not https; refusing."
}
if ($pin['sha256'] -notmatch '^[0-9a-f]{64}$') {
    throw "manifest.toml sha256 for $Platform is not 64 lowercase hex digits; refusing."
}
if ($pin['archive_name'] -notmatch '^[A-Za-z0-9._-]+$') {
    throw "manifest.toml archive_name for $Platform must be a bare file name; refusing."
}
if ($pin['library_member'] -match '(^/|\\|\.\.)') {
    throw "manifest.toml library_member for $Platform must be a relative path inside the archive; refusing."
}

$expected = $pin['sha256']
$outDir = Join-Path $repoRoot "target\libmpv\$Platform"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$archivePath = Join-Path $outDir $pin['archive_name']

function Get-Sha256Lower {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

if ((Test-Path -LiteralPath $archivePath) -and ((Get-Sha256Lower -Path $archivePath) -eq $expected)) {
    Write-Host "Using verified archive: $archivePath"
}
else {
    if (Test-Path -LiteralPath $archivePath) {
        Remove-Item -LiteralPath $archivePath -Force
    }
    $partPath = "$archivePath.part"
    # Windows PowerShell 5.1 defaults to older TLS versions; the download must use TLS 1.2 or later.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Write-Host "Downloading $($pin['archive_name'])"
    Invoke-WebRequest -Uri $pin['url'] -OutFile $partPath -UseBasicParsing

    $actual = Get-Sha256Lower -Path $partPath
    if ($actual -ne $expected) {
        Remove-Item -LiteralPath $partPath -Force
        throw "SHA-256 mismatch for $($pin['archive_name']): expected $expected, got $actual. Nothing was extracted."
    }
    Move-Item -LiteralPath $partPath -Destination $archivePath -Force
}

$stageDir = Join-Path $outDir ('extract-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stageDir | Out-Null
try {
    switch -Regex ($pin['archive_name']) {
        '\.zip$' {
            Expand-Archive -LiteralPath $archivePath -DestinationPath $stageDir -Force
        }
        '\.7z$' {
            $sevenZip = Get-Command 7z -ErrorAction SilentlyContinue
            if (-not $sevenZip) {
                throw '7z is required to extract a .7z archive; install 7-Zip and retry.'
            }
            & $sevenZip.Source x $archivePath "-o$stageDir" -y | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "7z extraction failed with exit code $LASTEXITCODE."
            }
        }
        default {
            throw "Unsupported archive format for $($pin['archive_name'])."
        }
    }

    $memberPath = Join-Path $stageDir ($pin['library_member'] -replace '/', '\')
    if (-not (Test-Path -LiteralPath $memberPath -PathType Leaf)) {
        throw "library_member '$($pin['library_member'])' was not found in $($pin['archive_name'])."
    }
    $libraryDest = Join-Path $outDir (Split-Path -Leaf $memberPath)
    Copy-Item -LiteralPath $memberPath -Destination $libraryDest -Force
}
finally {
    if (Test-Path -LiteralPath $stageDir) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force
    }
}

Write-Output $libraryDest
