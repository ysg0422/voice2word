# ─────────────────────────────────────────────────────────────────────────────
# 【已暂停 · 2026-09-29】
# 本脚本依赖 tools/whisper-186-test/ 目录下的可执行文件（whisper-vad-speech-segments.exe /
# whisper-quantize.exe 等）。该目录已不在本仓库中，脚本当前无法运行。
# 它对应的支线（外置 Silero VAD 预压缩 / 模型量化）已由用户确认按「依赖缺失，暂停」结案；
# 这两条支线的产出并未丢失：Q5 量化模型本身已在生产中正常使用。
# 若日后把 tools/whisper-186-test/ 补回仓库，可直接重跑本脚本。
# ─────────────────────────────────────────────────────────────────────────────
param(
    [ValidateSet('q4_0', 'q5_0', 'q5_k', 'q8_0')]
    [string]$Type = 'q5_0',
    [string]$Source = 'models\whisper\ggml-small.bin',
    [string]$Destination = ''
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$quantizer = Join-Path $root 'tools\whisper-186-test\Release\whisper-quantize.exe'
$sourcePath = Join-Path $root $Source
if (-not $Destination) {
    $stem = [IO.Path]::GetFileNameWithoutExtension($Source)
    $Destination = "models\whisper\${stem}-${Type}.bin"
}
$destinationPath = Join-Path $root $Destination

foreach ($path in @($quantizer, $sourcePath)) {
    if (-not (Test-Path -LiteralPath $path)) {
        throw "缺少文件: $path"
    }
}

& $quantizer $sourcePath $destinationPath $Type
if ($LASTEXITCODE -ne 0) {
    throw "whisper-quantize 退出码: $LASTEXITCODE"
}
Get-Item -LiteralPath $destinationPath | Select-Object FullName, Length, LastWriteTime
