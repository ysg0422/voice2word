<#
.SYNOPSIS
  Closeout verification: independently reproduce "whisper-cli factory default vs
  current production flags" wall-clock on one clip and one model, on both the GPU
  and CPU backends, and emit SRT/JSON for CER re-computation.

.DESCRIPTION
  Same methodology as scripts/bench_whisper_closeout.ps1, but only the key configs.
  Default output goes to $env:TEMP\v2w_verify so existing evidence is not overwritten.

  Configs:
    cpu_default   CPU(-ng) factory default
    gpu_default   GPU(-fa) factory default
    gpu_prod      GPU + production flags (VAD + -bo1 -bs1 -mc32 -sns -nf + prompt + carry)
    gpu_nocarry   GPU + production flags without --carry-initial-prompt
    cpu_prod      CPU + production flags
    gpu_prod_r2   second run of gpu_prod, to gauge wall-clock noise

  -McAb 是给「摘掉 --carry-initial-prompt 之后」的收口用的开关：它不改动默认矩阵，
  只换成 4 个点，用来判定 `-mc` 该不该按后端分流（CPU 开 / GPU 关）还是整体移除：
    cpu_nocarry_mc32  CPU + 生产参数去掉 carry（-mc 32）——A/B 的一臂
    cpu_nocarry_mc1   CPU + 生产参数去掉 carry（-mc -1，whisper 出厂默认）——A/B 的另一臂
    gpu_nocarry_mc32  GPU + 生产参数去掉 carry（-mc 32），同场重测以对齐机器负载
    gpu_nocarry_mc1   GPU + 生产参数去掉 carry（-mc -1），改动后的 GPU 生产配置

.EXAMPLE
  powershell -File scripts/bench_whisper_closeout_verify.ps1
  powershell -File scripts/bench_whisper_closeout_verify.ps1 -McAb -BenchDir "$env:TEMP\v2w_after"
  powershell -File scripts/bench_whisper_closeout_verify.ps1 -McAb -Only cpu_nocarry_mc1
#>
param(
    [int]$StartSec = 300,
    [int]$DurationSec = 600,
    [int]$Threads = 12,
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [string]$BenchDir = '',
    # 只跑「-mc 按后端分流 / 移除」这组对照与收口配置，默认矩阵不受影响
    [switch]$McAb,
    # 只跑指定的 Tag（留空表示跑该矩阵全部配置），便于分次补测
    [string[]]$Only = @()
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'resolve_paths.ps1')
$ffmpeg = Get-V2wPath -Key 'ffmpeg' -ProjectRoot $projectRoot
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
# The source video name contains non-ASCII; resolve by wildcard so this script stays ASCII-only.
$video = (Get-ChildItem -LiteralPath (Join-Path $projectRoot 'testVideo') -Filter '03.1.3*.mp4' |
    Select-Object -First 1).FullName
if (-not $BenchDir) { $BenchDir = Join-Path $env:TEMP 'v2w_verify' }
$srtDir = Join-Path $BenchDir 'srt'
$jsonDir = Join-Path $BenchDir 'json'

foreach ($p in @($ffmpeg, $cli, $model, $vadModel, $video)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "missing test file: $p" }
}
foreach ($d in @($BenchDir, $srtDir, $jsonDir)) {
    New-Item -ItemType Directory -Path $d -Force | Out-Null
}

# The Chinese prompt is built from code points; keeps this script ASCII-only so it
# parses correctly regardless of the console code page.
$promptText = [string]::Concat(
    [char]0x4EE5, [char]0x4E0B, [char]0x662F, [char]0x666E,
    [char]0x901A, [char]0x8BDD, [char]0x5F55, [char]0x97F3, [char]0x3002)

$clip = Join-Path $BenchDir "clip_${StartSec}_${DurationSec}.wav"
if (-not (Test-Path -LiteralPath $clip)) {
    Write-Host "extracting ${DurationSec}s benchmark audio ..." -ForegroundColor Cyan
    & $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
        -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clip
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg audio extract failed (exit $LASTEXITCODE)" }
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
    # Both pipes must be drained asynchronously, otherwise the 64KB Windows pipe
    # buffer blocks the child process.
    $p | Add-Member -NotePropertyName OutTask -NotePropertyValue $p.StandardOutput.ReadToEndAsync() -Force
    $p | Add-Member -NotePropertyName ErrTask -NotePropertyValue $p.StandardError.ReadToEndAsync() -Force
    return $p
}

