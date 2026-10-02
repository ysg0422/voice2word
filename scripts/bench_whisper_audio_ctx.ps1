# 本脚本记录的是历史消融组合；--carry-initial-prompt 已于 2026-09-29 从默认参数链移除（实测只换来 1% 以内且符号不稳定的速度差，却让 CER 稳定高 1.1~1.2 个百分点），此处保留该参数仅用于复现历史组合。
<#
.SYNOPSIS
  编码器窗口（-ac / audio-ctx）算法消融：量化"缩短编码窗口"对纯编码算力的影响。

.DESCRIPTION
  背景（读 whisper.cpp v1.8.4 源码得出的事实）：
    - 编码窗口长度 = 2 * audio_ctx 个梅尔帧 = audio_ctx * 20ms，默认 audio_ctx=1500 → 30s。
    - 自注意力成本 ∝ 窗口长度²，窗口数 ∝ 1/窗口长度，
      因此"每 30s 音频的自注意力总成本 ∝ 窗口长度"，窗口越短总注意力越省。
    - 窗口内的卷积/FFN 成本与窗口长度无关（线性于总帧数），所以收益只来自注意力部分。
    - 附带收益：cross-attention 的 KV 长度随 audio_ctx 线性缩短，解码每 token 更便宜。

  本脚本只改 -ac 一个变量，线程数 / 处理器数 / 模型 / VAD / 全部解码参数保持
  与 config.toml 生产链路完全一致，排除并行与线程变量。

  输出：target\audio_ctx_bench\raw\<tag>.json  +  每个组合的 whisper 内部分项耗时。

.EXAMPLE
  powershell -File scripts/bench_whisper_audio_ctx.ps1
  powershell -File scripts/bench_whisper_audio_ctx.ps1 -AudioCtx @(1500,1000,750,512) -Threads 16
