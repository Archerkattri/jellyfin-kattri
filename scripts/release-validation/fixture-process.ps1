[CmdletBinding()]
param(
    [ValidateRange(65, 3600)]
    [int]$DurationSeconds = 75,

    [ValidateRange(8, 128)]
    [int]$MemoryMiB = 32
)

$ErrorActionPreference = 'Stop'
$bufferLength = [int]($MemoryMiB * 1MB)
$buffer = [Array]::CreateInstance([byte], $bufferLength)
for ($offset = 0; $offset -lt $buffer.Length; $offset += 4096) {
    $buffer[$offset] = 1
}

Start-Sleep -Seconds $DurationSeconds
