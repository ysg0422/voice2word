# 集成说明：命令行（CLI）与 MCP

本文档说明 Voice2Word 在图形界面之外的集成方式：无头命令行（CLI）、Claude Code / Codex Skill，以及 MCP（Model Context Protocol）服务器。所有集成默认在本机运行，音视频与识别结果不会离开本机（选用在线翻译 API 时除外）。

## 1. 命令行（CLI）

### 1.1 定位可执行文件

Windows 下按以下顺序定位 `voice2word.exe`：

1. 显式指定（MCP / 脚本参数、环境变量 `VOICE2WORD_EXE`）；
2. 仓库内 `target\release\voice2word.exe`；
3. 仓库内 `target\debug\voice2word.exe` 或仓库根 `voice2word.exe`；
4. `PATH` 中的 `voice2word`。

```powershell
# 优先 release 构建，其次 PATH
$exe = if (Test-Path .\target\release\voice2word.exe) { '.\target\release\voice2word.exe' } else { 'voice2word' }
```

### 1.2 用法

```
voice2word [--gui]
voice2word transcribe <输入文件> [-o <输出>] [--format srt|ass|vtt|json|txt|ttml|ttal]
                          [--lang <代码>] [--translate <语言>] [--model <路径>]
                          [--threads N] [--polish] [--resume] [--artifact-dir <路径>]
voice2word --help | -h
voice2word --version | -V
```

- 无参数或 `--gui`：启动图形界面（与加入 CLI 之前行为一致）。
- `transcribe`：无头转写并导出字幕。

### 1.3 参数表

| 参数 | 取值 | 说明 | 默认 |
| --- | --- | --- | --- |
| `transcribe` | — | 无头转写子命令，后接输入文件 | — |
| `<输入文件>` | 路径 | 待转写的音视频文件（必填） | — |
| `-o`, `--output` | 路径 | 输出字幕路径 | 与输入同目录、同名的 `.<格式>` |
| `--format` | `srt`/`ass`/`vtt`/`json`/`txt`/`ttml`/`ttal` | 输出格式（不区分大小写，内部转小写） | `srt` |
| `--lang`, `--language` | 语言代码，如 `zh`/`en`/`ja`/`auto` | 源语言 | 取 `config.toml` 的 `pipeline.language` |
| `--translate` | 目标语言 | 目标语言（无头模式当前仅记录，翻译阶段未接入） | 无 |
| `--model` | 路径 | 覆盖 Whisper 模型文件 | 取 `config.toml` |
| `--threads` | 正整数 | 推理线程数 | 取 `config.toml` |
| `--polish` | 布尔（不接受取值） | 强制开启润色/标点 | 取 `config.toml` |
| `--resume` | 布尔（不接受取值） | 复用阶段产物目录中已有的识别结果（断点续跑） | 关闭 |
| `--artifact-dir` | 路径 | 指定阶段产物目录 | 仅 `--resume` 时回落 `<输入目录>/.voice2word-stages/<文件名>`；否则不落盘 |
| `-h`, `--help` | — | 显示帮助 | — |
| `-V`, `--version` | — | 显示版本号 | — |

选项同时支持 `--opt=value` 内联写法（如 `--format=vtt`、`--threads=4`）。

### 1.4 输出约定

- **stderr**：进度、阶段、诊断信息（如 `[阶段] ...`、`[stage] 45%`），以及出错信息。
- **stdout**：最终结果。成功时末行为：

  ```
  字幕已写入: <绝对或相对输出路径>（共 N 段）
  ```

因此可只重定向 stdout 取结果，stderr 保留给人或日志：

```powershell
& $exe transcribe talk.mp4 -o out\talk.srt 2> progress.log
```

### 1.5 退出码语义

| 退出码 | 含义 |
| --- | --- |
| `0` | 成功（含 `--help` / `--version`） |
| `1` | 转写运行期失败（输入文件不存在、管线报错、写盘失败、未产生字幕片段等） |
| `2` | 参数错误（缺少输入文件、未知子命令/选项、非法 `--format`、`--threads` 非正整数或为 0、选项缺少取值等） |

### 1.6 示例

```powershell
# 基础转写，输出与输入同目录的 .srt
voice2word transcribe talk.mp4

# 指定输出与格式
voice2word transcribe talk.mp4 -o out\talk.vtt --format vtt

# 指定源语言与线程数并开启润色
voice2word transcribe talk.mp4 --lang zh --threads 8 --polish

# 断点续跑：复用已有阶段产物
voice2word transcribe talk.mp4 --resume

# 指定阶段产物目录（配合 --resume 使用）
voice2word transcribe talk.mp4 --resume --artifact-dir .\.voice2word-stages\talk

# 查看帮助 / 版本
voice2word --help
voice2word --version
```

