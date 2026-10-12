# Voice2Word MCP 服务器

`integrations/mcp-server/voice2word_mcp.py` 是一个把 **Voice2Word** 命令行能力暴露给任意
MCP（Model Context Protocol）客户端的轻量服务器：Claude Desktop、Claude Code、Codex
等都能通过它直接调用本机的转写 / 环境探测 / 版本查询能力。

## 1. 简介

- **纯 Python 标准库实现**：不依赖 `mcp`、`fastmcp` 等第三方包，任何装有 Python 3 的机器都能直接跑。
- **stdio JSON-RPC 2.0**：stdin 逐行读 JSON 请求，stdout 逐行写 JSON 响应；**每行一个完整 JSON**
  （JSON Lines / NDJSON），请求之间用换行分隔，不能跨行拆分。
- **通知不回响应**：没有 `id` 的消息（例如 `notifications/initialized`）按通知处理，服务器不输出任何内容。
- **无状态、无端口**：不监听网络端口，不写临时状态，进程退出即结束。

启动方式：

```powershell
python integrations\mcp-server\voice2word_mcp.py
```

可选参数：

```powershell
# 显式指定 voice2word 可执行文件
python integrations\mcp-server\voice2word_mcp.py --exe <repo>\target\release\voice2word.exe
```

也可以用环境变量 `VOICE2WORD_EXE` 达到同样效果（详见第 6、7 节）。

自检（跑一遍内置协议用例后退出，不进入 stdio 循环）：

```powershell
python integrations\mcp-server\voice2word_mcp.py --self-test
```

## 2. 注册配置

MCP 客户端通过 `command` + `args` 拉起本服务器，因此注册信息本质上就是
「用哪个解释器、跑哪个脚本」。

> `<repo>` 是**占位符**，请替换成你自己的仓库绝对路径，例如 `C:\code\Voice2Word`。
> Windows 路径在 JSON 里必须把反斜杠转义成 `\\`；直接写正斜杠 `/` 也完全可用
> （例如 `"<repo>/integrations/mcp-server/voice2word_mcp.py"`），推荐用正斜杠，最省心。

### 2.1 Claude Desktop

配置文件：`claude_desktop_config.json`（Windows 通常在
`%APPDATA%\Claude\claude_desktop_config.json`；具体位置**以你的客户端版本为准**）。

```json
{
  "mcpServers": {
    "voice2word": {
      "command": "python",
      "args": ["<repo>\\integrations\\mcp-server\\voice2word_mcp.py"]
    }
  }
}
```

如需固定可执行文件，追加 `env`：

```json
{
  "mcpServers": {
    "voice2word": {
      "command": "python",
      "args": ["<repo>\\integrations\\mcp-server\\voice2word_mcp.py"],
      "env": { "VOICE2WORD_EXE": "<repo>\\target\\release\\voice2word.exe" }
    }
  }
}
```

改完配置需要**重启客户端**才会重新加载。

### 2.2 Claude Code

Claude Code 读取项目根目录的 `.mcp.json`（也可用 `claude mcp add` 写入用户级配置），
JSON 结构与上面完全一致：

```json
{
  "mcpServers": {
    "voice2word": {
      "command": "python",
      "args": ["<repo>/integrations/mcp-server/voice2word_mcp.py"]
    }
  }
}
```

等价的一行命令（把 `<repo>` 换成真实路径）：

```powershell
claude mcp add voice2word -- python <repo>/integrations/mcp-server/voice2word_mcp.py
```

配置文件名与作用域（项目级 / 用户级）在不同版本间有差异，**以你的客户端版本为准**。

### 2.3 Codex

Codex 的 MCP 服务器配置放在 `~/.codex/config.toml` 的 `[mcp_servers]` 段落中。
先给一份与其它客户端同构的 JSON 片段（便于对照）：

```json
{
  "mcpServers": {
    "voice2word": {
      "command": "python",
      "args": ["<repo>/integrations/mcp-server/voice2word_mcp.py"]
    }
  }
}
```

再给 **TOML 写法**（这是 Codex 实际读取的格式）：