function Get-Timing([string]$errText, [string]$name) {
    foreach ($line in ($errText -split "`n")) {
        if ($line -match ("whisper_print_timings:\s*" + $name + "\s*=\s*([0-9.]+)\s*ms")) {
            return [double]$Matches[1]
        }
    }
    return 0.0
}

$baseArgs = @('-m', $model, '-f', $clip, '-l', 'zh', '-t', "$Threads", '-p', '1', '-oj', '-osrt')
$vadArgs = @('--vad', '-vm', $vadModel, '-vt', '0.50', '-vsd', '250')
# 注意：`*_prod` 臂必须与**当前生产链路**逐字等价，否则「相对生产参数」的结论全部失真。
# 2026-10-03 复核发现这里曾残留 `--carry-initial-prompt`，而该参数早已从
# src/engines/whisper.rs 移除（见该文件注释：精度净亏 1.1~1.2 pp）。于是本脚本的
# `gpu_prod` 报出 CER 13.14%，而真实生产配置是 12.03%——差值全来自这个已删除的参数。
# 现已对齐：prod 与 nocarry 只差命名，取值与生产链路一致。
$prodArgs = @('-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf', '--prompt', $promptText)
$nocarryArgs = @('-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf', '--prompt', $promptText)
# 与 $nocarryArgs 只差 `-mc`：显式给 -1（whisper-cli 出厂默认，不裁剪上下文）。
# 显式写出而不是省略，是为了让两条对照臂在命令行上只差一个值，便于复核。
$nocarryMc1Args = @('-bo', '1', '-bs', '1', '-mc', '-1', '-sns', '-nf', '--prompt', $promptText)

$configs = @(
    @{ Tag = 'cpu_default'; Args = ($baseArgs + @('-ng')) },
    @{ Tag = 'gpu_default'; Args = ($baseArgs + @('-fa')) },
    @{ Tag = 'gpu_prod'; Args = ($baseArgs + @('-fa') + $vadArgs + $prodArgs) },
    @{ Tag = 'gpu_nocarry'; Args = ($baseArgs + @('-fa') + $vadArgs + $nocarryArgs) },
    @{ Tag = 'cpu_prod'; Args = ($baseArgs + @('-ng') + $vadArgs + $prodArgs) },
    @{ Tag = 'gpu_prod_r2'; Args = ($baseArgs + @('-fa') + $vadArgs + $prodArgs) }
)

# 「摘掉 --carry-initial-prompt 之后」的收口矩阵：用来判定 `-mc` 保留 / 分流 / 移除。
# 与默认矩阵独立，只有 -McAb 或 -Only 命中时才跑，默认用法完全不变。
$mcConfigs = @(
    @{ Tag = 'gpu_nocarry_mc32'; Args = ($baseArgs + @('-fa') + $vadArgs + $nocarryArgs) },
    @{ Tag = 'gpu_nocarry_mc1'; Args = ($baseArgs + @('-fa') + $vadArgs + $nocarryMc1Args) },
    @{ Tag = 'cpu_nocarry_mc32'; Args = ($baseArgs + @('-ng') + $vadArgs + $nocarryArgs) },
    @{ Tag = 'cpu_nocarry_mc1'; Args = ($baseArgs + @('-ng') + $vadArgs + $nocarryMc1Args) }
)

$allConfigs = @($configs) + @($mcConfigs)

if ($McAb) {
    Write-Host '[-McAb] 只跑 -mc 对照与改动后的收口配置（默认 6 配置矩阵不变）' -ForegroundColor Magenta
    $configs = $mcConfigs
}

