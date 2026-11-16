param(
    [Parameter(Mandatory = $true)]
    [string] $MediaPath,

    [Parameter(Mandatory = $true)]
    [string] $EnglishSubtitlePath,

    [Parameter(Mandatory = $true)]
    [string] $AlternateSubtitlePath
)

$ErrorActionPreference = 'Stop'

$wslCommand = Get-Command wsl.exe -ErrorAction SilentlyContinue
if ($null -eq $wslCommand) {
    throw 'WSL is required to run the existing ffprobe binary; no software is installed.'
}

$media = (Resolve-Path -LiteralPath $MediaPath -ErrorAction Stop).Path
$sidecars = @($EnglishSubtitlePath, $AlternateSubtitlePath) | ForEach-Object {
    (Resolve-Path -LiteralPath $_ -ErrorAction Stop).Path
}

$ffprobePath = & $wslCommand.Source --exec sh -lc 'command -v ffprobe' 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($ffprobePath)) {
    throw 'No ffprobe is available in the existing WSL installation; no software is installed.'
}
$ffprobePath = $ffprobePath.Trim()

$wslMedia = & $wslCommand.Source --exec wslpath -a -u $media 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($wslMedia)) {
    throw 'WSL could not resolve the local media path.'
}
$wslMedia = $wslMedia.Trim()

$probeJson = & $wslCommand.Source --exec $ffprobePath -v error `
    -show_entries 'stream=index,codec_type,codec_name,width,height,sample_rate,channels:format=format_name,duration' `
    -of json $wslMedia 2>$null
if ($LASTEXITCODE -ne 0) {
    throw 'ffprobe could not inspect the local media file.'
}
$probe = $probeJson | ConvertFrom-Json -ErrorAction Stop
$video = @($probe.streams | Where-Object { $_.codec_type -eq 'video' })
$audio = @($probe.streams | Where-Object { $_.codec_type -eq 'audio' })
if ($video.Count -lt 1 -or $audio.Count -lt 1) {
    throw 'The fixture must contain at least one video stream and one audio stream.'
}
if (-not ($video | Where-Object { $_.codec_name -eq 'h264' }) -or
    -not ($audio | Where-Object { $_.codec_name -eq 'aac' })) {
    throw 'The fixture must include H.264 video and AAC audio.'
}

$subtitleEvidence = foreach ($sidecar in $sidecars) {
    $text = Get-Content -LiteralPath $sidecar -Raw
    $cues = @()
    foreach ($block in ($text -split '(?:\r?\n){2,}')) {
        $lines = @($block -split '\r?\n' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
        if ($lines.Count -ge 3 -and $lines[1] -match '-->') {
            $cues += ($lines[2..($lines.Count - 1)] -join ' ')
        }
    }
    if ($cues.Count -lt 1) {
        throw "Subtitle sidecar contains no timed cues: $([IO.Path]::GetFileName($sidecar))"
    }
    [pscustomobject]@{
        file = [IO.Path]::GetFileName($sidecar)
        cue_count = $cues.Count
        sha256 = (Get-FileHash -LiteralPath $sidecar -Algorithm SHA256).Hash
        first_cue_text = $cues[0]
    }
}
if ($subtitleEvidence[0].first_cue_text -eq $subtitleEvidence[1].first_cue_text) {
    throw 'The two subtitle sidecars must contain distinct cue text.'
}

$mediaItem = Get-Item -LiteralPath $media
[pscustomobject]@{
    media_file = $mediaItem.Name
    media_size = $mediaItem.Length
    media_sha256 = (Get-FileHash -LiteralPath $media -Algorithm SHA256).Hash
    probe_tool = $ffprobePath
    format = $probe.format.format_name
    duration_seconds = [double]$probe.format.duration
    video_streams = @($video | ForEach-Object {
        [pscustomobject]@{ index = $_.index; codec = $_.codec_name; width = $_.width; height = $_.height }
    })
    audio_streams = @($audio | ForEach-Object {
        [pscustomobject]@{ index = $_.index; codec = $_.codec_name; sample_rate = $_.sample_rate; channels = $_.channels }
    })
    subtitle_sidecars = $subtitleEvidence | ForEach-Object {
        [pscustomobject]@{ file = $_.file; cue_count = $_.cue_count; sha256 = $_.sha256 }
    }
    distinct_subtitle_cues = $true
} | ConvertTo-Json -Depth 6
