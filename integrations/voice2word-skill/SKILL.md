---
name: voice2word
description: 把音视频转写成字幕、导出 srt/ass/vtt 等字幕格式、调用 voice2word CLI 进行无头转写时使用本技能。
---

# voice2word 技能

Voice2Word 是一个 Rust + GPUI 桌面字幕工具，同时提供无头（headless）CLI：把音频/视频转写为字幕文件。本技能说明如何定位可执行文件、调用 CLI、解读退出码与 stdout 约定，以及排查常见错误。

## 1. 何时使用

- 需要把音视频转写成字幕（srt/ass/vtt/json/txt/ttml/ttal）。
- 需要在脚本、CI 或其它自动化流程中调用 voice2word CLI。
- 需要无头转写（不开 GUI、不依赖交互界面）。
- 需要批量转写、指定语言、指定线程数或开启润色（--polish）。

## 2. 如何定位 voice2word.exe

按以下优先级顺序查找：

1. 显式指定：命令行参数 `-Exe <路径>`，或环境变量 `VOICE2WORD_EXE`。
2. 仓库内构建产物：`target\release\voice2word.exe`（相对仓库根）。
3. 仓库内调试产物或根目录副本：`target\debug\voice2word.exe`、`.\voice2word.exe`。
4. PATH 上的 `voice2word`（Get-Command voice2word）。

> 注意：若 `target\release\voice2word.exe` 是**加入 CLI 子命令之前**构建的旧产物，它不认 `transcribe`，被调用时会直接打开图形界面并长时间不返回。遇到这种情况请用 `-Exe` 显式指向 `target\debug\voice2word.exe`（当前支持 CLI 的构建），或重新执行 `cargo build --release`。封装脚本与 MCP 服务器会用「CLI 标记」筛选跳过这类旧构建。

PowerShell 定位片段（不写死绝对路径）：

```powershell
function Find-Voice2WordExe {
    param([string]$Exe)

    # 1) 显式参数
    if ($Exe -and (Test-Path -LiteralPath $Exe)) { return (Resolve-Path -LiteralPath $Exe).Path }

    # 1) 环境变量
    if ($env:VOICE2WORD_EXE -and (Test-Path -LiteralPath $env:VOICE2WORD_EXE)) {
        return (Resolve-Path -LiteralPath $env:VOICE2WORD_EXE).Path
    }

    # 2/3) 仓库内相对路径（相对于当前工作目录 = 仓库根）
    $candidates = @(
        'target\release\voice2word.exe',
        'target\debug\voice2word.exe',
        '.\voice2word.exe'
    )
    foreach ($c in $candidates) {
        if (Test-Path -LiteralPath $c) { return (Resolve-Path -LiteralPath $c).Path }
    }

    # 4) PATH
    $cmd = Get-Command voice2word -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    throw '未找到 voice2word.exe：请用 -Exe 指定，或设置 VOICE2WORD_EXE，或先 cargo build --release。'
}

$exe = Find-Voice2WordExe
& $exe --version
```

## 3. 常用命令示例

```powershell
# 基本转写（默认输出 srt）
voice2word transcribe .\samples\meeting.mp4

# 指定输出文件与格式
voice2word transcribe .\samples\meeting.mp4 -o .\out\meeting.ass --format ass

# 指定语言（如中文）
voice2word transcribe .\samples\meeting.mp4 --lang zh -o .\out\meeting.srt

# 指定线程数
voice2word transcribe .\samples\meeting.mp4 --threads 4

# 开启润色
voice2word transcribe .\samples\meeting.mp4 --polish

# 查看帮助 / 版本
voice2word --help
voice2word --version
```

说明：

- 不带参数或 `--gui` 会启动 GUI。
- 支持内联 `--opt=value` 形式，例如 `--format=vtt`、`--threads=4`。

## 4. 参数说明

| 参数 | 取值 | 说明 | 默认 |
| --- | --- | --- | --- |
| `transcribe` | 子命令 | 无头转写入口，后接输入文件 | — |
| `<输入文件>` | 路径 | 待转写的音频/视频文件；必填 | — |
| `-o` / `--output` | 路径 | 输出字幕文件路径 | 与输入同目录同名，扩展名随格式 |
| `--format` | `srt`/`ass`/`vtt`/`json`/`txt`/`ttml`/`ttal` | 输出字幕格式，输入大小写不敏感 | `srt` |
| `--lang` / `--language` | 语言代码，如 `zh`、`en` | 指定识别语言 | 取 `config.toml` 的 `pipeline.language`（示例配置为 `zh`） |
| `--translate` | 目标语言，如 `en` | 翻译目标语言；无头模式当前仅记录该参数，翻译阶段未接线 | 不翻译 |
| `--model` | 模型文件路径 | 覆盖默认 Whisper 模型文件 | 配置文件中的默认模型 |
| `--threads` | 正整数 | 推理线程数 | 取 `config.toml` 的 `pipeline.whisper_threads` |
| `--polish` | 布尔标志，不取值 | 强制开启润色/标点 | 取 `config.toml` 的 `pipeline.enable_polish`（示例配置为 `false`） |
| `--gui` | 布尔标志 | 启动图形界面 | 关闭 |
| `-h` / `--help` | 布尔标志 | 打印用法 | — |
| `-V` / `--version` | 布尔标志 | 打印版本 | — |

## 5. 退出码语义

| 退出码 | 含义 | 典型情形 |
| --- | --- | --- |
| 0 | 成功 | 正常完成转写；`--help`、`--version` 也返回 0 |
| 1 | 转写运行期失败 | 输入文件不存在、磁盘写入失败、转写流水线错误 |
| 2 | 参数错误 | 缺少输入文件、未知选项/子命令、非法 `--format`、`--threads` 非正整数或 0、选项缺少取值 |

