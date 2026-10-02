# 本脚本记录的是历史消融组合；--carry-initial-prompt 已于 2026-09-29 从默认参数链移除（实测只换来 1% 以内且符号不稳定的速度差，却让 CER 稳定高 1.1~1.2 个百分点），此处保留该参数仅用于复现历史组合。
<#
.SYNOPSIS
  Whisper 解码速度「算法层」A/B 基准：在固定素材上对比不同解码/预处理策略的
  端到端耗时、编码器窗口数、解码 token 数，并对同一组参数重复多次取中位数，
  以抵抗本机（AMD 核显 + 后台进程）的调度噪声。

.DESCRIPTION
  与 bench_whisper_speed.ps1 / bench_whisper_audio_ctx.ps1 的区别：
    - 本脚本把「重复采样 + 中位数」作为一等公民，因为实测同一配置多次运行
      总耗时抖动可达 ±30%，单次采样无法用于判定算法收益。
    - 同时统计 token 分类（时间戳 token / 文本 token / 特殊 token），用于判断
      解码 token 到底花在哪里。

  默认生产参数与 src/engines/whisper.rs 完全一致，确保 A/B 只改被测变量。

.EXAMPLE
  powershell -File scripts/bench_algo_ab.ps1 -Tag base -Clip target\whisper_speed_bench\clip_300_600_x1_00.wav -Repeats 3
  powershell -File scripts/bench_algo_ab.ps1 -Tag ac1024 -Extra @('-ac','1024') -Repeats 3
#>
param(
    [string]$Tag = 'base',
    [string]$Clip = 'target\whisper_speed_bench\clip_300_600_x1_00.wav',
    [string[]]$Extra = @(),
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [int]$Threads = 12,
    [int]$Processors = 1,
    [double]$VadThreshold = 0.50,
    [switch]$NoVad,
    [switch]$NoCarry,
    [int]$Repeats = 3,
    [switch]$KeepJson
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$clipPath = if ([System.IO.Path]::IsPathRooted($Clip)) { $Clip } else { Join-Path $projectRoot $Clip }
$benchDir = Join-Path $projectRoot 'target\algo_ab'
$utf8 = New-Object System.Text.UTF8Encoding($false)
New-Item -ItemType Directory -Force -Path $benchDir | Out-Null

foreach ($p in @($cli, $model, $clipPath)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "缺少文件: $p" }
}

# .ps1 在 PS 5.1 下按 ANSI 读取，中文提示词必须用码点构造，否则会变成乱码
$promptText = [string]::Concat(
    [char]0x4EE5, [char]0x4E0B, [char]0x662F, [char]0x666E,
    [char]0x901A, [char]0x8BDD, [char]0x5F55, [char]0x97F3, [char]0x3002)

