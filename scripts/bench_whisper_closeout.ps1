<#
.SYNOPSIS
  「提升 Whisper 解码速度且不损失精度」工作线的收口基准：在同一个 10 分钟音频、
  同一个模型档位上，逐个开启各项优化，测墙钟耗时并产出 SRT 供 CER 对照。

.DESCRIPTION
  对照组（全部纯 CPU -ng、同一 clip、同一模型）：
    baseline       whisper-cli 出厂默认解码（beam5，无 VAD、无 prompt、有温度回退）
    vad            基线 + Silero VAD 静音跳过（-vt 0.50 -vsd 250）
    prod           vad + 生产解码参数（-bo 1 -bs 1 -mc 32 -sns -nf + prompt + carry）
    prod_nocarry   prod 去掉 --carry-initial-prompt（量化该参数是否值得保留）
    prod_p2        prod 改单进程 -p 2 -t 6（进程内多处理器并行）
    prod_ac750     prod + -ac 750（audio_ctx 裁剪到 15s 窗口；src 未接入，仅脚本侧验证）
    prod_ac512     prod + -ac 512
    mp2_slice      prod 参数，但把 600s 切成 2x300s 两个 -p 1 -t 6 进程并发
                   （复刻 src/engines/chunked_whisper.rs::transcribe_chunked 的策略）

  输出：<BenchDir>\srt\<tag>.srt（时间轴基于切片，CER 脚本会 +StartSec 偏移）
        <BenchDir>\speed_results.txt

.EXAMPLE
  powershell -File scripts/bench_whisper_closeout.ps1
#>
param(
    [int]$StartSec = 300,
    [int]$DurationSec = 600,
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [int]$Threads = 12,
    [double]$VadThreshold = 0.50,
    [string]$BenchDir = ''
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'resolve_paths.ps1')
$ffmpeg = Get-V2wPath -Key 'ffmpeg' -ProjectRoot $projectRoot
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$video = Join-Path $projectRoot 'testVideo\03.1.3概率不等式.mp4'
if (-not $BenchDir) { $BenchDir = Join-Path $env:TEMP 'v2w_closeout' }
$srtDir = Join-Path $BenchDir 'srt'

foreach ($p in @($ffmpeg, $cli, $model, $vadModel, $video)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少测试文件: $p" }
}
New-Item -ItemType Directory -Path $BenchDir -Force | Out-Null
New-Item -ItemType Directory -Path $srtDir -Force | Out-Null

# PS 5.1 按 ANSI 读 .ps1，中文提示词用码点构造（"以下是普通话录音。"）
$promptText = [string]::Concat(
    [char]0x4EE5, [char]0x4E0B, [char]0x662F, [char]0x666E,
    [char]0x901A, [char]0x8BDD, [char]0x5F55, [char]0x97F3, [char]0x3002)

$clip = Join-Path $BenchDir "clip_${StartSec}_${DurationSec}.wav"
if (-not (Test-Path -LiteralPath $clip)) {
    Write-Host "抽取 ${DurationSec}s 基准音频 ..." -ForegroundColor Cyan
    & $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
        -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clip
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg 抽音频失败 (退出码 $LASTEXITCODE)" }
}

function Start-Whisper([string]$exe, [string[]]$argv) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.Arguments = ($argv | ForEach-Object {
        if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
    }) -join ' '
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $psi
    $null = $p.Start()
    # 必须异步排空两条管道，否则 Windows 64KB 缓冲会阻塞子进程；
    # 用 ReadToEndAsync（而非 BeginOutputReadLine）以便稍后还能读 StandardError。
    $p | Add-Member -NotePropertyName OutTask -NotePropertyValue $p.StandardOutput.ReadToEndAsync() -Force
    $p | Add-Member -NotePropertyName ErrTask -NotePropertyValue $p.StandardError.ReadToEndAsync() -Force
    return $p
}

function Get-Timing([string]$errText, [string]$name) {
    foreach ($line in ($errText -split "`n")) {
        if ($line -match ("whisper_print_timings:\s*" + $name + "\s*=\s*([0-9.]+)\s*ms")) { return [double]$Matches[1] }
    }
    return 0.0
}

# 基线参数：whisper-cli 出厂默认（beam 5、无 VAD、无 prompt、允许温度回退）
$baseArgs = @('-m', $model, '-f', $clip, '-l', 'zh', '-t', "$Threads", '-p', '1', '-ng', '-oj', '-osrt')
# 生产参数：与 src/engines/whisper.rs 当前下发的组合一致
$prodArgs = @('-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf',
    '--prompt', $promptText, '--carry-initial-prompt')
$vadArgs = @('--vad', '-vm', $vadModel, '-vt', ('{0:0.00}' -f $VadThreshold), '-vsd', '250')