```toml
[mcp_servers.voice2word]
command = "python"
args = ["<repo>/integrations/mcp-server/voice2word_mcp.py"]
# 可选：固定可执行文件，等价于命令行 --exe
# env = { VOICE2WORD_EXE = "<repo>/target/release/voice2word.exe" }
```

如果 `python` 不在 PATH 中，把 `command` 换成解释器绝对路径，例如
`command = "<python 安装目录>/python.exe"`。`config.toml` 的确切字段名与所在位置
**以你的 Codex 版本为准**。

## 3. 手动测试

服务器是纯 stdio 协议，所以可以直接用管道喂请求、看响应。

准备一个 `requests.jsonl`，**每行一个 JSON 请求**：

```jsonl
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"manual-test","version":"0.0.1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"probe","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"version","arguments":{}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"transcribe","arguments":{"input":"<仓库根>/samples/talk.mp4","output":"out/talk.srt","format":"srt","language":"zh","threads":4,"polish":true}}}
```

在 PowerShell 中执行：

```powershell
Get-Content requests.jsonl | python integrations\mcp-server\voice2word_mcp.py
```

也可以直接内联两行请求：

```powershell
$lines = @(
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"manual-test","version":"0.0.1"}}}',
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
)
$lines | python integrations\mcp-server\voice2word_mcp.py
```

`stdout` 中每个响应同样占一行。示例响应：

**`initialize`**

```json
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"voice2word-mcp","version":"0.1.0"}}}
```

**`tools/list`**（恰好 3 个工具）

```json
{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"transcribe","description":"...","inputSchema":{"type":"object","properties":{"input":{"type":"string"},"output":{"type":"string"},"format":{"type":"string","enum":["srt","ass","vtt","json","txt","ttml","ttal"]},"language":{"type":"string"},"threads":{"type":"integer","minimum":1},"polish":{"type":"boolean"},"resume":{"type":"boolean"},"artifact_dir":{"type":"string"}},"required":["input"]}},{"name":"probe","description":"...","inputSchema":{"type":"object","properties":{}}},{"name":"version","description":"...","inputSchema":{"type":"object","properties":{}}}]}}
```

**`tools/call` → `probe`**

```json
{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"{\"ffprobe\": {\"available\": true, \"path\": \"...\"}, \"ffmpeg\": {\"available\": true, \"path\": \"...\"}}"}],"isError":false}}
```

**`tools/call` → `transcribe`**

```json
{"jsonrpc":"2.0","id":5,"result":{"content":[{"type":"text","text":"字幕已写入: out/talk.srt（共 128 段）"}],"isError":false}}
```

失败时同样是 `result`，但 `isError` 为 `true`：

```json
{"jsonrpc":"2.0","id":5,"result":{"content":[{"type":"text","text":"转写失败: ..."}],"isError":true}}
```

> 说明：`initialize` 的 `serverInfo` 字段与各工具的 `description` 文案以实际实现为准，
> 上例仅示意结构。`notifications/initialized` 是通知，**不会**产生任何输出行。

### 结果结构

所有 `tools/call` 的成功/失败都返回同一外壳：

```json
{
  "content": [{ "type": "text", "text": "<文本内容>" }],
  "isError": false
}
```

- `content`：内容块数组，本服务器只产出 `type: "text"`。
- `isError`：`false` 表示工具正常完成，`true` 表示工具内部执行失败（错误详情在 `text` 里）。

## 4. 工具说明

服务器恰好暴露 **3 个工具**。

### 4.1 `transcribe`

调用 `voice2word` CLI 转写音视频并导出字幕。参数直接映射到 CLI 选项，
未提供的选项由 CLI / `config.toml` 的默认值决定。

