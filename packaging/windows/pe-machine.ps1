# Shared helper for the Windows packaging scripts. Dot-source it; it has no side effects.
#
# Reads the COFF machine field of a PE file so a package cannot carry an x64 binary under
# the arm64 label, or the reverse. Returns the machine value, or $null when the file is
# not a PE image.

$PeMachineByPlatform = @{
    x64   = 0x8664
    arm64 = 0xAA64
}

function Get-PeMachine {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path
    )

    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $dosHeader = New-Object byte[] 64
        if ($stream.Read($dosHeader, 0, 64) -lt 64) { return $null }
        # 'MZ' signature, then the offset of the PE header at 0x3C.
        if ($dosHeader[0] -ne 0x4D -or $dosHeader[1] -ne 0x5A) { return $null }
        $peOffset = [BitConverter]::ToInt32($dosHeader, 0x3C)

        $stream.Position = $peOffset
        $peHeader = New-Object byte[] 6
        if ($stream.Read($peHeader, 0, 6) -lt 6) { return $null }
        # 'PE' followed by two zero bytes; the machine field is the next two bytes.
        if ($peHeader[0] -ne 0x50 -or $peHeader[1] -ne 0x45 -or $peHeader[2] -ne 0 -or $peHeader[3] -ne 0) {
            return $null
        }
        return [BitConverter]::ToUInt16($peHeader, 4)
    }
    finally {
        $stream.Dispose()
    }
}

function Assert-PeMachine {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [ValidateSet('x64', 'arm64')]
        [string]$Platform
    )

    $actual = Get-PeMachine -Path $Path
    $expected = $PeMachineByPlatform[$Platform]
    if ($null -eq $actual) {
        throw "$Path is not a PE image; expected a $Platform executable."
    }
    if ($actual -ne $expected) {
        throw ("{0} has PE machine 0x{1:X4}; expected 0x{2:X4} for platform {3}." -f $Path, $actual, $expected, $Platform)
    }
}