# -Only 从「默认矩阵 + 收口矩阵」的并集里挑 Tag，因此既能只补测某个默认配置
# （如 cpu_prod），也能只补测收口配置（如 cpu_nocarry_mc1），无需改脚本。
if ($Only.Count -gt 0) {
    $known = @($allConfigs | ForEach-Object { $_.Tag })
    foreach ($t in $Only) {
        if ($known -notcontains $t) { throw "未知 Tag: $t（可选: $($known -join ', ')）" }
    }
    $configs = @($allConfigs | Where-Object { $Only -contains $_.Tag })
    Write-Host "[-Only] 只跑: $($configs.Tag -join ', ')" -ForegroundColor Magenta
}

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
    if (-not (Test-Path -LiteralPath "$prefix.srt")) { throw "$tag produced no SRT (exit $code)" }
    Copy-Item -LiteralPath "$prefix.srt" -Destination (Join-Path $srtDir "$tag.srt") -Force
    Copy-Item -LiteralPath "$prefix.json" -Destination (Join-Path $jsonDir "$tag.json") -Force

    $encRuns = 0; $decRuns = 0
    if ($errText -match 'encode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $encRuns = [int]$Matches[1] }
    if ($errText -match 'decode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $decRuns = [int]$Matches[1] }
    $gpu = if ($argv -contains '-ng') { 'cpu' } else { 'vulkan' }
    $segCount = (Select-String -LiteralPath (Join-Path $srtDir "$tag.srt") -Pattern '-->' -AllMatches).Count

    $row = [pscustomobject]@{
        Tag       = $tag
        WallSec   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        TotalSec  = [math]::Round((Get-Timing $errText 'total time') / 1000.0, 2)
        LoadSec   = [math]::Round((Get-Timing $errText 'load time') / 1000.0, 2)
        EncodeSec = [math]::Round((Get-Timing $errText 'encode time') / 1000.0, 2)
        DecodeSec = [math]::Round((Get-Timing $errText 'decode time') / 1000.0, 2)
        VadSec    = [math]::Round((Get-Timing $errText 'vad time') / 1000.0, 2)
        EncRuns   = $encRuns
        DecRuns   = $decRuns
        Segments  = $segCount
        Backend   = $gpu
        Exit      = $code
    }
    Write-Host ("{0,-13} wall {1,8:N2}s  total {2,8:N2}s  enc {3,7:N2}s/{4,4}runs  dec {5,8:N2}s/{6,5}runs  vad {7,6:N2}s  seg {8,4}  {9}" -f `
        $row.Tag, $row.WallSec, $row.TotalSec, $row.EncodeSec, $row.EncRuns, $row.DecodeSec, $row.DecRuns, $row.VadSec, $row.Segments, $row.Backend) -ForegroundColor Green
    return $row
}

$rows = @()
Write-Host "`n===== closeout verification (audio ${DurationSec}s, model $(Split-Path $ModelRelative -Leaf), -t $Threads) =====" -ForegroundColor Cyan
foreach ($c in $configs) {
    Write-Host "[run] $($c.Tag) ..." -ForegroundColor Yellow
    $rows += Invoke-Run $c.Tag $c.Args
}

Write-Host "`n===== summary (sorted by wall clock) =====" -ForegroundColor Cyan
$rows | Sort-Object WallSec | Format-Table -AutoSize
foreach ($b in @('cpu_default', 'gpu_default')) {
    $base = ($rows | Where-Object { $_.Tag -eq $b } | Select-Object -First 1).WallSec
    if ($base) {
        Write-Host "relative to $b ($base s):" -ForegroundColor Green
        foreach ($r in ($rows | Sort-Object WallSec)) {
            Write-Host ("  {0,-13} {1,8:N2}s  x{2:N2}" -f $r.Tag, $r.WallSec, ($base / $r.WallSec))
        }
    }
}
$rows | ConvertTo-Json | Out-File -FilePath (Join-Path $BenchDir 'verify_results.json') -Encoding utf8
Write-Host "`nJSON: $jsonDir" -ForegroundColor Cyan
Write-Host "CER: cargo run --offline --example eval_whisper_parallel -- `"$jsonDir`" 300 305 895" -ForegroundColor Cyan
