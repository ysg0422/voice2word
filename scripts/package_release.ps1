<#
.SYNOPSIS
  Voice2Word 瘦包打包（release 产物 → 可直接分发的 zip）。

.DESCRIPTION
  「瘦包」只含**程序本体**，不含模型与外部工具（那些靠首次运行时的
  「模型与组件 → 一键补齐」下载）。因此包体只有 ~20MB，用户拿到即用。

  包内容：
    voice2word.exe        主程序（release）
    config.toml           可移植默认配置（路径全是相对路径）
    README-首次使用.txt    首次运行引导
    tools/punc_runner.py  Python 标点/识别胶水（小文件，必须随包）

  刻意**不含**：models/、tools/*.exe、tools/*.dll、testVideo/、voice2word.db、
  logs/、config.local.toml（本机专属）、target/、_whisper_build/

.PARAMETER OutputDir
  zip 输出目录，默认 <仓库根>/dist。

.PARAMETER SkipBuild
  跳过 cargo build --release（用已有产物打包，便于快速重打）。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1
#>
[CmdletBinding()]
param(
    [string]$OutputDir,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
if (-not $OutputDir) { $OutputDir = Join-Path $repoRoot 'dist' }

# ── 版本号：从 Cargo.toml 读，保证与二进制一致 ──────────────────────────────
$cargoToml = Join-Path $repoRoot 'Cargo.toml'
$version = (Select-String -LiteralPath $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
if (-not $version) { throw "无法从 Cargo.toml 读取 version" }

$exe = Join-Path $repoRoot 'target\release\voice2word.exe'

if (-not $SkipBuild) {
    Write-Host "[1/4] cargo build --release ..." -ForegroundColor Cyan
    Push-Location $repoRoot
    try { cargo build --release } finally { Pop-Location }
}

if (-not (Test-Path -LiteralPath $exe)) {
    throw "找不到 release 产物：$exe（请先 cargo build --release）"
}

# ── 组装 staging 目录 ───────────────────────────────────────────────────────
$pkgName = "voice2word-v$version-win64-lite"
$stage = Join-Path $OutputDir $pkgName
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Write-Host "[2/4] 复制程序本体 ..." -ForegroundColor Cyan
Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'voice2word.exe') -Force

# config.toml 是共享配置（可移植相对路径），随包分发
$cfg = Join-Path $repoRoot 'config.toml'
if (Test-Path -LiteralPath $cfg) {
    Copy-Item -LiteralPath $cfg -Destination (Join-Path $stage 'config.toml') -Force
} else {
    Write-Warning "仓库根没有 config.toml；程序首次运行会自动生成默认配置"
}

# Python 胶水脚本是必需的小文件（SenseVoice / CT-Punc 走它们）
$runnerDir = Join-Path $stage 'tools'
New-Item -ItemType Directory -Path $runnerDir -Force | Out-Null
foreach ($py in @('punc_runner.py', 'sensevoice_runner.py')) {
    $src = Join-Path $repoRoot "tools\$py"
    if (Test-Path -LiteralPath $src) {
        Copy-Item -LiteralPath $src -Destination (Join-Path $runnerDir $py) -Force
    } else {
        Write-Warning "缺少 $py（SenseVoice/标点功能会不可用，可在应用内一键下载）"
    }
}

# ── 首次使用引导 ────────────────────────────────────────────────────────────
$readme = @"
Voice2Word v$version — 首次使用说明
================================================

一、直接运行
   双击 voice2word.exe 即可启动（绿色版，无需安装）。

   程序会自动把配置、字幕库(voice2word.db)、日志写到
   **exe 所在目录**，与你在哪个目录启动无关。

二、首次运行需要补齐「模型与组件」（约 280MB，只需一次）
   本包是**瘦包**，不含模型与外部工具。首次启动后：

     1. 进入「性能设置」页
     2. 找到「模型与组件」卡片
     3. 点击「一键补齐缺失的 N 项」
     4. 等待下载完成（默认走 hf-mirror 免梯子镜像）

   必需项：Whisper 识别模型、whisper.cpp 识别程序、FFmpeg
   可选项：SenseVoice 模型（更快）、标点模型、离线翻译模型（Qwen）

   补齐后回到「语音转写」页即可开始使用。

三、功能概览
   语音转写 → 标点/润色 → 多语言翻译 → 剪辑校对 → 导出
   导出格式：SRT / ASS / VTT / TXT / JSON / TTML(EBU-TT-D, Netflix TTAL)
             / FCPXML / Premiere XML / 剪映草稿

四、翻译说明
   离线翻译：用本地 Qwen 小模型，免费无需联网（需下载翻译模型）
   在线翻译：在「性能设置 → 翻译」里填 API 地址/密钥/模型
             推荐点「拉取模型列表」选择，避免手写模型 id 出错

五、常见问题
   - 提示缺少组件：见上面第二步
   - 端口/权限问题：不要放在 Program Files 等需要管理员权限的目录
   - 想换机器：整个文件夹拷走即可（模型也在这个目录下）
"@
[IO.File]::WriteAllText((Join-Path $stage 'README-首次使用.txt'), $readme, (New-Object System.Text.UTF8Encoding($false)))

# ── 打 zip ──────────────────────────────────────────────────────────────────
Write-Host "[3/4] 压缩 ..." -ForegroundColor Cyan
$zip = Join-Path $OutputDir "$pkgName.zip"
if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -LiteralPath $stage -Recurse -Force

# ── 报告 ────────────────────────────────────────────────────────────────────
$size = (Get-Item -LiteralPath $zip).Length
Write-Host "[4/4] 完成" -ForegroundColor Green
Write-Host ("  产物: {0}" -f $zip)
Write-Host ("  体积: {0:N1} MB" -f ($size / 1MB))
Write-Host ("  版本: {0}" -f $version)