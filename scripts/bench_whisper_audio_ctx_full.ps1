<#
.SYNOPSIS
  Full-film -ac (audio context) ablation: same production flags as the current
  pipeline, sweeping only -ac, on the whole 0..DurationSec audio, emitting JSON
  + SRT for CER re-computation.

.DESCRIPTION
  Full-film counterpart of scripts/bench_whisper_audio_ctx.ps1 (which swept -ac
  on a 360 s clip). Everything except -ac is pinned to the current production
  flags after the --carry-initial-prompt removal:
    VAD + greedy(-bo 1 -bs 1) + -mc 32 + -sns -nf + prompt
  Backend defaults to Vulkan (-fa), the default pipeline path.

  The full-film 16 kHz mono WAV is reused from %TEMP%\v2w_full when present, so
  the audio is byte-identical to the earlier full-film run. Outputs go to a fresh
  directory (default %TEMP%\v2w_ac_full_<backend>) and never overwrite anything.

.EXAMPLE
  powershell -File scripts/bench_whisper_audio_ctx_full.ps1
  powershell -File scripts/bench_whisper_audio_ctx_full.ps1 -AudioCtx @(1000,750,512) -Backend ng
#>
param(
    [int[]]$AudioCtx = @(1500, 1000, 750, 512),
    [int]$StartSec = 0,
    [int]$DurationSec = 1942,
    [int]$Threads = 12,
    # 'fa' = Vulkan GPU (default pipeline path), 'ng' = CPU
    [ValidateSet('fa', 'ng')]
    [string]$Backend = 'fa',
    [string]$ModelRelative = 'models\whisper\ggml-small-q5_0.bin',
    [string]$BenchDir = ''
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'resolve_paths.ps1')
$ffmpeg = Get-V2wPath -Key 'ffmpeg' -ProjectRoot $projectRoot
$cli = Join-Path $projectRoot 'tools\whisper-vulkan\whisper-1.8.4-windows-x64\whisper-cli.exe'
$model = Join-Path $projectRoot $ModelRelative
$vadModel = Join-Path $projectRoot 'models\whisper\ggml-silero-v6.2.0.bin'
$video = (Get-ChildItem -LiteralPath (Join-Path $projectRoot 'testVideo') -Filter '03.1.3*.mp4' |
    Select-Object -First 1).FullName
if (-not $BenchDir) { $BenchDir = Join-Path $env:TEMP "v2w_ac_full_$Backend" }
$srtDir = Join-Path $BenchDir 'srt'
$jsonDir = Join-Path $BenchDir 'json'

foreach ($p in @($ffmpeg, $cli, $model, $vadModel, $video)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "missing test file: $p" }
}
foreach ($d in @($BenchDir, $srtDir, $jsonDir)) {
    New-Item -ItemType Directory -Path $d -Force | Out-Null
}

# Reuse the full-film baseline audio (same bytes as the v2w_full transcription).
$clipName = "clip_${StartSec}_${DurationSec}.wav"
$clipCandidates = @(
    (Join-Path $env:TEMP "v2w_full\$clipName"),
    (Join-Path $BenchDir $clipName)
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

# Chinese prompt built from code points so this script stays ASCII-only and thus
# parses correctly regardless of the console code page.
$promptText = [string]::Concat(
    [char]0x4EE5, [char]0x4E0B, [char]0x662F, [char]0x666E,
    [char]0x901A, [char]0x8BDD, [char]0x5F55, [char]0x97F3, [char]0x3002)

$baseArgs = @('-m', $model, '-f', $clip, '-l', 'zh', '-t', "$Threads", '-p', '1', '-oj', '-osrt')
$vadArgs = @('--vad', '-vm', $vadModel, '-vt', '0.50', '-vsd', '250')
# Current production flags (no --carry-initial-prompt); only -ac is the variable.
$prodArgs = @('-bo', '1', '-bs', '1', '-mc', '32', '-sns', '-nf', '--prompt', $promptText)
$devArg = if ($Backend -eq 'ng') { '-ng' } else { '-fa' }

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

$rows = @()
Write-Host "`n===== full-film -ac ablation (audio ${DurationSec}s, backend $Backend, -t $Threads) =====" -ForegroundColor Cyan
foreach ($ac in $AudioCtx) {
    $tag = if ($ac -gt 0) { "ac$ac" } else { 'ac0_default' }
    $prefix = Join-Path $BenchDir $tag
    $argv = $baseArgs + @($devArg) + $vadArgs + $prodArgs + @('-of', $prefix)
    if ($ac -gt 0) { $argv += @('-ac', "$ac") }
    $win = if ($ac -gt 0) { [math]::Round($ac * 0.02, 2) } else { 30 }
    Write-Host "[run] $tag (encode window ${win}s) ..." -ForegroundColor Yellow

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
    $segCount = (Select-String -LiteralPath (Join-Path $srtDir "$tag.srt") -Pattern '-->' -AllMatches).Count

    $rows += [pscustomobject]@{
        Tag       = $tag
        Ac        = $ac
        WindowSec = $win
        WallSec   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        TotalSec  = [math]::Round((Get-Timing $errText 'total time') / 1000.0, 2)
        EncodeSec = [math]::Round((Get-Timing $errText 'encode time') / 1000.0, 2)
        DecodeSec = [math]::Round((Get-Timing $errText 'decode time') / 1000.0, 2)
        VadSec    = [math]::Round((Get-Timing $errText 'vad time') / 1000.0, 2)
        EncRuns   = $encRuns
        DecRuns   = $decRuns
        Segments  = $segCount
        Backend   = $Backend
        Exit      = $code
    }
    Write-Host ("  wall {0,8:N2}s  enc {1,7:N2}s/{2,4}runs  dec {3,8:N2}s  seg {4,4}" -f `
        $rows[-1].WallSec, $rows[-1].EncodeSec, $rows[-1].EncRuns, $rows[-1].DecodeSec, $rows[-1].Segments) -ForegroundColor Green
}

Write-Host "`n===== summary =====" -ForegroundColor Cyan
$rows | Format-Table Tag, WindowSec, WallSec, TotalSec, EncodeSec, DecodeSec, VadSec, EncRuns, DecRuns, Segments -AutoSize
$rows | ConvertTo-Json | Out-File -FilePath (Join-Path $BenchDir 'ac_full_results.json') -Encoding utf8
Write-Host "`nCER: cargo run --offline --example eval_whisper_parallel -- `"$jsonDir`" 0 0 $DurationSec" -ForegroundColor Cyan
