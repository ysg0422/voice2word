# 本脚本记录的是历史消融组合；--carry-initial-prompt 已于 2026-09-29 从默认参数链移除（实测只换来 1% 以内且符号不稳定的速度差，却让 CER 稳定高 1.1~1.2 个百分点），此处保留该参数仅用于复现历史组合。
param(
    [double[]]$Rates = @(1.0, 1.15, 1.25, 1.35, 1.5),
    [double[]]$VadThresholds = @(0.50),
    [string]$Prompt = '以下是普通话录音。',
    [string]$ResultTag = '',
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [int]$Processors = 1,
    [int]$Threads = 16,
    [int]$StartSec = 300,
    [int]$DurationSec = 600
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$video = Join-Path $projectRoot 'testVideo\03.1.3概率不等式.mp4'
$ffmpeg = 'A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe'
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$benchDir = Join-Path $projectRoot 'target\whisper_speed_bench'

foreach ($path in @($video, $ffmpeg, $cli, $model, $vadModel)) {
    if (-not (Test-Path -LiteralPath $path)) { throw "缺少测试文件: $path" }
}
New-Item -ItemType Directory -Path $benchDir -Force | Out-Null

$results = foreach ($rate in $Rates) {
    if ($rate -lt 1.0 -or $rate -gt 1.5) { throw '倍率必须在 1.0～1.5 之间' }
    $rateTag = ('{0:0.00}' -f $rate).Replace('.', '_')
    $wav = Join-Path $benchDir "clip_${StartSec}_${DurationSec}_x${rateTag}.wav"
    $ffmpegArgs = @('-hide_banner', '-loglevel', 'error', '-y', '-ss', "$StartSec", '-t', "$DurationSec", '-i', $video, '-map', '0:a:0', '-vn', '-sn', '-dn')
    if ($rate -gt 1.0) { $ffmpegArgs += @('-af', ('atempo={0:0.00}' -f $rate)) }
    $ffmpegArgs += @('-ar', '16000', '-ac', '1', '-c:a', 'pcm_s16le', $wav)
    $audioSeconds = (Measure-Command {
        & $ffmpeg @ffmpegArgs 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "FFmpeg 退出码 $LASTEXITCODE" }
    }).TotalSeconds

    foreach ($threshold in $VadThresholds) {
        $vadTag = ('{0:0.00}' -f $threshold).Replace('.', '_')
        $suffix = if ($ResultTag) { "_$ResultTag" } else { '' }
        $prefix = Join-Path $benchDir "result_x${rateTag}_vad${vadTag}${suffix}"
        $threadsPerProcessor = [math]::Max(4, [int][math]::Floor($Threads / $Processors))
        $cliArgs = @('-m', $model, '-f', $wav, '--vad', '-vm', $vadModel,
            '-vt', ('{0:0.00}' -f $threshold), '-vsd', '250', '-l', 'zh',
            '-t', "$threadsPerProcessor", '-p', "$Processors", '-bo', '1', '-bs', '1', '-mc', '32', '-sns',
            '-oj', '-of', $prefix, '-nf', '-ng', '--prompt', $Prompt,
            '--carry-initial-prompt')
        $asrSeconds = (Measure-Command {
            & $cli @cliArgs 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "Whisper 退出码 $LASTEXITCODE；倍率 $rate，VAD $threshold" }
        }).TotalSeconds
        $jsonPath = "$prefix.json"
        if (-not (Test-Path -LiteralPath $jsonPath)) { throw "Whisper 未输出 JSON: $jsonPath" }
        $json = Get-Content -LiteralPath $jsonPath -Raw | ConvertFrom-Json
        $items = @($json.transcription)
        $textLength = (($items | ForEach-Object { $_.text }) -join '').Length
        [pscustomobject]@{
            Rate = $rate
            Vad = $threshold
            AudioSec = [math]::Round($audioSeconds, 2)
            AsrSec = [math]::Round($asrSeconds, 2)
            TotalSec = [math]::Round($audioSeconds + $asrSeconds, 2)
            Segments = $items.Count
            TextChars = $textLength
            Json = $jsonPath
        }
    }
}

$results | Format-Table Rate, Vad, AudioSec, AsrSec, TotalSec, Segments, TextChars -AutoSize
