# Signs one file with signtool when a signing reference is configured, and reports the
# artifact as unsigned when it is not.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging/windows/sign.ps1 -Path dist/app.msi
#
# Environment variables:
#   KATTRI_SIGNING_SERVICE
#       Default name of the variable that holds the signing reference. The script reads
#       the variable named by -ReferenceVariable, which defaults to this name. Its value
#       is a list of signtool selector arguments separated by spaces, with no key
#       material. Examples:
#         /dlib <path to signing client DLL> /dmdf <path to metadata file>
#         /sha1 <certificate thumbprint>   (certificate already present in a store)
#       Paths in the value must not contain spaces.
#   The private key never appears here. The signing key lives on an offline hardware
#   token, and the release job only ever receives the external service reference.
#
# Behaviour:
#   - Reference variable unset or empty: prints an UNSIGNED line naming the file and exits 0.
#   - Reference variable set and signtool found: signs with SHA-256 and a timestamp, then
#     verifies the signature with signtool verify /pa. Any failure exits non-zero.
#   - Reference variable set and signtool missing: exits non-zero, because a configured
#     signing step must not be skipped silently.
#
# Parameters:
#   -Path               File to sign. Required.
#   -ReferenceVariable  Name of the environment variable holding the signing reference.
#                       Defaults to KATTRI_SIGNING_SERVICE.
#   -TimestampUrl       RFC 3161 timestamp server used when signing.

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Path,

    [string]$ReferenceVariable = 'KATTRI_SIGNING_SERVICE',

    [string]$TimestampUrl = 'http://timestamp.digicert.com'
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    throw "File to sign was not found: $Path"
}
$fullPath = (Resolve-Path -LiteralPath $Path).Path
$displayName = Split-Path -Leaf $fullPath

$reference = [Environment]::GetEnvironmentVariable($ReferenceVariable)
if ([string]::IsNullOrWhiteSpace($reference)) {
    Write-Output "UNSIGNED: $displayName was not signed because $ReferenceVariable is not set."
    exit 0
}

$signtool = Get-Command signtool -ErrorAction SilentlyContinue
if (-not $signtool) {
    throw 'signtool was not found on PATH, but a signing reference is configured. Install the Windows SDK signing tools.'
}

$selectorArgs = @($reference -split '\s+' | Where-Object { $_ -ne '' })

$signArgs = @('sign', '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256') + $selectorArgs + @($fullPath)
& $signtool.Source @signArgs
if ($LASTEXITCODE -ne 0) {
    throw "signtool sign failed for $displayName with exit code $LASTEXITCODE."
}

& $signtool.Source verify /pa /v $fullPath
if ($LASTEXITCODE -ne 0) {
    throw "signtool verify failed for $displayName with exit code $LASTEXITCODE."
}

Write-Output "SIGNED: $displayName"