## 6. 结果在 stdout 的约定

- 进度与诊断信息输出到 **stderr**。
- 最终结果路径输出到 **stdout**；成功时 stdout 最后一行形如：

```
字幕已写入: <路径>（共 N 段）
```

因此脚本里应当**分离两个流**，避免进度信息污染结果路径。

```powershell
# 只把 stderr 丢进日志，stdout 保留结果路径
$result = voice2word transcribe .\samples\meeting.mp4 -o .\out\meeting.srt 2> .\progress.log

# 只要最后一行（结果路径）
$last = ($result | Select-Object -Last 1)

# 判断是否成功
if ($LASTEXITCODE -eq 0) { Write-Host "OK: $last" } else { Write-Host "失败，见 progress.log" }
```

也可以直接使用封装脚本 `integrations/voice2word-skill/scripts/transcribe.ps1`（见第 8 节），它内部已经做好了流分离与退出码处理。

## 7. 常见错误排查

| 现象 | 可能原因 | 处理 |
| --- | --- | --- |
| 退出码 2，stderr 含 `参数错误: 缺少输入文件` | 未提供输入文件 | 补上输入文件路径；`transcribe` 后必须跟文件 |
| 退出码 2，stderr 含 `参数错误: 未知选项: ...` | 未知选项或子命令 | 检查拼写；用 `--help` 查看支持项 |
| 退出码 2，stderr 含 `不支持的输出格式` | `--format` 非法 | 只能取 srt/ass/vtt/json/txt/ttml/ttal |
| 退出码 2，stderr 含 `需要正整数` / `需要大于 0 的整数` | `--threads` 非正整数或 0 | 传正整数，如 `--threads 4` |
| 退出码 2，stderr 含 `缺少取值` | 选项后没跟值 | 如 `-o`、`--format` 后补值 |
| 退出码 2，stderr 含 `多余的参数` | 位置参数过多 | 只保留一个输入文件 |
| 退出码 1，提示输入文件不存在 | 路径错误或文件缺失 | 用 `Test-Path` 确认路径，注意相对路径基于当前工作目录 |
| 退出码 1，提示写盘失败 | 输出目录不存在或无权限 | 先创建输出目录；检查权限与磁盘空间 |
| 启动即报 config.toml 缺失 | 配置文件缺失 | 在仓库/工作目录下恢复 `config.toml`（含模型路径等配置） |
| 报模型未下载 / 找不到模型文件 | 默认 Whisper 模型未下载 | 下载模型，或用 `--model <路径>` 指定已有模型文件 |
| 报 ffmpeg 缺失 / 无法解码 | 系统没有 ffmpeg 或不在 PATH | 安装 ffmpeg 并加入 PATH；确认 `Get-Command ffmpeg` 可用 |
| stdout 被进度信息污染 | 未分离 stderr/stdout | 用 `2> progress.log` 重定向，或改用 `scripts/transcribe.ps1`；只取 stdout 最后一行作为结果路径 |
| 进程长时间不返回、弹出 GUI 窗口、stdout 无任何输出、退出码却是 0 | 定位到的 exe 是**旧构建**：它早于 CLI 子命令，会忽略全部参数直接开 GUI | 重新构建（`cargo build --release`），或显式传 `-Exe .\target\debug\voice2word.exe`。封装脚本遇到"退出码 0 但没有成功行"会以退出码 1 报错，不会静默假成功 |
| 报 `invalid model data (bad magic)` / 加载模型失败 | `config.toml` 里的 `whisper_model` 指向了非 Whisper 的模型文件（例如 SenseVoice 的 `.onnx`） | 用 `--model <Whisper ggml 模型>` 或封装脚本的 `-Model` 覆盖，例如 `models\whisper\ggml-base.bin` |

## 8. 封装脚本与安装位置

- 仓库内提供了封装脚本 `integrations/voice2word-skill/scripts/transcribe.ps1`，已内置 exe 定位、stderr/stdout 分离与退出码处理：

```powershell
# 在仓库根执行
.\integrations\voice2word-skill\scripts\transcribe.ps1 -Input .\samples\meeting.mp4 -Format srt -Language zh

# 也可显式指定 exe
.\integrations\voice2word-skill\scripts\transcribe.ps1 -Input .\samples\meeting.mp4 -Exe .\target\release\voice2word.exe

# 覆盖模型文件（config.toml 的 whisper_model 不可用时）
.\integrations\voice2word-skill\scripts\transcribe.ps1 -Input .\samples\meeting.mp4 -Format vtt -Model .\models\whisper\ggml-base.bin
```

- 封装脚本参数：`-Input`（必填，位置 0）、`-Output`、`-Format`、`-Language`、`-Model`、`-Translate`、`-Threads`、`-Polish`、`-Exe`。
- 封装脚本的退出码与 CLI 保持一致：0 成功 / 1 运行期失败 / 2 参数错误；本地参数校验（输入不存在、`-Format` 非法、`-Threads` 非正整数、`-Model`/`-Exe` 指向不存在的文件）不通过时同样返回 2。
- 特例：CLI 返回 0 却没有输出成功行时，脚本以退出码 1 报错（原因与处理见第 7 节），不会静默假成功。
- 成功时脚本在回显 stdout 之后追加一行 `Result path: <路径>`，便于直接抓取结果路径。

- 该技能目录可整体安装到：
  - `~/.claude/skills/voice2word-skill/`
  - `$CODEX_HOME/skills/voice2word-skill/`

安装后即可在对应环境中按本技能文档调用 voice2word。