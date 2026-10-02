# 本脚本记录的是历史消融组合；--carry-initial-prompt 已于 2026-09-29 从默认参数链移除（实测只换来 1% 以内且符号不稳定的速度差，却让 CER 稳定高 1.1~1.2 个百分点），此处保留该参数仅用于复现历史组合。
<#
.SYNOPSIS
  Whisper 解码参数消融基准：量化 -mc（跨句上下文）/ --carry-initial-prompt /
  --prompt / VAD 等参数各自对纯解码耗时的影响（单进程固定线程数，排除并行变量）。

.DESCRIPTION
  whisper.cpp 每个 segment 都要把 [sot, lang, task, ...prompt_past(-mc), 初始prompt]
  作为前缀重新解码一遍，再自回归生成正文。前缀 token 数与正文长度同量级时，
  解码成本可能有一半以上花在重复喂上下文上。本脚本就是量化这部分开销。

  输出：raw\<tag>.json（whisper 原始）+ json\merged_<tag>.json（供 CER 评估）。

.EXAMPLE
  powershell -File scripts/bench_whisper_flags.ps1
  powershell -File scripts/bench_whisper_flags.ps1 -Threads 8 -Processors 2
#>
param(
    [int]$StartSec = 300,
    [int]$DurationSec = 360,
    [int]$Threads = 16,
    [int]$Processors = 1,
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [double]$VadThreshold = 0.50
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$video = Join-Path $projectRoot 'testVideo\03.1.3概率不等式.mp4'
$ffmpeg = 'A:\cppsoft\ffmpeg-6.9\bin\ffmpeg.exe'
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$benchDir = Join-Path $projectRoot 'target\whisper_flag_bench'
$promptText = '以下是普通话录音。'
$utf8 = New-Object System.Text.UTF8Encoding($false)

foreach ($p in @($video, $ffmpeg, $cli, $model, $vadModel)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少测试文件: $p" }
}
if (Test-Path -LiteralPath $benchDir) { Remove-Item -LiteralPath $benchDir -Recurse -Force }
$rawDir = Join-Path $benchDir 'raw'
$jsonDir = Join-Path $benchDir 'json'
New-Item -ItemType Directory -Path $rawDir -Force | Out-Null
New-Item -ItemType Directory -Path $jsonDir -Force | Out-Null

# ── 消融矩阵：基线 = 当前 config.toml 实际下发的参数组合 ──
$sets = @(
    @{ Tag = 'base';           Mc = 32;  Carry = $true;  Prompt = $true;  Vad = $true  },
    @{ Tag = 'nocarry';        Mc = 32;  Carry = $false; Prompt = $true;  Vad = $true  },
    @{ Tag = 'mc0';            Mc = 0;   Carry = $false; Prompt = $true;  Vad = $true  },
    @{ Tag = 'mc16_nocarry';   Mc = 16;  Carry = $false; Prompt = $true;  Vad = $true  },
    @{ Tag = 'noprompt';       Mc = 32;  Carry = $false; Prompt = $false; Vad = $true  },
    @{ Tag = 'mc0_noprompt';   Mc = 0;   Carry = $false; Prompt = $false; Vad = $true  },
    @{ Tag = 'base_novad';     Mc = 32;  Carry = $true;  Prompt = $true;  Vad = $false }
)

$clipWav = Join-Path $benchDir "clip_${StartSec}_${DurationSec}.wav"
Write-Host "[1/3] 抽取 ${DurationSec}s 基准音频 ..." -ForegroundColor Cyan
& $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
    -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clipWav
if ($LASTEXITCODE -ne 0) { throw "FFmpeg 抽音频失败 (退出码 $LASTEXITCODE)" }

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
    $p.BeginOutputReadLine()
    $p.BeginErrorReadLine()
    return $p
}

Write-Host "[2/3] 逐个参数组合跑解码基准 ..." -ForegroundColor Cyan
$rows = @()
$i = 0
foreach ($s in $sets) {
    $i++
    $prefix = Join-Path $rawDir $s.Tag
    $argv = @('-m', $model, '-f', $clipWav, '-l', 'zh',
        '-t', "$Threads", '-p', "$Processors", '-bo', '1', '-bs', '1', '-sns',
        '-oj', '-of', $prefix, '-nf', '-ng')
    if ($s.Vad) {
        $argv += @('--vad', '-vm', $vadModel, '-vt', ('{0:0.00}' -f $VadThreshold), '-vsd', '250')
    }
    if ($s.Mc -ge 0) { $argv += @('-mc', "$($s.Mc)") }
    if ($s.Prompt) { $argv += @('--prompt', $promptText) }
    if ($s.Carry) { $argv += @('--carry-initial-prompt') }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $p = Start-Whisper $cli $argv
    $p.WaitForExit()
    $sw.Stop()
    if ($p.ExitCode -ne 0) { Write-Warning "$($s.Tag) 退出码 $($p.ExitCode)" }

    $srcJson = "$prefix.json"
    if (-not (Test-Path -LiteralPath $srcJson)) { throw "$($s.Tag) 未产出 JSON" }
    # 单进程输出已是全局时间轴，直接复制为合并件供评估
    $obj = [System.IO.File]::ReadAllText($srcJson, $utf8) | ConvertFrom-Json
    $merged = @($obj.transcription)
    $payload = @{ result = @{ language = 'zh' }; transcription = $merged } | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText((Join-Path $jsonDir "merged_$($s.Tag).json"), $payload, $utf8)

    $chars = (($merged | ForEach-Object { $_.text }) -join '').Length
    $rows += [pscustomobject]@{
        组合     = $s.Tag
        Mc       = $s.Mc
        Carry    = $s.Carry
        Prompt   = $s.Prompt
        Vad      = $s.Vad
        墙钟秒   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        句数     = $merged.Count
        字数     = $chars
    }
    Write-Host ("[{0}/{1}] {2,-14} 墙钟 {3,7:N2}s  句数 {4,4}  字数 {5,6}" -f `
        $i, $sets.Count, $s.Tag, $sw.Elapsed.TotalSeconds, $merged.Count, $chars) -ForegroundColor Yellow
}

Write-Host "`n===== Whisper 解码参数消融（音频 ${DurationSec}s，-t $Threads -p $Processors，纯 CPU）=====" -ForegroundColor Green
$rows | Sort-Object 墙钟秒 | Format-Table -AutoSize
$base = ($rows | Where-Object { $_.组合 -eq 'base' } | Select-Object -First 1).墙钟秒
if ($base) {
    Write-Host "相对 base（$base s）的加速比：" -ForegroundColor Green
    $rows | Sort-Object 墙钟秒 | ForEach-Object {
        Write-Host ("  {0,-14} {1,7:N2}s   x{2:N2}" -f $_.组合, $_.墙钟秒, ($base / $_.墙钟秒))
    }
}
Write-Host "`n质量对照：" -ForegroundColor Green
Write-Host "  cargo run --offline --example eval_whisper_parallel -- `"$jsonDir`" $StartSec $($StartSec + 5) $($StartSec + $DurationSec - 5)"