function Invoke-Run([string]$tag, [string[]]$argv) {
    $prefix = Join-Path $BenchDir $tag
    $argv = $argv + @('-of', $prefix)
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = Start-Whisper $cli $argv
    $p.WaitForExit()
    $sw.Stop()
    $errText = $p.ErrTask.Result
    $code = $p.ExitCode
    [System.IO.File]::WriteAllText("$prefix.err", $errText, (New-Object System.Text.UTF8Encoding($false)))

    $srt = "$prefix.srt"
    if (-not (Test-Path -LiteralPath $srt)) { throw "$tag 未产出 SRT（退出码 $code）" }
    Copy-Item -LiteralPath $srt -Destination (Join-Path $srtDir "$tag.srt") -Force

    $encRuns = 0; $decRuns = 0
    if ($errText -match 'encode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $encRuns = [int]$Matches[1] }
    if ($errText -match 'decode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $decRuns = [int]$Matches[1] }
    $segCount = (Select-String -LiteralPath (Join-Path $srtDir "$tag.srt") -Pattern '-->' -AllMatches).Count

    $row = [pscustomobject]@{
        Tag       = $tag
        WallSec   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        EncodeSec = [math]::Round((Get-Timing $errText 'encode time') / 1000.0, 2)
        DecodeSec = [math]::Round((Get-Timing $errText 'decode time') / 1000.0, 2)
        PromptSec = [math]::Round((Get-Timing $errText 'prompt time') / 1000.0, 2)
        VadSec    = [math]::Round((Get-Timing $errText 'vad time') / 1000.0, 2)
        EncRuns   = $encRuns
        DecRuns   = $decRuns
        Segments  = $segCount
        Exit      = $code
    }
    Write-Host ("{0,-14} 墙钟 {1,8:N2}s  编码 {2,7:N2}s/{3,4}runs  解码 {4,7:N2}s/{5,5}runs  VAD {6,6:N2}s  句 {7,4}" -f `
        $row.Tag, $row.WallSec, $row.EncodeSec, $row.EncRuns, $row.DecodeSec, $row.DecRuns, $row.VadSec, $row.Segments) -ForegroundColor Green
    return $row
}

$rows = @()
Write-Host "`n===== 逐项优化 A/B（音频 ${DurationSec}s，模型 $(Split-Path $ModelRelative -Leaf)，-t $Threads，纯 CPU）=====" -ForegroundColor Cyan

Write-Host "[1] baseline ..." -ForegroundColor Yellow
$rows += Invoke-Run 'baseline' $baseArgs

Write-Host "[2] vad ..." -ForegroundColor Yellow
$rows += Invoke-Run 'vad' ($baseArgs + $vadArgs)

Write-Host "[3] prod ..." -ForegroundColor Yellow
$rows += Invoke-Run 'prod' ($baseArgs + $vadArgs + $prodArgs)

Write-Host "[4] prod_nocarry ..." -ForegroundColor Yellow
$nc = $baseArgs + $vadArgs + @('-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf', '--prompt', $promptText)
$rows += Invoke-Run 'prod_nocarry' $nc

Write-Host "[5] prod_p2 (-p 2 -t 6) ..." -ForegroundColor Yellow
$p2 = @('-m', $model, '-f', $clip, '-l', 'zh', '-t', '6', '-p', '2', '-ng', '-oj', '-osrt') + $vadArgs + $prodArgs
$rows += Invoke-Run 'prod_p2' $p2

Write-Host "[6] prod_ac750 ..." -ForegroundColor Yellow
$rows += Invoke-Run 'prod_ac750' (($baseArgs + $vadArgs + $prodArgs) + @('-ac', '750'))

Write-Host "[7] prod_ac512 ..." -ForegroundColor Yellow
$rows += Invoke-Run 'prod_ac512' (($baseArgs + $vadArgs + $prodArgs) + @('-ac', '512'))

Write-Host "[8] mp2_slice（2x300s 两进程并发，复刻 chunked_whisper 策略）..." -ForegroundColor Yellow
$half = [double]$DurationSec / 2
$slicePaths = @()
for ($i = 0; $i -lt 2; $i++) {
    $sp = Join-Path $BenchDir "slice_$i.wav"
    if (-not (Test-Path -LiteralPath $sp)) {
        & $ffmpeg -hide_banner -loglevel error -y -ss ($i * $half) -t $half -i $clip `
            -ar 16000 -ac 1 -c:a pcm_s16le $sp
    }
    $slicePaths += $sp
}
$procs = @()
$sw = [System.Diagnostics.Stopwatch]::StartNew()
for ($i = 0; $i -lt 2; $i++) {
    $prefix = Join-Path $BenchDir "mp2_s$i"
    $argv = @('-m', $model, '-f', $slicePaths[$i], '-l', 'zh', '-t', '6', '-p', '1', '-ng', '-oj', '-osrt') + $vadArgs + $prodArgs + @('-of', $prefix)
    $procs += Start-Whisper $cli $argv
}
foreach ($p in $procs) { $p.WaitForExit() }
$sw.Stop()
# 两段 SRT 都还在切片时间轴上（0~300s / 0~300s），直接顺序拼接即为
# 0~600s 的连续切片时间轴；CER 阶段统一 +StartSec 还原到原视频时间轴。
$merge = New-Object System.Collections.Generic.List[string]
foreach ($i in 0, 1) {
    $txt = [System.IO.File]::ReadAllText((Join-Path $BenchDir "mp2_s$i.srt"), (New-Object System.Text.UTF8Encoding($false)))
    $merge.Add($txt.TrimEnd())
}
[System.IO.File]::WriteAllText((Join-Path $srtDir 'mp2_slice.srt'), ($merge -join "`r`n`r`n"), (New-Object System.Text.UTF8Encoding($false)))
$segCount = (Select-String -LiteralPath (Join-Path $srtDir 'mp2_slice.srt') -Pattern '-->' -AllMatches).Count
$rows += [pscustomobject]@{
    Tag       = 'mp2_slice'
    WallSec   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
    EncodeSec = 0; DecodeSec = 0; PromptSec = 0; VadSec = 0; EncRuns = 0; DecRuns = 0
    Segments  = $segCount; Exit = 0
}
Write-Host ("{0,-14} 墙钟 {1,8:N2}s  句 {2,4}" -f 'mp2_slice', $rows[-1].WallSec, $segCount) -ForegroundColor Green

Write-Host "`n===== 汇总 =====" -ForegroundColor Cyan
$rows | Format-Table -AutoSize
$baseline = ($rows | Where-Object { $_.Tag -eq 'baseline' } | Select-Object -First 1).WallSec
if ($baseline) {
    Write-Host "相对 baseline（$baseline s）的加速比：" -ForegroundColor Green
    foreach ($r in ($rows | Sort-Object WallSec)) {
        Write-Host ("  {0,-14} {1,8:N2}s   x{2:N2}" -f $r.Tag, $r.WallSec, ($baseline / $r.WallSec))
    }
}
$rows | ConvertTo-Json | Out-File -FilePath (Join-Path $BenchDir 'speed_results.txt') -Encoding utf8

# 切片 SRT（0 起）统一 +StartSec 还原到原视频时间轴，交给既有的 scripts\eval_cer.py
# （该脚本硬编码评估区间 05:05–14:55，与 StartSec=300 / DurationSec=600 对齐）。
$offsetDir = Join-Path $srtDir 'offset'
New-Item -ItemType Directory -Path $offsetDir -Force | Out-Null
function Add-SrtOffset([string]$path, [double]$offsetSec) {
    $lines = [System.IO.File]::ReadAllLines($path, (New-Object System.Text.UTF8Encoding($false)))
    $out = New-Object System.Collections.Generic.List[string]
    foreach ($l in $lines) {
        if ($l -match '^(\d{2}):(\d{2}):(\d{2}),(\d{3})\s*-->\s*(\d{2}):(\d{2}):(\d{2}),(\d{3})') {
            $s = [int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3] + [int]$Matches[4] / 1000.0 + $offsetSec
            $e = [int]$Matches[5] * 3600 + [int]$Matches[6] * 60 + [int]$Matches[7] + [int]$Matches[8] / 1000.0 + $offsetSec
            $fs = [TimeSpan]::FromSeconds($s); $fe = [TimeSpan]::FromSeconds($e)
            $out.Add(('{0:00}:{1:00}:{2:00},{3:000} --> {4:00}:{5:00}:{6:00},{7:000}' -f `
                $fs.Hours, $fs.Minutes, $fs.Seconds, $fs.Milliseconds, $fe.Hours, $fe.Minutes, $fe.Seconds, $fe.Milliseconds))
        } else { $out.Add($l) }
    }
    [System.IO.File]::WriteAllLines($path, $out, (New-Object System.Text.UTF8Encoding($false)))
}
Get-ChildItem -LiteralPath $srtDir -Filter '*.srt' | ForEach-Object {
    $dst = Join-Path $offsetDir $_.Name
    Copy-Item -LiteralPath $_.FullName -Destination $dst -Force
    Add-SrtOffset $dst $StartSec
}
Write-Host "`nSRT 输出目录: $srtDir  (原视频时间轴副本: $offsetDir)" -ForegroundColor Cyan
Write-Host "CER 对照: python scripts\eval_cer.py (Get-ChildItem '$offsetDir\*.srt').FullName" -ForegroundColor Cyan