#>
param(
    [int[]]$AudioCtx = @(1500, 1000, 750, 512),
    [int]$Threads = 16,
    [int]$Processors = 1,
    [int]$StartSec = 300,
    [int]$DurationSec = 360,
    [double]$VadThreshold = 0.50,
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [string]$ClipPath = ''
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$video = Join-Path $projectRoot 'testVideo\03.1.3概率不等式.mp4'
$ffmpeg = 'A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe'
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$benchDir = Join-Path $projectRoot 'target\audio_ctx_bench'
$promptText = '以下是普通话录音。'
$utf8 = New-Object System.Text.UTF8Encoding($false)

foreach ($p in @($ffmpeg, $cli, $model, $vadModel)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少测试文件: $p" }
}
New-Item -ItemType Directory -Path $benchDir -Force | Out-Null
$rawDir = Join-Path $benchDir 'raw'
New-Item -ItemType Directory -Path $rawDir -Force | Out-Null

# 复用历史基准音频，保证与既有 base.json 基线逐字可比
$clip = if ($ClipPath) { $ClipPath } else {
    Join-Path $projectRoot "target\whisper_flag_bench\clip_${StartSec}_${DurationSec}.wav"
}
if (-not (Test-Path -LiteralPath $clip)) {
    if (-not (Test-Path -LiteralPath $video)) { throw "缺少测试视频: $video" }
    Write-Host "抽取 ${DurationSec}s 基准音频 ..." -ForegroundColor Cyan
    & $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
        -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clip
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg 抽音频失败 (退出码 $LASTEXITCODE)" }
}
Write-Host "基准音频: $clip" -ForegroundColor Cyan

# whisper.cpp 的 whisper_print_timings 分项：从 stderr 抓 encode/decode/mel/vad/total
function Get-Timings([string]$stderrText) {
    $t = @{ mel = 0.0; encode = 0.0; decode = 0.0; prompt = 0.0; vad = 0.0; total = 0.0; load = 0.0; encRuns = 0 }
    foreach ($line in ($stderrText -split "`n")) {
        if ($line -match 'whisper_print_timings:\s*load time\s*=\s*([0-9.]+)') { $t.load = [double]$Matches[1] }
        elseif ($line -match 'whisper_print_timings:\s*mel time\s*=\s*([0-9.]+)') { $t.mel = [double]$Matches[1] }
        elseif ($line -match 'whisper_print_timings:\s*encode time\s*=\s*([0-9.]+) ms\s*/\s*(\d+) runs') {
            $t.encode = [double]$Matches[1]; $t.encRuns = [int]$Matches[2]
        }
        elseif ($line -match 'whisper_print_timings:\s*decode time\s*=\s*([0-9.]+)') { $t.decode = [double]$Matches[1] }
        elseif ($line -match 'whisper_print_timings:\s*prompt time\s*=\s*([0-9.]+)') { $t.prompt = [double]$Matches[1] }
        elseif ($line -match 'whisper_print_timings:\s*vad time\s*=\s*([0-9.]+)') { $t.vad = [double]$Matches[1] }
        elseif ($line -match 'whisper_print_timings:\s*total time\s*=\s*([0-9.]+)') { $t.total = [double]$Matches[1] }
    }
    return $t
}

function Invoke-Whisper([string]$tag, [int]$ac) {
    $prefix = Join-Path $rawDir $tag
    $argv = @('-m', $model, '-f', $clip, '--vad', '-vm', $vadModel,
        '-vt', ('{0:0.00}' -f $VadThreshold), '-vsd', '250',
        '-l', 'zh', '-t', "$Threads", '-p', "$Processors",
        '-bo', '1', '-bs', '1', '-mc', '32', '-sns',
        '-oj', '-of', $prefix, '-nf', '-ng',
        '--prompt', $promptText, '--carry-initial-prompt')
    if ($ac -gt 0) { $argv += @('-ac', "$ac") }

    $errFile = "$prefix.err"
    $outFile = "$prefix.stdout"
    # 用 System.Diagnostics.Process 直接读管道：既躲开 PowerShell 5.1 对原生命令
    # stderr 抛 NativeCommandError 的坑，也不会因 cmd 引号转义失败。
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $cli
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.Arguments = ($argv | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $null = $proc.Start()
    $errTask = $proc.StandardError.ReadToEndAsync()
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $proc.WaitForExit()
    $sw.Stop()
    $code = $proc.ExitCode
    $errText = $errTask.Result
    [System.IO.File]::WriteAllText($errFile, $errText, $utf8)
    [System.IO.File]::WriteAllText($outFile, $outTask.Result, $utf8)
    if ($code -ne 0) { Write-Warning "$tag 退出码 $code" }
    $timings = Get-Timings $errText

    $jsonPath = "$prefix.json"
    if (-not (Test-Path -LiteralPath $jsonPath)) { throw "$tag 未产出 JSON" }
    $json = [System.IO.File]::ReadAllText($jsonPath, $utf8) | ConvertFrom-Json
    $items = @($json.transcription)
    $text = (($items | ForEach-Object { $_.text }) -join '')

    return [pscustomobject]@{
        Tag        = $tag
        Ac         = $ac
        WindowSec  = if ($ac -gt 0) { [math]::Round($ac * 0.02, 2) } else { 30.0 }
        WallSec    = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        EncodeSec  = [math]::Round($timings.encode / 1000.0, 2)
        DecodeSec  = [math]::Round($timings.decode / 1000.0, 2)
        MelSec     = [math]::Round($timings.mel / 1000.0, 2)
        VadSec     = [math]::Round($timings.vad / 1000.0, 2)
        PromptSec  = [math]::Round($timings.prompt / 1000.0, 2)
        EncRuns    = $timings.encRuns
        Segments   = $items.Count
        Chars      = $text.Length
        Text       = $text
    }
}

$rows = @()
foreach ($ac in $AudioCtx) {
    $tag = if ($ac -gt 0) { "ac$ac" } else { 'ac0_default' }
    Write-Host "跑 $tag (窗口 $(if ($ac -gt 0) { [math]::Round($ac * 0.02, 2) } else { 30 })s) ..." -ForegroundColor Yellow
    $r = Invoke-Whisper $tag $ac
    $rows += $r
    Write-Host ("  墙钟 {0,7:N2}s  编码 {1,7:N2}s  解码 {2,6:N2}s  VAD {3,5:N2}s  编码次数 {4,4}  字数 {5,5}" -f `
        $r.WallSec, $r.EncodeSec, $r.DecodeSec, $r.VadSec, $r.EncRuns, $r.Chars) -ForegroundColor Green
}

Write-Host "`n===== 编码器窗口消融（音频 ${DurationSec}s，-t $Threads -p $Processors，纯 CPU，仅 -ac 一个变量）=====" -ForegroundColor Green
$rows | Format-Table Tag, WindowSec, WallSec, EncodeSec, DecodeSec, MelSec, VadSec, EncRuns, Segments, Chars -AutoSize

$base = $rows | Where-Object { $_.Ac -le 0 } | Select-Object -First 1
if (-not $base) { $base = $rows | Select-Object -First 1 }
Write-Host "相对 $($base.Tag)（墙钟 $($base.WallSec)s / 编码 $($base.EncodeSec)s）的收益：" -ForegroundColor Green
foreach ($r in $rows) {
    $dt = if ($r.WallSec -gt 0) { $base.WallSec / $r.WallSec } else { 0 }
    Write-Host ("  {0,-14} 墙钟 {1,7:N2}s  x{2:N3}   编码 {3,7:N2}s  x{4:N3}" -f `
        $r.Tag, $r.WallSec, $dt, $r.EncodeSec, ($base.EncodeSec / [math]::Max($r.EncodeSec, 0.01)))
}

# 逐字对比：与基线文本求字符级差异比例，确认 -ac 是否丢字
Write-Host "`n与基线文本的差异（越接近 0 越安全）：" -ForegroundColor Green
foreach ($r in $rows) {
    if ($r.Tag -eq $base.Tag) { continue }
    $a = $base.Text; $b = $r.Text
    $common = 0
    $minLen = [math]::Min($a.Length, $b.Length)
    for ($i = 0; $i -lt $minLen; $i++) { if ($a[$i] -eq $b[$i]) { $common++ } }
    $diff = if ($a.Length -gt 0) { 1.0 - ($common / [double]$a.Length) } else { 0 }
    Write-Host ("  {0,-14} 字数 {1,5} (基线 {2,5})  首段差异 {3:P2}" -f $r.Tag, $b.Length, $a.Length, $diff)
}
