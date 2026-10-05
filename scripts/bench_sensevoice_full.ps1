<#
.SYNOPSIS
  全片 SenseVoice 转写 + SRT 导出，供 examples/eval_gold.rs 算全片 CER。

.DESCRIPTION
  直接调用生产用的 tools/sensevoice_runner.py（即 src/engines/sensevoice.rs 走的同一条
  单进程路径，parallel_workers=0），输入复用 %TEMP%\v2w_full\clip_0_1942.wav 这份全片
  16 kHz 单声道 WAV。输出 JSON 与 SRT 落到独立目录，不覆盖任何已有 %TEMP%\v2w_* 产物。

.EXAMPLE
  powershell -File scripts/bench_sensevoice_full.ps1
#>
param(
    [int]$StartSec = 0,
    [int]$DurationSec = 1942,
    [int]$Threads = 16,
    [string]$Language = 'zh',
    [string]$BenchDir = ''
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'resolve_paths.ps1')
$ffmpeg = Get-V2wPath -Key 'ffmpeg' -ProjectRoot $projectRoot
$runner = Join-Path $projectRoot 'tools\sensevoice_runner.py'
$model = Join-Path $projectRoot 'models\sensevoice\model.int8.onnx'
$tokens = Join-Path $projectRoot 'models\sensevoice\tokens.txt'
$vadModel = Join-Path $projectRoot 'models\sensevoice\silero_vad.onnx'
$video = (Get-ChildItem -LiteralPath (Join-Path $projectRoot 'testVideo') -Filter '03.1.3*.mp4' |
    Select-Object -First 1).FullName
if (-not $BenchDir) { $BenchDir = Join-Path $env:TEMP 'v2w_sv_full' }
New-Item -ItemType Directory -Path $BenchDir -Force | Out-Null

foreach ($p in @($ffmpeg, $runner, $model, $tokens, $vadModel, $video)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "missing test file: $p" }
}

# 复用全片基准音频（与 Whisper 全片转写同一份字节）
$clipCandidates = @(
    (Join-Path $env:TEMP "v2w_full\clip_${StartSec}_${DurationSec}.wav"),
    (Join-Path $BenchDir "clip_${StartSec}_${DurationSec}.wav")
)
$clip = $clipCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
if (-not $clip) {
    $clip = $clipCandidates[1]
    Write-Host "extracting ${DurationSec}s benchmark audio ..." -ForegroundColor Cyan
    & $ffmpeg -hide_banner -loglevel error -y -ss $StartSec -t $DurationSec -i $video `
        -map '0:a:0' -vn -sn -dn -ar 16000 -ac 1 -c:a pcm_s16le $clip
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg audio extract failed (exit $LASTEXITCODE)" }
}
Write-Host "audio: $clip" -ForegroundColor Cyan

$outJson = Join-Path $BenchDir 'sensevoice_full.json'
$outSrt = Join-Path $BenchDir 'sensevoice_full.srt'
$errLog = Join-Path $BenchDir 'sensevoice_full.err'
$stdoutLog = Join-Path $BenchDir 'sensevoice_full.stdout'
$tmpJson = Join-Path $BenchDir 'tmp_out.json'

$python = 'python'
$sw = [System.Diagnostics.Stopwatch]::StartNew()
& $python $runner --model $model --tokens $tokens --vad-model $vadModel `
    --input $clip --output $tmpJson --threads $Threads --language $Language `
    --total-duration $DurationSec 1> $stdoutLog 2> $errLog
$code = $LASTEXITCODE
$sw.Stop()
if ($code -ne 0) { throw "SenseVoice runner failed (exit $code); see $errLog" }
Move-Item -LiteralPath $tmpJson -Destination $outJson -Force

& $python (Join-Path $projectRoot 'scripts\sensevoice_json_to_srt.py') $outJson $outSrt
if ($LASTEXITCODE -ne 0) { throw "SRT 转换失败" }

$json = Get-Content -LiteralPath $outJson -Raw -Encoding UTF8 | ConvertFrom-Json
$chars = ($json.segments | ForEach-Object { $_.text }) -join ''
$row = [pscustomobject]@{
    Tag      = 'sensevoice_full'
    WallSec  = [math]::Round($sw.Elapsed.TotalSeconds, 2)
    RunnerElapsedSec = $json.elapsed_sec
    Segments = @($json.segments).Count
    Chars    = $chars.Length
}
$row | Format-List
$row | ConvertTo-Json | Out-File -FilePath (Join-Path $BenchDir 'sensevoice_full_results.json') -Encoding utf8
Write-Host "`nCER: cargo run --offline --example eval_gold -- `"$outSrt`"" -ForegroundColor Cyan