## 2. Claude Code / Codex Skill

Skill 位于 `integrations/voice2word-skill/`：

```
integrations/voice2word-skill/
  SKILL.md                  # YAML frontmatter（name/description）+ 使用说明
  scripts/transcribe.ps1    # PowerShell 包装脚本（参数校验 + 友好报错）
```

`SKILL.md` 包含：何时使用、如何定位 `voice2word.exe`、常用命令示例、参数说明、退出码语义、stdout 结果约定、常见错误排查。

### 2.1 安装方式

- **Claude Code**：把 `integrations/voice2word-skill/` 整个目录复制到用户级 skills 目录 `~/.claude/skills/voice2word-skill/`，或项目级 `.claude/skills/voice2word-skill/`。
- **Codex**：复制到 `$CODEX_HOME/skills/voice2word-skill/`（Windows 默认 `C:\Users\<用户名>\.codex\skills\`）。

```powershell
# 以 Claude Code 用户级为例
$dst = Join-Path $HOME '.claude\skills\voice2word-skill'
New-Item -ItemType Directory -Force -Path $dst | Out-Null
Copy-Item -Recurse -Force .\integrations\voice2word-skill\* $dst
```

### 2.2 使用包装脚本

```powershell
.\integrations\voice2word-skill\scripts\transcribe.ps1 -Input talk.mp4 -Format vtt -Language zh -Threads 8

# 断点续跑并指定阶段产物目录
.\integrations\voice2word-skill\scripts\transcribe.ps1 -Input talk.mp4 -Resume -ArtifactDir .\.voice2word-stages\talk
```

脚本会校验输入文件、格式与线程数，自动定位 `voice2word.exe`，把进度（stderr）透传，并打印结果路径；退出码与 CLI 一致（0/1/2）。

## 3. MCP 服务器

MCP 服务器位于 `integrations/mcp-server/`：

```
integrations/mcp-server/
  voice2word_mcp.py    # 纯标准库 stdio JSON-RPC 2.0 服务器
  README.md            # 注册与手动测试说明
  test_protocol.py     # unittest 协议测试
```

### 3.1 暴露的工具

| 工具 | 说明 | 主要入参 |
| --- | --- | --- |
| `transcribe` | 调用 `voice2word transcribe ...` 并返回结果 | `input`（必填）、`output`、`format`、`language`、`threads`、`polish`、`resume`、`artifact_dir` |
| `probe` | 探测 `ffprobe`/`ffmpeg` 是否可用 | 无 |
| `version` | 返回 `voice2word` 版本号 | 无 |

协议覆盖：`initialize`、`notifications/initialized`、`tools/list`、`tools/call`、`ping`；未知方法返回 `-32601`，参数非法返回 `-32602`，解析失败返回 `-32700`。

### 3.2 注册方式

在 MCP 客户端（Claude Desktop / Claude Code / Codex）的配置里加入 `mcpServers` 片段，`command` 使用 `python`，`args` 指向本仓库中的脚本（请按本机实际路径填写）：

```json
{
  "mcpServers": {
    "voice2word": {
      "command": "python",
      "args": ["<仓库路径>\\integrations\\mcp-server\\voice2word_mcp.py"],
      "env": { "VOICE2WORD_EXE": "<仓库路径>\\target\\release\\voice2word.exe" }
    }
  }
}
```

`env.VOICE2WORD_EXE` 可选；未设置时服务器会按相对脚本位置自动定位仓库内的可执行文件，其次回退到 `PATH`。

### 3.3 手动测试

```powershell
# 逐行输入 JSON 请求，观察 stdout 响应
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"manual","version":"0"}}}' | python integrations\mcp-server\voice2word_mcp.py
```

更多示例（`tools/list`、`tools/call`）见 `integrations/mcp-server/README.md`。

## 4. 安全与权限注意事项

- **本地优先**：CLI、Skill 与 MCP 服务器都在本机运行，转写产物写入本机路径；仅在配置在线翻译 API 时才会联网。
- **路径与参数校验**：包装脚本与 MCP 服务器会校验输入文件、格式与线程数；输出路径由调用方决定，请避免覆盖重要文件。
- **可执行文件来源**：请确认 `voice2word.exe` 来自本仓库可信构建；MCP 客户端会以当前用户权限启动该进程。
- **最小权限**：MCP 服务器只暴露转写/探测/版本三个工具，不提供任意命令执行入口；如需限制，可在客户端侧收紧工具权限。
- **敏感数据**：转写文本可能包含隐私内容，导出与日志请按所在环境的合规要求妥善保存；`config.toml` 中的 API Key 建议留空并改用环境变量 `VOICE2WORD_API_KEY`。