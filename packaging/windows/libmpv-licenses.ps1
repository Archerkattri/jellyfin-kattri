# Exact license and notice files selected from the source revisions used by the pinned
# Windows libmpv build. The 2026-10-23 release archives contain only headers, an import
# library, and libmpv-2.dll; these texts are not present in either .7z archive.
#
# mpv: fa0534a729dd3dff33ea03cc7c226d317a4fcd07, LICENSE.LGPL and Copyright
# FFmpeg: 179d08349347e00b2a24593cfa8b3cbbde2a6cfb, COPYING.LGPLv3 and LICENSE.md

$script:LibmpvLicensePayload = @(
    @{ Name = 'LICENSE.LGPL'; Sha256 = '72b672113d642cbb8ef5dcc76938db801983c56e50b1400ab930f1a64d6dc8d9' },
    @{ Name = 'Copyright'; Sha256 = 'bfe9ee4cceabcb8ecbfadf208d04156f73d801e6a57369a5606bb8341e204a23' },
    @{ Name = 'COPYING.LGPLv3'; Sha256 = 'da7eabb7bafdf7d3ae5e9f223aa5bdc1eece45ac569dc21b3b037520b4464768' },
    @{ Name = 'LICENSE.md'; Sha256 = 'e6dc72d5665a1ebd6dcc841de59278dad4ca77ecc47b03f1064a55c3e9ea8bb8' }
)

function Get-LibmpvManifestPin {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepositoryRoot,

        [Parameter(Mandatory = $true)]
        [string]$Platform
    )

    $manifest = Join-Path $RepositoryRoot 'packaging\libmpv\manifest.toml'
    $inSection = $false
    $values = @{}
    foreach ($line in Get-Content -LiteralPath $manifest -Encoding UTF8) {
        $trimmed = $line.Trim()
        if ($trimmed -match '^\[[^\]]+\]$') {
            $inSection = $trimmed -eq "[platforms.$Platform]"
            continue
        }
        if ($inSection -and $trimmed -match '^([a-z0-9_]+)\s*=\s*"([^"]*)"$') {
            $values[$Matches[1]] = $Matches[2]
        }
    }

    foreach ($key in @('status', 'libmpv_version', 'url', 'sha256', 'library_member', 'license')) {
        if (-not $values.ContainsKey($key) -or [string]::IsNullOrWhiteSpace($values[$key])) {
            throw "Pinned libmpv manifest is missing '$key' for $Platform."
        }
    }
    if ($values['status'] -ne 'pinned' -or $values['sha256'] -notmatch '^[0-9a-f]{64}$') {
        throw "Pinned libmpv manifest entry is not verifiable for $Platform."
    }
    return [pscustomobject]$values
}

function Get-VerifiedLibmpvLicenseFiles {
    [CmdletBinding()]
    param()

    $licenseDirectory = Join-Path $PSScriptRoot 'libmpv-licenses'
    foreach ($entry in $script:LibmpvLicensePayload) {
        $path = Join-Path $licenseDirectory $entry.Name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Pinned libmpv license file was not found: $path"
        }
        $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $entry.Sha256) {
            throw "Pinned libmpv license file hash mismatch: $($entry.Name)"
        }
        [pscustomobject]@{
            Name = $entry.Name
            Path = $path
            Sha256 = $actual
        }
    }
}

function Get-LibmpvNoticePath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepositoryRoot
    )

    $path = Join-Path $RepositoryRoot 'packaging\libmpv\NOTICE.txt'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "libmpv notice was not found: $path"
    }
    return (Resolve-Path -LiteralPath $path).Path
}
