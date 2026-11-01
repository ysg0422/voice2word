<#
.SYNOPSIS
  Voice2Word 瘦包打包（release 产物 → 可直接分发的 zip）。

.DESCRIPTION
  「瘦包」只含**程序本体**，不含模型与外部工具（那些靠首次运行时的
  「模型与组件 → 一键补齐」下载）。因此包体只有 ~20MB，用户拿到即用。

  包内容：
    voice2word.exe        主程序（release）
    voice2word-cli.exe    命令行入口（release）
    *.dll                主程序所需 VC++ 运行库
    config.toml           可移植默认配置（路径全是相对路径）
    README-首次使用.txt    首次运行引导
    tools/*_runner.py     识别、标点、配音和人声分离运行器（必须随包）

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
if ($version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') { throw "Cargo.toml 版本号无效" }

$exe = Join-Path $repoRoot 'target\release\voice2word.exe'
$cli = Join-Path $repoRoot 'target\release\voice2word-cli.exe'

if (-not $SkipBuild) {
    Write-Host "[1/4] cargo build --release ..." -ForegroundColor Cyan
    Push-Location $repoRoot
    try {
        & (Join-Path $PSScriptRoot 'build_release.ps1')
        if ($LASTEXITCODE -ne 0) { throw "release 构建失败（exit=$LASTEXITCODE）" }
    } finally { Pop-Location }
}

if (-not (Test-Path -LiteralPath $exe)) {
    throw "找不到 release 产物：$exe（请先 cargo build --release）"
}
if (-not (Test-Path -LiteralPath $cli)) { throw "缺少命令行入口：$cli" }
$cliVersion = & $cli --version
if ($LASTEXITCODE -ne 0 -or ($cliVersion -join ' ') -notmatch [regex]::Escape($version)) {
    throw "release 产物版本与 Cargo.toml 不一致，请重新构建"
}

# ── 组装 staging 目录 ───────────────────────────────────────────────────────
$pkgName = "voice2word-v$version-win64-lite"
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
$stage = [IO.Path]::GetFullPath((Join-Path $OutputDir $pkgName))
# 删除前确认 staging 是指定输出目录的直接子目录，拒绝符号链接。
if ([IO.Path]::GetDirectoryName($stage).TrimEnd('\', '/') -ne $OutputDir.TrimEnd('\', '/')) {
    throw "staging 路径超出输出目录：$stage"
}
if ((Test-Path -LiteralPath $stage) -and ((Get-Item -LiteralPath $stage).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
    throw "staging 不得是符号链接：$stage"
}
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Path $stage -Force | Out-Null

Write-Host "[2/4] 复制程序本体 ..." -ForegroundColor Cyan
Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'voice2word.exe') -Force
Copy-Item -LiteralPath $cli -Destination (Join-Path $stage 'voice2word-cli.exe') -Force
foreach ($dll in @('vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'vcomp140.dll')) {
    $src = Join-Path $repoRoot "tools\$dll"
    if (-not (Test-Path -LiteralPath $src)) { throw "缺少启动运行库：$src" }
    Copy-Item -LiteralPath $src -Destination (Join-Path $stage $dll) -Force
}

# 从本次二进制生成默认值，避免把开发机配置或密钥带进便携包。
$defaults = & $cli --print-default-config
if ($LASTEXITCODE -ne 0) { throw "生成默认配置失败（exit=$LASTEXITCODE）" }
$configText = $defaults -join "`n"
if ($configText -notmatch '(?m)^whisper_model\s*=\s*"models/whisper/' -or
    $configText -match '(?m)^\s*api_key\s*=\s*"[^"]+"') {
    throw "生成的默认配置未通过便携性检查"
}
[IO.File]::WriteAllText((Join-Path $stage 'config.toml'), $configText, [Text.UTF8Encoding]::new($false))

# Python 胶水脚本随包分发，缺少任一脚本就停止打包。
$runnerDir = Join-Path $stage 'tools'
New-Item -ItemType Directory -Path $runnerDir -Force | Out-Null
foreach ($py in @('punc_runner.py', 'sensevoice_runner.py', 'edge_tts_runner.py', 'vocal_sep_runner.py')) {
    $src = Join-Path $repoRoot "tools\$py"
    if (Test-Path -LiteralPath $src) {
        Copy-Item -LiteralPath $src -Destination (Join-Path $runnerDir $py) -Force
    } else {
        throw "缺少仓库脚本 tools/$py，无法生成完整发布包"
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
$zipPart = Join-Path $OutputDir (".$pkgName." + [guid]::NewGuid().ToString('N') + '.zip')
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPart -CompressionLevel Optimal
Move-Item -LiteralPath $zipPart -Destination $zip -Force
Remove-Item -LiteralPath $stage -Recurse -Force

# ── 报告 ────────────────────────────────────────────────────────────────────
$size = (Get-Item -LiteralPath $zip).Length
Write-Host "[4/4] 完成" -ForegroundColor Green
Write-Host ("  产物: {0}" -f $zip)
Write-Host ("  体积: {0:N1} MB" -f ($size / 1MB))
Write-Host ("  版本: {0}" -f $version)
