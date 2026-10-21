# 打包自检：确认「会打进安装包的东西」在客户机上真的能用。
#
# 为什么需要单独一支脚本：
#   installer/voice2word.iss 的 [Files] 用了一大堆 glob + skipifsourcedoesntexist。
#   源文件不在时**什么都不报**，安装包照样能编译出来——于是「本机有、包里的没有」
#   这类缺陷在打包流程里是完全静默的。本脚本把「可选」与「必需」分开，
#   必需的缺失直接失败。
#
# 检查项：
#   1. FFmpeg 三件套（ffmpeg / ffplay / ffprobe）**能真的启动**（-version 退出码 0）
#      —— 只看文件存不存在会漏掉「缺同目录 DLL」这种必然失败；
#   2. llama.cpp / whisper.cpp 入口 exe + 同目录 DLL 齐备；
#   3. VC++ 可再发行运行库四件套；
#   4. 随包 config.toml 是**干净的默认配置**（不含绝对路径、不含密钥、
#      whisper_model 不得指向 .onnx）；
#   5. 便携 Python 能 import numpy / sherpa_onnx（SenseVoice 与标点的前提）；
#   6. 胶水脚本（punc / sensevoice / edge_tts）在 tools/ 下。
#
# 用法：
#   pwsh -File scripts/verify_package.ps1            # 校验仓库里将要打包的输入
#   pwsh -File scripts/verify_package.ps1 -Stage D:\stage   # 校验已装配的目录

param(
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$Stage = ""
)

$ErrorActionPreference = "Continue"

if ($Stage -ne "") { $root = $Stage } else { $root = $RepoRoot }
$tools = Join-Path $root "tools"
$failures = @()
$warnings = @()
$passes = @()

function Add-Fail { param([string]$m) $script:failures += $m }
function Add-Warn { param([string]$m) $script:warnings += $m }
function Add-Pass { param([string]$m) $script:passes += $m }

# 【关键】在**隔离的 PATH** 下试跑。
#
# 用当前 PATH 试跑会骗自己：打包机上 ffmpeg 的 DLL 目录往往就在 PATH 里，
# 于是 `tools/ffplay.exe`（shared 构建、缺 DLL）也能跑起来、检查通过——
# 但它单独打进安装包后在客户机上必然 0xC0000135。
# 把 PATH 收窄到「exe 自己所在目录 + System32」，才等价于客户机环境。
function Test-ExeRuns {
    param([string]$Exe)
    if (-not (Test-Path $Exe)) { return $false }
    $dir = Split-Path -Parent (Resolve-Path $Exe).Path
    $origPath = $env:PATH
    $env:PATH = "$dir;$env:SystemRoot\System32"
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = (Resolve-Path $Exe).Path
        $psi.Arguments = '-version'
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.CreateNoWindow = $true
        $proc = [System.Diagnostics.Process]::Start($psi)
        $null = $proc.StandardOutput.ReadToEnd()
        $null = $proc.StandardError.ReadToEnd()
        $proc.WaitForExit(30000) | Out-Null
        return ($proc.ExitCode -eq 0)
    } catch { return $false } finally { $env:PATH = $origPath }
}

function Test-DllsPresent {
    param([string]$Dir, [string[]]$Names)
    $missing = @()
    foreach ($n in $Names) {
        if (-not (Test-Path (Join-Path $Dir $n))) { $missing += $n }
    }
    return $missing
}

Write-Host "打包自检：$root" -ForegroundColor Cyan
Write-Host ("=" * 64)

# ── 1. FFmpeg 三件套 ────────────────────────────────────────────────
foreach ($pair in @(
        @{ File = "ffmpeg.exe"; Name = "ffmpeg"; Required = $true },
        @{ File = "ffplay.exe"; Name = "ffplay"; Required = $true },
        @{ File = "ffprobe.exe"; Name = "ffprobe"; Required = $false }
    )) {
    $exe = Join-Path $tools $pair.File
    if (Test-ExeRuns $exe) {
        Add-Pass "$($pair.Name) 可独立启动"
    } elseif (Test-Path $exe) {
        $msg = "$($pair.Name).exe 存在但**无法启动**（几乎必然是缺同目录 DLL，客户机上会 0xC0000135 秒退）"
        if ($pair.Required) { Add-Fail $msg } else { Add-Warn $msg }
    } else {
        $msg = "缺 $($pair.Name).exe"
        if ($pair.Required) { Add-Fail $msg } else { Add-Warn $msg }
    }
}

# ffplay 的共享库依赖（shared 构建才需要）
$avDlls = @(Get-ChildItem -LiteralPath $tools -Filter 'av*.dll' -ErrorAction SilentlyContinue)
$swDlls = @(Get-ChildItem -LiteralPath $tools -Filter 'sw*.dll' -ErrorAction SilentlyContinue)
if ((Test-Path (Join-Path $tools 'ffplay.exe')) -and ($avDlls.Count -eq 0) -and -not (Test-ExeRuns (Join-Path $tools 'ffplay.exe'))) {
    Add-Warn "ffplay.exe 是 shared 构建但 tools/ 下没有 av*/sw* DLL"
}

