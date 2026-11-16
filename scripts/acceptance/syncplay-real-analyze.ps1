[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$TraceA,

    [Parameter(Mandatory = $true)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$TraceB,

    [Parameter(Mandatory = $true)]
    [ValidateRange(0, 9223372036854775807)]
    [long]$WindowStartUnixMs,

    [ValidateRange(600, 3600)]
    [int]$WindowDurationSeconds = 600,

    [ValidateRange(1, 5000)]
    [int]$DriftThresholdMilliseconds = 500,

    [ValidateRange(50, 1000)]
    [int]$ComparisonIntervalMilliseconds = 250
)

$ErrorActionPreference = 'Stop'

function Import-PositionTrace {
    param([string]$Path)

    try {
        $inputRows = @(Import-Csv -LiteralPath $Path -ErrorAction Stop)
    }
    catch {
        throw 'A position trace could not be read.'
    }
    if ($inputRows.Count -lt 2) { throw 'A position trace has too few samples.' }
    $requiredColumns = @('utc_unix_ms', 'monotonic_ms', 'position_ticks', 'paused', 'buffering', 'player_error')
    foreach ($column in $requiredColumns) {
        if ($null -eq $inputRows[0].PSObject.Properties[$column]) {
            throw 'A position trace has an unexpected schema.'
        }
    }

    $rows = [System.Collections.Generic.List[object]]::new()
    foreach ($row in $inputRows) {
        [long]$utc = 0
        [long]$monotonic = 0
        [long]$position = 0
        [bool]$paused = $false
        [bool]$buffering = $false
        [bool]$playerError = $false
        if (-not [long]::TryParse([string]$row.utc_unix_ms, [ref]$utc) -or
            -not [long]::TryParse([string]$row.monotonic_ms, [ref]$monotonic) -or
            -not [long]::TryParse([string]$row.position_ticks, [ref]$position) -or
            $position -lt 0 -or
            -not [bool]::TryParse([string]$row.paused, [ref]$paused) -or
            -not [bool]::TryParse([string]$row.buffering, [ref]$buffering) -or
            -not [bool]::TryParse([string]$row.player_error, [ref]$playerError)) {
            throw 'A position trace contains a malformed numeric or boolean sample.'
        }
        $rows.Add([pscustomobject]@{
            utc_unix_ms = $utc
            monotonic_ms = $monotonic
            position_ticks = $position
            paused = $paused
            buffering = $buffering
            player_error = $playerError
        })
    }
    $rows.Sort([System.Comparison[object]]{
        param($left, $right)
        $left.utc_unix_ms.CompareTo($right.utc_unix_ms)
    })
    for ($index = 1; $index -lt $rows.Count; $index++) {
        if ($rows[$index].utc_unix_ms -le $rows[$index - 1].utc_unix_ms) {
            throw 'A position trace has duplicate or non-monotonic UTC samples.'
        }
    }
    return ,$rows.ToArray()
}

function Get-TracePositionAt {
    param(
        [object[]]$Rows,
        [long]$Timestamp,
        [int]$MaxInterpolationGapMs = 1000
    )

    $low = 0
    $high = $Rows.Count - 1
    while ($low -le $high) {
        $middle = [int][Math]::Floor(($low + $high) / 2)
        $at = [long]$Rows[$middle].utc_unix_ms
        if ($at -eq $Timestamp) { return [double]$Rows[$middle].position_ticks }
        if ($at -lt $Timestamp) { $low = $middle + 1 } else { $high = $middle - 1 }
    }
    $rightIndex = $low
    $leftIndex = $rightIndex - 1
    if ($leftIndex -lt 0 -or $rightIndex -ge $Rows.Count) { return $null }
    $left = $Rows[$leftIndex]
    $right = $Rows[$rightIndex]
    $gap = [long]$right.utc_unix_ms - [long]$left.utc_unix_ms
    if ($gap -le 0 -or $gap -gt $MaxInterpolationGapMs) { return $null }
    $fraction = ($Timestamp - [long]$left.utc_unix_ms) / [double]$gap
    return ([double]$left.position_ticks +
        ([double]$right.position_ticks - [double]$left.position_ticks) * $fraction)
}

function Get-TraceStats {
    param([object[]]$Rows)

    $maxGap = 0L
    $gapsOver500Ms = 0
    $gapsOver1000Ms = 0
    for ($index = 1; $index -lt $Rows.Count; $index++) {
        $gap = [long]$Rows[$index].utc_unix_ms - [long]$Rows[$index - 1].utc_unix_ms
        if ($gap -gt $maxGap) { $maxGap = $gap }
        if ($gap -gt 500) { $gapsOver500Ms++ }
        if ($gap -gt 1000) { $gapsOver1000Ms++ }
    }
    return [pscustomobject]@{
        samples = $Rows.Count
        max_gap_ms = $maxGap
        gaps_over_500_ms = $gapsOver500Ms
        gaps_over_1000_ms = $gapsOver1000Ms
        paused_samples = @($Rows | Where-Object paused).Count
        buffering_samples = @($Rows | Where-Object buffering).Count
        player_error_samples = @($Rows | Where-Object player_error).Count
    }
}

$rowsA = Import-PositionTrace -Path $TraceA
$rowsB = Import-PositionTrace -Path $TraceB
$windowEnd = $WindowStartUnixMs + ([long]$WindowDurationSeconds * 1000)
$traceStart = [Math]::Max([long]$rowsA[0].utc_unix_ms, [long]$rowsB[0].utc_unix_ms)
$traceEnd = [Math]::Min([long]$rowsA[-1].utc_unix_ms, [long]$rowsB[-1].utc_unix_ms)
if ($traceStart -gt $WindowStartUnixMs -or $traceEnd -lt $windowEnd) {
    throw 'The two position traces do not cover the requested full measurement window.'
}

$thresholdTicks = [double]$DriftThresholdMilliseconds * 10000.0
$drifts = [System.Collections.Generic.List[double]]::new()
$missingPairs = 0
for ($timestamp = $WindowStartUnixMs; $timestamp -le $windowEnd; $timestamp += $ComparisonIntervalMilliseconds) {
    $positionA = Get-TracePositionAt -Rows $rowsA -Timestamp $timestamp
    $positionB = Get-TracePositionAt -Rows $rowsB -Timestamp $timestamp
    if ($null -eq $positionA -or $null -eq $positionB) {
        $missingPairs++
        continue
    }
    $drifts.Add([Math]::Abs($positionA - $positionB))
}
if ($drifts.Count -eq 0) { throw 'No comparable position samples were available.' }

$driftMs = @($drifts | ForEach-Object { $_ / 10000.0 } | Sort-Object)
$maxDriftMs = $driftMs[-1]
$p50Index = [int][Math]::Max(0, [Math]::Ceiling($driftMs.Count * 0.50) - 1)
$p95Index = [int][Math]::Max(0, [Math]::Ceiling($driftMs.Count * 0.95) - 1)
$outliers = @($driftMs | Where-Object { $_ -gt $DriftThresholdMilliseconds }).Count
$statsA = Get-TraceStats -Rows $rowsA
$statsB = Get-TraceStats -Rows $rowsB
$longestGap = [Math]::Max($statsA.max_gap_ms, $statsB.max_gap_ms)
$errorSamples = $statsA.player_error_samples + $statsB.player_error_samples
$withinThreshold = $outliers -eq 0 -and $missingPairs -eq 0
$complete = $statsA.gaps_over_1000_ms -eq 0 -and $statsB.gaps_over_1000_ms -eq 0

Write-Output "WINDOW_SECONDS=$WindowDurationSeconds; comparison_interval_ms=$ComparisonIntervalMilliseconds; pairs=$($drifts.Count); missing_pairs=$missingPairs"
Write-Output "DRIFT_MS=median=$([Math]::Round($driftMs[$p50Index],2)); p95=$([Math]::Round($driftMs[$p95Index],2)); max=$([Math]::Round($maxDriftMs,2)); outliers_over_$($DriftThresholdMilliseconds)ms=$outliers"
Write-Output "TRACE_A=samples=$($statsA.samples); max_gap_ms=$($statsA.max_gap_ms); gaps_over_500ms=$($statsA.gaps_over_500_ms); paused_samples=$($statsA.paused_samples); buffering_samples=$($statsA.buffering_samples); player_error_samples=$($statsA.player_error_samples)"
Write-Output "TRACE_B=samples=$($statsB.samples); max_gap_ms=$($statsB.max_gap_ms); gaps_over_500ms=$($statsB.gaps_over_500_ms); paused_samples=$($statsB.paused_samples); buffering_samples=$($statsB.buffering_samples); player_error_samples=$($statsB.player_error_samples)"
if (-not $complete) { Write-Output 'SAMPLE_COVERAGE=failed; at least one per-client trace has a gap over 1000 ms' }
else { Write-Output 'SAMPLE_COVERAGE=pass; neither per-client trace has a gap over 1000 ms' }
if ($errorSamples -gt 0) { Write-Output "PLAYER_ERRORS=present; samples=$errorSamples" }
else { Write-Output 'PLAYER_ERRORS=none_observed' }
if ($withinThreshold -and $complete -and $errorSamples -eq 0) {
    Write-Output 'DRIFT_ACCEPTANCE=pass'
}
else {
    Write-Output 'DRIFT_ACCEPTANCE=fail_or_incomplete'
    exit 2
}
