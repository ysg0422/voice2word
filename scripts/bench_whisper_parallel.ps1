<#
.SYNOPSIS
  Whisper 解码并行策略基准：对比「单进程多线程 / 单进程 -p 多处理器 / 多进程切块」
  三种并行方式在纯 CPU 下的墙钟耗时，并把各配置输出合并为全局时间轴 JSON，
  供 examples/eval_whisper_parallel 计算 CER 做质量对照。

.DESCRIPTION
  与 src/engines/chunked_whisper.rs 的切块策略一一对应：
  多进程模式 = 把切片平均切给 N 个 whisper-cli 进程（每个 -p 1），并发跑，
  正是 transcribe_chunked / run_chunked_parallel 的实际行为。
  单进程 -p N 模式 = whisper.cpp 内部的 whisper_full_parallel 切分。

.EXAMPLE
  powershell -File scripts/bench_whisper_parallel.ps1
  powershell -File scripts/bench_whisper_parallel.ps1 -StartSec 300 -DurationSec 360
#>
param(
    [int]$StartSec = 300,
    [int]$DurationSec = 360,
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [double]$VadThreshold = 0.50,
    [int]$MaxContext = 32,
    [int]$Cores = 16,
    # 每个配置重复次数（取最小值，消除单次抖动）
    [int]$Reps = 2,
    # 去掉 --carry-initial-prompt（实测该参数既略慢又拉高 CER）
    [switch]$NoCarry,
    # 分号分隔；每项: "标签:进程数:每进程线程数:p<进程内处理器数>"
    [string]$ConfigList = 'single_t16_p1:1:16:p1;single_t8_p2:1:8:p2;single_t4_p4:1:4:p4;single_t2_p8:1:2:p8;mp2_t8:2:8:p1;mp3_t5:3:5:p1;mp4_t4:4:4:p1;mp6_t2:6:2:p1;mp8_t2:8:2:p1'
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$video = Join-Path $projectRoot 'testVideo\03.1.3概率不等式.mp4'
$ffmpeg = 'A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe'
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$benchDir = Join-Path $projectRoot 'target\whisper_parallel_bench'
$prompt = '以下是普通话录音。'

foreach ($p in @($video, $ffmpeg, $cli, $model, $vadModel)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少测试文件: $p" }
}
if (Test-Path -LiteralPath $benchDir) { Remove-Item -LiteralPath $benchDir -Recurse -Force }
New-Item -ItemType Directory -Path $benchDir -Force | Out-Null
$jsonDir = Join-Path $benchDir 'json'
New-Item -ItemType Directory -Path $jsonDir -Force | Out-Null
# 各进程原始输出单独放 raw/：评估脚本按目录扫 json，合并件与原始件同目录会被重复统计
$rawDir = Join-Path $benchDir 'raw'
New-Item -ItemType Directory -Path $rawDir -Force | Out-Null

# ── 1. 抽取整段基准音频（16kHz 单声道 PCM）──
$clipWav = Join-Path $benchDir "clip_${StartSec}_${DurationSec}.wav"
Write-Host "[1/4] 抽取 ${DurationSec}s 基准音频 ..." -ForegroundColor Cyan
& $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
    -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clipWav
if ($LASTEXITCODE -ne 0) { throw "FFmpeg 抽音频失败 (退出码 $LASTEXITCODE)" }
Write-Host ("      音频就绪: {0}" -f $clipWav)

# ── 2. 生成每进程的切片（多进程模式用）──
$sliceCache = @{}
function Get-SlicePath([int]$workers, [int]$index) {
    $key = "$workers-$index"
    if ($sliceCache.ContainsKey($key)) { return $sliceCache[$key] }
    $per = [double]$DurationSec / $workers
    $start = [double]$StartSec + $index * $per
    $path = Join-Path $benchDir ("slice_w{0}_{1}.wav" -f $workers, $index)
    & $ffmpeg -hide_banner -loglevel error -y -ss $start -t $per -i $video `
        -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $path
    if ($LASTEXITCODE -ne 0) { throw "FFmpeg 切片失败 (workers=$workers idx=$index)" }
    $sliceCache[$key] = @{ Path = $path; OffsetSec = $index * $per }
    return $sliceCache[$key]
}

function New-WhisperArgs([string]$model, [string]$audio, [int]$threads, [int]$processors, [string]$prefix) {
    $a = @(
        '-m', $model, '-f', $audio, '--vad', '-vm', $vadModel,
        '-vt', ('{0:0.00}' -f $VadThreshold), '-vsd', '250', '-l', 'zh',
        '-t', "$threads", '-p', "$processors", '-bo', '1', '-bs', '1', '-mc', "$MaxContext", '-sns',
        '-oj', '-of', $prefix, '-nf', '-ng', '--prompt', $prompt
    )
    if (-not $NoCarry) { $a += '--carry-initial-prompt' }
    return $a
}

# 启动一个 whisper-cli（异步），返回 Process 对象
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
    # 必须异步排空管道，否则 Windows 64KB 缓冲会阻塞子进程
    $p.BeginOutputReadLine()
    $p.BeginErrorReadLine()
    return $p
}

# ── 3. 逐配置跑基准 ──
$rows = @()
$idx = 0
$Configs = @($ConfigList -split ';' | Where-Object { $_.Trim() -ne '' })
foreach ($cfg in $Configs) {
    $idx++
    $parts = $cfg.Split(':')
    if ($parts.Count -ne 4) { throw "配置格式错误: $cfg" }
    $tag = $parts[0]; $workers = [int]$parts[1]; $threads = [int]$parts[2]; $pmode = $parts[3]
    $processors = if ($pmode -like 'p*') { [int]$pmode.Substring(1) } else { 1 }
    $prefix = Join-Path $rawDir $tag
    $best = [double]::MaxValue
    $mergeSrc = @()

    for ($rep = 1; $rep -le $Reps; $rep++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        if ($workers -eq 1) {
            # 单进程：整段音频，内部 -p $processors
            $argv = New-WhisperArgs $model $clipWav $threads $processors $prefix
            $p = Start-Whisper $cli $argv
            $p.WaitForExit()
            if ($p.ExitCode -ne 0) { Write-Warning "$tag 退出码 $($p.ExitCode)" }
            $sw.Stop()
            $mergeSrc = @(@{ Path = "$prefix.json"; OffsetSec = 0.0 })
        } else {
            # 多进程：把音频切成 $workers 段，各起一个进程（-p 1）并发
            $procs = @()
            $srcs = @()
            for ($i = 0; $i -lt $workers; $i++) {
                $slice = Get-SlicePath $workers $i
                $sp = Join-Path $rawDir ("{0}_s{1}" -f $tag, $i)
                $argv = New-WhisperArgs $model $slice.Path $threads 1 $sp
                $procs += Start-Whisper $cli $argv
                $srcs += @{ Path = "$sp.json"; OffsetSec = [double]$slice.OffsetSec }
            }
            foreach ($p in $procs) { $p.WaitForExit() }
            $sw.Stop()
            $mergeSrc = $srcs
        }
        if ($sw.Elapsed.TotalSeconds -lt $best) { $best = $sw.Elapsed.TotalSeconds }
    }

    # 合并为全局时间轴 JSON，供 CER 评估
    $mergedPath = Join-Path $jsonDir ("merged_{0}.json" -f $tag)
    $merged = @()
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    foreach ($src in $mergeSrc) {
        if (-not (Test-Path -LiteralPath $src.Path)) { continue }
        # 必须显式按 UTF-8 读：whisper-cli 输出无 BOM，PS 5.1 默认 ANSI 解码会把中文弄乱
        $obj = [System.IO.File]::ReadAllText($src.Path, $utf8) | ConvertFrom-Json
        foreach ($item in @($obj.transcription)) {
            if ($null -eq $item) { continue }
            $item.offsets.from = [double]$item.offsets.from + $src.OffsetSec * 1000.0
            $item.offsets.to = [double]$item.offsets.to + $src.OffsetSec * 1000.0
            $merged += $item
        }
    }
    $sorted = @($merged | Sort-Object { $_.offsets.from })
    # 包成 whisper 原生 JSON 结构，且以无 BOM 的 UTF-8 落盘（serde_json 不认 BOM）
    $payload = @{ result = @{ language = 'zh' }; transcription = $sorted } | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText($mergedPath, $payload, $utf8)

    $chars = (($merged | ForEach-Object { $_.text }) -join '').Length
    $rows += [pscustomobject]@{
        配置         = $tag
        进程数       = $workers
        每进程线程   = $threads
        进程内处理器 = $processors
        墙钟秒       = [math]::Round($best, 2)
        句数         = $merged.Count
        字数         = $chars
        合并JSON     = "json\merged_$tag.json"
    }
    Write-Host ("[{0}] {1,-14} 墙钟 {2,7:N2}s  句数 {3,4}  字数 {4,6}" -f `
        $idx, $tag, $best, $merged.Count, $chars) -ForegroundColor Yellow
}

# ── 4. 汇总 ──
Write-Host "`n===== Whisper 并行配置基准（音频 ${DurationSec}s，${Cores} 核纯 CPU）=====" -ForegroundColor Green
$rows | Sort-Object 墙钟秒 | Format-Table -AutoSize
$baseline = ($rows | Where-Object { $_.配置 -eq 'single_t16_p1' } | Select-Object -First 1).墙钟秒
if ($baseline) {
    Write-Host "相对 single_t16_p1 基线（$baseline s）的加速比：" -ForegroundColor Green
    $rows | Sort-Object 墙钟秒 | ForEach-Object {
        Write-Host ("  {0,-14} {1,7:N2}s   x{2:N2}" -f $_.配置, $_.墙钟秒, ($baseline / $_.墙钟秒))
    }
}
Write-Host "`n质量对照请运行：" -ForegroundColor Green
Write-Host "  cargo run --offline --example eval_whisper_parallel -- `"$jsonDir`" $StartSec $($StartSec + 5) $($StartSec + $DurationSec - 5)"