# ── 2. llama.cpp / whisper.cpp ──────────────────────────────────────
$llamaExe = Join-Path $tools 'llama-completion.exe'
$llamaDlls = @('llama.dll', 'llama-common.dll', 'llama-server-impl.dll', 'llama-completion-impl.dll', 'ggml.dll', 'ggml-base.dll')
if (Test-Path $llamaExe) {
    $missing = Test-DllsPresent -Dir $tools -Names $llamaDlls
    if ($missing.Count -eq 0) {
        Add-Pass "llama.cpp 入口 exe + 同目录 DLL 齐备"
    } else {
        Add-Fail ("llama.cpp 缺同目录 DLL：" + ($missing -join ', ') + "（客户机上会静默 0xC0000139 退出）")
    }
} else {
    Add-Warn "未打包 llama.cpp（离线翻译 / 润色需要；客户机可一键下载）"
}

$whisperDir = Join-Path $tools 'whisper-vulkan\whisper-1.8.4-windows-x64'
$whisperExe = Join-Path $whisperDir 'whisper-cli.exe'
$whisperDlls = @('whisper.dll', 'ggml.dll', 'ggml-base.dll', 'ggml-cpu.dll')
if (Test-Path $whisperExe) {
    $missing = Test-DllsPresent -Dir $whisperDir -Names $whisperDlls
    if ($missing.Count -eq 0) {
        Add-Pass "whisper.cpp 入口 exe + 同目录 DLL 齐备"
    } else {
        Add-Fail ("whisper.cpp 缺同目录 DLL：" + ($missing -join ', ') + "（客户机上会静默 0xC0000139 退出）")
    }
    # 自编译 Vulkan 版依赖显卡驱动提供的 vulkan-1.dll，虚拟机/服务器没有。
    if ((Test-Path (Join-Path $whisperDir 'ggml-vulkan.dll')) -and -not (Test-Path (Join-Path $whisperDir 'ggml-cpu.dll'))) {
        Add-Warn "whisper 目录里只有 Vulkan 构建（缺 ggml-cpu.dll）：无独显驱动的机器上会失败"
    }
} else {
    Add-Warn "未打包 whisper.cpp（Whisper 引擎需要；客户机可一键下载）"
}

# ── 3. VC++ 运行库 ──────────────────────────────────────────────────
$vcDlls = @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'vcomp140.dll')
$missing = Test-DllsPresent -Dir $tools -Names $vcDlls
if ($missing.Count -eq 0) {
    Add-Pass "VC++ 运行库四件套齐备"
} else {
    Add-Fail ("缺 VC++ 运行库：" + ($missing -join ', ') + "（干净系统上 whisper / llama 会 0xC0000139）")
}

# ── 4. 随包 config.toml ─────────────────────────────────────────────
$cfgPath = Join-Path $root 'config.toml'
if (Test-Path $cfgPath) {
    $cfg = Get-Content $cfgPath -Raw -Encoding UTF8
    $probs = @()
    if ($cfg -match '(?m)^\s*whisper_model\s*=\s*"[^"]*\.onnx"') {
        $probs += 'whisper_model 指向 .onnx（那是 SenseVoice 的模型，whisper-cli 读不了 → 退出码 0 且不写 JSON）'
    }
    if ($cfg -match '(?m)^\s*(ffmpeg|whisper_cli|llama_cli|llm_model)\s*=\s*"[A-Za-z]:') {
        $probs += '含盘符绝对路径（那是打包机的路径，客户机上必然找不到）'
    }
    if ($cfg -match '(?m)^\s*api_key\s*=\s*"[^"]+"') {
        $probs += '含 api_key（密钥绝不允许进安装包）'
    }
    if ($probs.Count -eq 0) {
        Add-Pass "config.toml 是干净的便携默认配置"
    } else {
        foreach ($p in $probs) { Add-Fail ("config.toml：" + $p) }
    }
} else {
    Add-Warn "没有随包 config.toml（程序首次启动会用内置默认值生成；若安装目录只读则设置不会持久化）"
}

# ── 5. 便携 Python ──────────────────────────────────────────────────
$pyExe = Join-Path $tools 'python\python.exe'
if (Test-Path $pyExe) {
    $out = & $pyExe -c "import numpy, sherpa_onnx; print('ok')" 2>&1
    if ($LASTEXITCODE -eq 0) {
        Add-Pass "便携 Python 可 import numpy / sherpa_onnx"
    } else {
        Add-Fail ("便携 Python 缺依赖（SenseVoice / 标点会失败）：" + ($out -join ' '))
    }
} else {
    Add-Warn "未打包便携 Python（客户机需自备完整 Python，否则 SenseVoice / 标点不可用）"
}

# ── 6. 胶水脚本 ─────────────────────────────────────────────────────
foreach ($py in @('punc_runner.py', 'sensevoice_runner.py', 'edge_tts_runner.py')) {
    if (Test-Path (Join-Path $tools $py)) {
        Add-Pass "$py 已随包"
    } else {
        Add-Fail "缺 $py（对应功能在客户机上直接不可用）"
    }
}

# ── 结果 ────────────────────────────────────────────────────────────
Write-Host ""
foreach ($p in $passes) { Write-Host "  [OK]   $p" -ForegroundColor Green }
foreach ($w in $warnings) { Write-Host "  [WARN] $w" -ForegroundColor Yellow }
foreach ($f in $failures) { Write-Host "  [FAIL] $f" -ForegroundColor Red }

Write-Host ""
Write-Host ("通过 {0} 项 / 警告 {1} 项 / 失败 {2} 项" -f $passes.Count, $warnings.Count, $failures.Count)

if ($failures.Count -gt 0) {
    Write-Host "自检未通过：这些问题会让包在客户机上必然失败，请先修复再打包。" -ForegroundColor Red
    exit 1
}
Write-Host "自检通过。" -ForegroundColor Green
exit 0