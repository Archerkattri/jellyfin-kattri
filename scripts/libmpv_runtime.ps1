# Resolve an already installed Windows runtime without downloading or loading a DLL.
# Keep development runtimes outside target/: cargo clean owns that whole directory.

function Get-KattriPeMachine {
    param([Parameter(Mandatory = $true)][string] $Path)
    $reader = $null
    $stream = $null
    try {
        $stream = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        $reader = [System.IO.BinaryReader]::new($stream)
        if ($stream.Length -lt 64 -or $reader.ReadUInt16() -ne 0x5A4D) { return $null }
        $stream.Position = 0x3C
        $offset = $reader.ReadUInt32()
        if ($offset -lt 64 -or $offset -gt 1MB -or ($offset + 6L) -gt $stream.Length) {
            return $null
        }
        $stream.Position = $offset
        if ($reader.ReadUInt32() -ne 0x00004550) { return $null }
        return $reader.ReadUInt16()
    }
    catch { return $null }
    finally {
        if ($null -ne $reader) { $reader.Dispose() }
        elseif ($null -ne $stream) { $stream.Dispose() }
    }
}

function Find-KattriMpvInDirectory {
    param([string] $Directory, [int] $Machine)
    if (-not $Directory -or -not (Test-Path -LiteralPath $Directory -PathType Container)) {
        return $null
    }
    foreach ($name in @('mpv-2.dll', 'libmpv-2.dll', 'mpv.dll')) {
        $candidate = Join-Path $Directory $name
        if ((Test-Path -LiteralPath $candidate -PathType Leaf) -and
            (Get-KattriPeMachine -Path $candidate) -eq $Machine) {
            return [System.IO.Path]::GetFullPath($Directory)
        }
    }
    return $null
}

function Resolve-KattriLibmpvDirectory {
    param(
        [Parameter(Mandatory = $true)][string] $RepoRoot,
        [Parameter(Mandatory = $true)][string] $Executable
    )
    $machine = Get-KattriPeMachine -Path $Executable
    $architecture = switch ($machine) {
        0x8664 { 'windows-x64' }
        0xAA64 { 'windows-arm64' }
        default { return $null }
    }
    if ($env:KATTRI_LIBMPV_DIR) {
        $explicit = Find-KattriMpvInDirectory -Directory $env:KATTRI_LIBMPV_DIR -Machine $machine
        if (-not $explicit) {
            throw 'KATTRI_LIBMPV_DIR does not contain a Windows libmpv runtime matching the executable.'
        }
        return $explicit
    }
    $directories = [System.Collections.Generic.List[string]]::new()
    $directories.Add((Split-Path -Parent ([System.IO.Path]::GetFullPath($Executable))))
    $localRoot = Join-Path $RepoRoot '.local\libmpv'
    $directories.Add((Join-Path $localRoot $architecture))
    $directories.Add($localRoot)
    if (Test-Path -LiteralPath $localRoot -PathType Container) {
        foreach ($install in (Get-ChildItem -LiteralPath $localRoot -Directory | Sort-Object Name -Descending)) {
            $directories.Add((Join-Path $install.FullName 'dev'))
            $directories.Add($install.FullName)
        }
    }
    # Keep existing developer installations working while preferring durable storage.
    $directories.Add((Join-Path $RepoRoot "target\libmpv\$architecture"))
    foreach ($directory in $directories) {
        $found = Find-KattriMpvInDirectory -Directory $directory -Machine $machine
        if ($found) { return $found }
    }
    return $null
}