function Invoke-Once([int]$run) {
    $prefix = Join-Path $benchDir "$Tag`_r$run"
    $argv = @('-m', $model, '-f', $clipPath, '-l', 'zh', '-t', "$Threads", '-p', "$Processors",
        '-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf', '-ng',
        '-ojf', '-of', $prefix)
    if (-not $NoVad) {
        $argv += @('--vad', '-vm', $vadModel, '-vt', ('{0:0.00}' -f $VadThreshold), '-vsd', '250')
    }
    $argv += @('--prompt', $promptText)
    if (-not $NoCarry) { $argv += '--carry-initial-prompt' }
    $argv += $Extra

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
    $errText = $errTask.Result
    [System.IO.File]::WriteAllText("$prefix.err", $errText, $utf8)
    if ($proc.ExitCode -ne 0) { Write-Warning "$Tag r$run 退出码 $($proc.ExitCode)" }

    function Get-Timing([string]$name) {
        foreach ($line in ($errText -split "`n")) {
            if ($line -match ("whisper_print_timings:\s*" + $name + "\s*=\s*([0-9.]+)\s*ms")) { return [double]$Matches[1] }
        }
        return 0.0
    }
    $encRuns = 0
    if ($errText -match 'encode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $encRuns = [int]$Matches[1] }
    $decRuns = 0
    if ($errText -match 'decode time =\s*[0-9.]+ ms /\s*(\d+) runs') { $decRuns = [int]$Matches[1] }

    $json = [System.IO.File]::ReadAllText("$prefix.json", $utf8) | ConvertFrom-Json
    $items = @($json.transcription)
    $tsTok = 0; $textTok = 0; $special = 0
    $sb = New-Object System.Text.StringBuilder
    foreach ($item in $items) {
        $null = $sb.Append([string]$item.text)
        foreach ($tk in @($item.tokens)) {
            $t = [string]$tk.text
            if ($t -match '^\[_TT_\d+\]$' -or $t -match '^\[_BEG_\]$') { $tsTok++ }
            elseif ($t -match '^\[') { $special++ }
            else { $textTok++ }
        }
    }
    # 文本指纹：字数 + 内容哈希（SHA1 前 8 位）。用于判定 -ac / VAD 等
    # 参数是否静默丢了音频——耗时变快但指纹变了就是丢内容，必须否决。
    $fullText = $sb.ToString()
    $sha = [System.Security.Cryptography.SHA1]::Create()
    $hashBytes = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($fullText))
    $textHash = (($hashBytes[0..3] | ForEach-Object { $_.ToString('x2') }) -join '')
    if (-not $KeepJson) { Remove-Item "$prefix.json" -ErrorAction SilentlyContinue }

    return [pscustomobject]@{
        Run       = $run
        WallSec   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        EncodeSec = [math]::Round((Get-Timing 'encode time') / 1000.0, 2)
        DecodeSec = [math]::Round((Get-Timing 'decode time') / 1000.0, 2)
        PromptSec = [math]::Round((Get-Timing 'prompt time') / 1000.0, 2)
        VadSec    = [math]::Round((Get-Timing 'vad time') / 1000.0, 2)
        EncRuns   = $encRuns
        DecRuns   = $decRuns
        Segments  = $items.Count
        TsTok     = $tsTok
        TextTok   = $textTok
        SpecialTok= $special
        Chars     = $fullText.Length
        TextHash  = $textHash
    }
}

function Get-Median($values) {
    $s = @($values | Sort-Object)
    if ($s.Count -eq 0) { return 0 }
    $mid = [int][math]::Floor($s.Count / 2)
    if ($s.Count % 2 -eq 1) { return $s[$mid] }
    return ($s[$mid - 1] + $s[$mid]) / 2.0
}

$rows = @()
for ($r = 1; $r -le $Repeats; $r++) {
    Write-Host "[$Tag] 第 $r/$Repeats 次 ..." -ForegroundColor Yellow
    $row = Invoke-Once $r
    $rows += $row
    Write-Host ("  墙钟 {0,7:N2}s  编码 {1,7:N2}s/{2,3}runs  解码 {3,7:N2}s/{4,4}runs  提示 {5,5:N2}s  句 {6,4}" -f `
        $row.WallSec, $row.EncodeSec, $row.EncRuns, $row.DecodeSec, $row.DecRuns, $row.PromptSec, $row.Segments) -ForegroundColor Green
}

$summary = [pscustomobject]@{
    Tag        = $Tag
    Clip       = (Split-Path $clipPath -Leaf)
    Repeats    = $Repeats
    WallMed    = [math]::Round((Get-Median $rows.WallSec), 2)
    WallMin    = [math]::Round(($rows.WallSec | Measure-Object -Minimum).Minimum, 2)
    EncodeMed  = [math]::Round((Get-Median $rows.EncodeSec), 2)
    DecodeMed  = [math]::Round((Get-Median $rows.DecodeSec), 2)
    PromptMed  = [math]::Round((Get-Median $rows.PromptSec), 2)
    EncRunsMed = [int](Get-Median $rows.EncRuns)
    DecRunsMed = [int](Get-Median $rows.DecRuns)
    Segments   = [int](Get-Median $rows.Segments)
    TsTok      = [int](Get-Median $rows.TsTok)
    TextTok    = [int](Get-Median $rows.TextTok)
    SpecialTok = [int](Get-Median $rows.SpecialTok)
    Chars      = [int](Get-Median $rows.Chars)
    TextHashes = (($rows.TextHash | Sort-Object -Unique) -join ',')
    Extra      = ($Extra -join ' ')
}
Write-Host "`n===== $Tag 汇总（$Repeats 次中位数）=====" -ForegroundColor Cyan
$summary | Format-List
$summary | ConvertTo-Json | Out-File -FilePath (Join-Path $benchDir "$Tag`_summary.json") -Encoding utf8