| 参数 | 类型 | 必填 | 说明 | 对应 CLI 选项 |
| --- | --- | --- | --- | --- |
| `input` | string | 是 | 输入音视频文件路径 | 位置参数 `<输入文件>` |
| `output` | string | 否 | 输出字幕文件路径；缺省时与输入同目录、同名、换成目标格式后缀 | `-o` / `--output` |
| `format` | string（枚举 `srt` / `ass` / `vtt` / `json` / `txt` / `ttml` / `ttal`） | 否 | 输出字幕格式，默认 `srt` | `--format` |
| `language` | string | 否 | 源语言代码（如 `zh` / `en` / `ja` / `auto`），默认取 `config.toml` | `--lang` / `--language` |
| `threads` | integer（最小值 1） | 否 | 推理线程数，覆盖 `config.toml` | `--threads` |
| `polish` | boolean | 否 | 强制开启润色 / 标点，默认沿用 `config.toml` | `--polish` |
| `resume` | boolean | 否 | 复用阶段产物目录中已有的识别结果（断点续跑） | `--resume` |
| `artifact_dir` | string | 否 | 阶段产物目录；缺省时仅在 `resume` 为真时回落到 `<输入目录>/.voice2word-stages/<文件名>` | `--artifact-dir` |

`inputSchema` 要点：`type = "object"`，`required = ["input"]`，`threads` 带 `minimum: 1`，
`format` 带 `enum` 白名单。

调用示例：

```json
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"transcribe","arguments":{"input":"<仓库根>/samples/talk.mp4","format":"vtt","language":"zh","threads":4}}}
```

### 4.2 `probe`

无参数（`inputSchema`：`type = "object"`，`properties = {}`）。用 `shutil.which`
探测本机 `ffprobe` / `ffmpeg` 是否可用，并返回可用性与解析到的路径。
适合在正式转写前确认外部依赖是否就绪。

返回的 `text` 是一段 JSON 字符串，形如：

```json
{
  "ffprobe": { "available": true, "path": "..." },
  "ffmpeg": { "available": true, "path": "..." }
}
```

未探测到某项时对应 `available` 为 `false`，`path` 为 `null`（以实际实现为准）。

| 参数 | 类型 | 必填 | 说明 | 对应 CLI 选项 |
| --- | --- | --- | --- | --- |
| （无） | — | — | 不接受任何参数，传入的字段会被忽略 | — |

### 4.3 `version`

无参数。运行 `voice2word --version`，把版本字符串原样返回，形如 `voice2word 0.1.9`。

| 参数 | 类型 | 必填 | 说明 | 对应 CLI 选项 |
| --- | --- | --- | --- | --- |
| （无） | — | — | 不接受任何参数 | `--version` / `-V` |

## 5. 支持的方法与错误码

| 方法 | 说明 |
| --- | --- |
| `initialize` | 握手，返回协议版本、能力（`tools`）与服务器信息 |
| `notifications/initialized` | 通知，服务器**忽略**且不返回任何内容 |
| `ping` | 存活探测，`result` 为空对象 `{}` |
| `tools/list` | 列出全部 3 个工具及其 `inputSchema` |
| `tools/call` | 调用指定工具，返回统一的 `content` / `isError` 结构 |

| 错误码 | 含义 | 触发场景 |
| --- | --- | --- |
| `-32700` | 解析失败（Parse error） | 某行不是合法 JSON |
| `-32601` | 未知方法（Method not found） | `method` 不在上表 |
| `-32602` | 参数非法（Invalid params） | `tools/call` 缺少 `name`、工具名未知、`transcribe` 的 `arguments` 不是对象或缺少必填 `input`，以及 `output`/`format`/`language`/`threads`/`polish`/`resume`/`artifact_dir` 类型或取值非法 |

> `probe` 与 `version` 不接受参数：它们会忽略传入的 `arguments`，不会因为多余字段返回 `-32602`。

错误响应形如：

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"未知方法: foo"}}
```

## 6. 可执行文件查找顺序

`transcribe` 与 `version` 都需要一个 `voice2word` 可执行文件。服务器按以下顺序查找，
**命中即止**：

1. 启动参数 `--exe <路径>`；
2. 环境变量 `VOICE2WORD_EXE`；
3. 仓库内 `target/release/voice2word.exe`；
4. 仓库内 `target/debug/voice2word.exe`；
5. 仓库根目录下的 `voice2word.exe`；
6. PATH 上的 `voice2word`。

因此开发期直接 `cargo build --release` 后无需任何配置即可被找到；
若把可执行文件放到别处，用 `--exe` 或 `VOICE2WORD_EXE` 显式指定：

补充说明：

- 第 1、2 项（`--exe` / `VOICE2WORD_EXE`）属于**显式指定**，只要文件存在就直接采用，不再做任何检查。
- 第 3~6 项是自动候选。若某个候选存在但**不支持 CLI**（例如仓库里遗留的、只带图形界面的旧构建：
  它不认 `transcribe` 子命令，被选中后会弹窗挂住），会被自动跳过，改用顺序中的下一个候选。
  跳过判断是纯读文件检查（比对二进制里的 CLI 标记字符串），**不启动进程、无副作用**。
- 若所有候选都没通过该检查，则**回退**为顺序中的第一个存在的候选，即完全不筛选。
- 环境变量 `VOICE2WORD_NO_PROBE=1` 可关闭这项检查，严格按顺序取第一个存在的候选。
- 超时保护：`transcribe` 默认 3600 秒、`version` 默认 30 秒，可用 `VOICE2WORD_TIMEOUT`
  与 `VOICE2WORD_VERSION_TIMEOUT` 覆盖（单位秒，正数）。超时返回 `isError: true`，
  并附上当前解析到的可执行文件路径与排查提示。

```powershell
# 方式一：启动参数
python integrations\mcp-server\voice2word_mcp.py --exe <repo>\target\release\voice2word.exe

# 方式二：环境变量（当前会话）
$env:VOICE2WORD_EXE = "<repo>\target\release\voice2word.exe"
python integrations\mcp-server\voice2word_mcp.py
```

在客户端配置里则写成：

```json
"env": { "VOICE2WORD_EXE": "<repo>\\target\\release\\voice2word.exe" }
```

## 7. 底层 CLI 事实

MCP 工具最终调用的是 Voice2Word 的无头模式：

```text
voice2word transcribe <输入文件> [-o <输出>] [--format ...] [--lang <代码>] [--threads N] [--polish] [--resume] [--artifact-dir <路径>]

# 顶层形式：voice2word [--gui] | --help/-h | --version/-V（另接受裸子命令 help / version）
```

`transcribe` 另支持 `--translate <语言>`（目标语言，当前仅记录、翻译阶段未接入）与
`--model <路径>`（覆盖 Whisper 模型文件）；顶层还有 `--gui`（启动图形界面，无参数时等同）、
`--help`/`-h`、`--version`/`-V`。这些选项**不在** MCP `transcribe` 工具的入参里，需要时请直接调用 CLI。

- **进度 / 诊断**由 CLI 写到它自己的 `stderr`（MCP 服务器在 stdio 模式下**不**写 `stderr`，
  这些内容会被摘要进 `tools/call` 返回的文本里）；
- **最终结果路径**由 CLI 写 `stdout`，成功时末行形如：`字幕已写入: <路径>（共 N 段）`；
- 退出码：

| 退出码 | 含义 |
| --- | --- |
| `0` | 成功 |
| `1` | 转写运行期失败 |
| `2` | 参数错误（未知选项、非法 `--format`、`--threads` 非正整数等） |

服务器会把这些结果包装进 `tools/call` 的 `content` / `isError` 中返回，不直接透传退出码。

## 8. 故障排查

- **客户端看不到工具**：确认 `command` 指向的解释器能在该环境启动，且 `args` 中的脚本路径正确；
  先按第 3 节用管道手动跑一遍。
- **`tools/call` 返回 `isError: true`，提示找不到可执行文件**：按第 6 节检查查找顺序，
  或显式设置 `--exe` / `VOICE2WORD_EXE`。
- **`probe` 显示 `ffmpeg` 不可用**：把 ffmpeg / ffprobe 加入 PATH，或按项目 `config.toml`
  的路径配置指定。
- **响应里出现 `-32700`**：说明那一行不是合法 JSON，检查是否误把多行 JSON 拆成了多行输入。
- **配置文件路径不确定**：各客户端版本差异较大，一律**以你的客户端版本为准**。