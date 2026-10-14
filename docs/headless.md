# 无头模式与机器调用（命令与 JSON 契约）

本文档只讲一件事：外部程序（自动化脚本、任务编排、AI 助手）怎么调用 Voice2Word 完成转写，
以及会拿到什么格式的返回值。图形界面的使用说明不在这里。

- 音频、视频与识别结果默认只在本机处理；只有显式启用在线翻译或工具调用时才会联网。
- 所有命令都有 `--help`，参数与默认值与本文一致。

---

## 1. 定位可执行文件

按以下顺序找 `voice2word.exe`（**脚本请优先用控制台子系统的 `voice2word-cli.exe`**：它是同一个 CLI，但由 shell 同步等待，`> out.json` 这类重定向与 `` 才可靠；`voice2word.exe` 是 GUI 子系统，PowerShell 不等它，重定向可能拿到空文件）：

1. 安装包安装后的程序目录；
2. 源码构建产物：`target\release\voice2word.exe`（其次 `target\debug\voice2word.exe`）；
3. `PATH` 中的 `voice2word`。

```powershell
$exe = if (Test-Path .\target\release\voice2word.exe) { '.\target\release\voice2word.exe' } else { 'voice2word' }
& $exe --version
```

## 2. 子命令

| 子命令 | 作用 | stdout |
| --- | --- | --- |
| `transcribe <输入>` | 转写并导出字幕（面向人） | 结果一行：`字幕已写入: <路径>（共 N 段）` |
| `run <输入>` | 一次调用完成一次转写（面向机器） | **一行 JSON**（默认开启，等价于 `transcribe --json`） |
| `tools` | 列出可被外部调用的工具与参数 schema | 工具一览（`--json` 时是一行 JSON 清单） |
| `doctor` | 本机自检（只读）：配置、依赖、硬件、日志与数据库 | 每项一行 `OK/WARN/ERROR 名称 - 说明` + 末行汇总（`--json` 时是一行 JSON 报告） |
| `--help` / `-h` | 顶层帮助 | 帮助文本 |
| `--version` / `-V` | 版本号 | `voice2word <版本>` |
| `--gui` | 启动图形界面（无参数时也是它） | — |

`<输入>` 可以是**文件**，也可以是**目录**（目录=批量，默认只看该目录一层，`--recursive` 递归子目录）。

## 3. 退出码

| 退出码 | 含义 |
| --- | --- |
| `0` | 成功（含 `--help`、`--version`、`tools`） |
| `1` | 运行失败：输入路径不存在、转写报错、写盘失败、未产生字幕片段、在线接口报错等 |
| `2` | 参数错误：缺少输入、未知子命令或选项、非法取值、选项缺少取值 |

`run` 在**运行失败时同样会输出一行 JSON**（`status` = `error`，`exit_code` = `1`，`error` 写明原因），
因此调用方可以先解析 stdout、再看进程退出码。参数错误（退出码 `2`）时 stdout 为空，原因与用法都写在 stderr。

`doctor` 的退出码另有细分：没有 ERROR 就是 `0`（只有注意项也算 `0`），存在 ERROR 是 `1`，参数错误同样是 `2`（它只接受 `--json`）。

## 4. 输出约定

- **stdout 只放结果**：`transcribe` 是结果行，`run --json` 是**一行** JSON（无换行、无缩进）。
- **stderr 放进度与诊断**：`[阶段] …`、`[提取音频] 42%`、`[翻译] 60% …`、收尾的耗时统计块、错误说明。
- 想只要结果：

```powershell
$json = & $exe run --json --input .\talk.mp4 2> $env:TEMP\v2w.log
$result = $json | ConvertFrom-Json
$result.status; $result.output; $result.segments
```

## 5. `run --json` 的结果字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schema` | string | 固定 `voice2word.run.v1`；字段语义变化时递增 |
| `status` | string | `ok` / `partial` / `error`（批量时按整体判定） |
| `action` | string | `transcribe` 或 `tool_call` |
| `exit_code` | number | 与进程退出码一致 |
| `input` | string/null | 输入路径（**绝对路径**） |
| `input_kind` | string/null | `file` / `directory` / `tool_call` |
| `output` | string/null | 输出字幕文件的**绝对路径**；目录批量时为 `null`，逐条见 `items[]` |
| `format` | string/null | 实际使用的输出格式 |
| `language` | string/null | 显式指定的源语言（未指定为 `null`，表示按配置默认） |
| `segments` | number | 字幕片段数 |
| `duration_sec` | number | 字幕覆盖的媒体时长（秒） |
| `elapsed_sec` | number | 本次墙钟耗时（秒） |
| `polished` | boolean | 是否开启了润色/标点 |
| `translated_to` | string/null | 翻译目标语言；未翻译为 `null` |
| `hotwords` | string/null | 本次生效的热词（命令行覆盖值） |
| `speakers` | number/null | 说话人数；未开启说话人分离为 `null` |
| `resumed` | boolean | 是否启用了断点续跑 |
| `artifact_dir` | string/null | 阶段产物目录（绝对路径） |
| `dry_run` | boolean | 是否只是预演（`--dry-run`） |
| `error` | string/null | 失败原因；成功为 `null` |
| `items` | array | 目录批量时的逐文件结果（字段与顶层相同） |
| `tool_call` | object/null | `--tool-call` 时模型选择的那次调用：`id` / `name` / `arguments` / `raw_arguments` |
| `assistant_message` | string/null | 模型给出的自然语言说明 |
| `model` | string/null | `--tool-call` 使用的在线模型名 |
| `result` | object/null | `--tool-call` 时本地执行的结果（与顶层同构） |

示例（单文件，成功；实际输出是一行）：

```json
{"schema":"voice2word.run.v1","status":"ok","action":"transcribe","exit_code":0,"input":"D:\\media\\talk.mp4","input_kind":"file","output":"D:\\media\\talk.srt","format":"srt","language":"zh","segments":128,"duration_sec":312.4,"elapsed_sec":96.2,"polished":false,"translated_to":null,"hotwords":null,"speakers":null,"resumed":false,"artifact_dir":null,"dry_run":false,"error":null,"items":[],"tool_call":null,"assistant_message":null,"model":null,"result":null}
```

示例（目录批量，其中一条失败）：

```json
{"schema":"voice2word.run.v1","status":"partial","action":"transcribe","exit_code":1,"input":"D:\\media\\clips","input_kind":"directory","output":null,"format":"srt","segments":64,"duration_sec":180.0,"elapsed_sec":75.5,"polished":true,"translated_to":"English","hotwords":"张三,某品牌","speakers":null,"resumed":false,"artifact_dir":null,"dry_run":false,"error":"D:\\media\\clips\\b.mp4: 转写失败：…","items":[{"schema":"voice2word.run.v1","status":"ok","action":"transcribe","exit_code":0,"input":"D:\\media\\clips\\a.mp4","input_kind":"file","output":"D:\\media\\clips\\a.srt","format":"srt","language":null,"segments":64,"duration_sec":180.0,"elapsed_sec":61.0,"polished":true,"translated_to":"English","hotwords":"张三,某品牌","speakers":null,"resumed":false,"artifact_dir":null,"dry_run":false,"error":null,"items":[],"tool_call":null,"assistant_message":null,"model":null,"result":null},{"schema":"voice2word.run.v1","status":"error","action":"transcribe","exit_code":1,"input":"D:\\media\\clips\\b.mp4","input_kind":"file","output":"D:\\media\\clips\\b.srt","format":"srt","language":null,"segments":0,"duration_sec":0.0,"elapsed_sec":3.2,"polished":false,"translated_to":null,"hotwords":null,"speakers":null,"resumed":false,"artifact_dir":null,"dry_run":false,"error":"音频解码失败","items":[],"tool_call":null,"assistant_message":null,"model":null,"result":null}]}
```

## 6. 体检：`voice2word doctor`

转写跑不起来时先跑一次体检：它把配置、依赖、硬件、日志与数据库五类判定逐项列出来，
每项给出结论与可执行的修复建议。

```powershell
& $exe doctor            # 人用：每项一行结论，末行汇总
& $exe doctor --json     # 机器用：stdout 恒为一行 JSON
```

人用模式的输出形状：

```text
OK 配置文件 - D:\app\config.toml 可读，无本机覆盖（便携默认，正常）
OK 项目根 - D:\app（models/ 存在）
WARN ffprobe - D:\app\tools\ffprobe.exe 不存在，PATH 上也没有 ffprobe
fix: 需要外部脚本探测流信息时，把 ffprobe.exe 放到 ffmpeg 同目录或加入 PATH。
ERROR 数据库可打开 - D:\app\voice2word.db 只读打开失败：...
fix: 检查文件权限与磁盘状态；被其它进程独占时稍后重试。
汇总: 正常 10 项 / 注意 1 项 / 失败 1 项，整体状态 error
```

`--json` 的字段（实际输出是一行，无换行、无缩进）：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schema` | string | 固定 `voice2word.doctor.v1`；字段语义变化时递增 |
| `status` | string | 整体结论：`ok` / `warn` / `error` |
| `exit_code` | number | `0`（没有 ERROR）或 `1`（存在 ERROR） |
| `checks` | array | 逐项结果；每项含 `name`（稳定标识，如 `ffmpeg`）、`label`、`status`、`message`、`fix`（无建议时为 `null`） |
| `summary` | string | 一行汇总，与人用模式的末行一致 |

示例（实际输出是一行）：

```json
{"schema":"voice2word.doctor.v1","status":"warn","exit_code":0,"checks":[{"name":"config_readable","label":"配置文件","status":"ok","message":"D:\\app\\config.toml 可读，无本机覆盖（便携默认，正常）","fix":null},{"name":"ffprobe","label":"ffprobe","status":"warn","message":"D:\\app\\tools\\ffprobe.exe 不存在，PATH 上也没有 ffprobe","fix":"需要外部脚本探测流信息时，把 ffprobe.exe 放到 ffmpeg 同目录或加入 PATH。"}],"summary":"汇总: 正常 10 项 / 注意 1 项 / 失败 0 项，整体状态 warn"}
```

退出码：

| 退出码 | 含义 |
| --- | --- |
| `0` | 没有 ERROR（只有注意项也算成功） |
| `1` | 存在 ERROR：组件缺失、配置读不出来、日志目录不可写、数据库打不开等 |
| `2` | 参数错误：`doctor` 只接受 `--json`，未知选项与多余参数都算 |

体检是**只读**操作：不改配置、不写字幕、不访问网络。判定「目录可写」时会写入一个临时探针文件
并立即删除（与图形界面的自检报告同一套做法），不会留下文件；数据库只做只读打开与 `quick_check`，
建表、迁移与损坏自愈都留给程序正常启动路径。
## 7. 目录批量

输入给目录即批量：按扩展名筛选媒体文件（`mp3 wav m4a aac flac ogg opus wma amr mp4 mkv mov avi webm ts flv wmv m4v`，大小写不敏感），
按路径排序后**逐个串行**转写（避免多个推理进程互相抢 CPU）。`--output` 此时是**输出目录**。

```powershell
# 每首歌各自导出同目录同名 srt
& $exe run --json --input .\clips

# 递归子目录，全部导出到 out\ 下，格式 vtt
& $exe run --json --input .\clips --recursive --output .\out --format vtt
```

- 整体 `status`：全部成功为 `ok`，全部失败为 `error`，部分失败为 `partial`（此时退出码为 `1`）。
- 逐个文件的结果在 `items[]`，字段与顶层相同（含各自的 `output` 与 `error`）。

## 8. 常用覆盖参数（不必改 config.toml）

| 参数 | 作用 | 默认 |
| --- | --- | --- |
| `--format <格式>` | `srt` / `ass` / `vtt` / `json` / `txt` / `txt-ts` / `ttml` / `ttal` / `sbv` / `lrc`（`txt` 是纯文本，`txt-ts` 是每行带时间码的纯文本，两者扩展名都是 `.txt`） | `srt` |
| `--lang, --language <代码>` | 源语言（`zh` / `en` / `ja` / `auto` …） | 取 `config.toml` 的 `pipeline.language` |
| `--hotwords "张三,某品牌"` | 热词（人名、品牌、术语），合入识别提示词 | 取 `pipeline.asr_hotwords` |
| `--translate <语言>` | 转写完成后翻译字幕（双语导出） | 不翻译 |
| `--translate-mode online\|offline` | 翻译链路：在线接口 / 本地模型 | 取 `[translate].mode` |
| `--glossary "甲=乙,丙=丁"` | 术语表覆盖（`原文=译文`） | 取 `[translate].glossary` |
| `--polish` | 强制开启润色/标点 | 取 `pipeline.enable_polish` |
| `--diarize` / `--speakers 2-4` | 说话人分离与人数 | 取 `pipeline.enable_diarization` |
| `--threads <数量>` | 推理线程数（超过本机逻辑核数自动收敛） | 取 `pipeline.whisper_threads` |
| `--model <路径>` | 覆盖识别模型文件（路径里含 `sensevoice` 即走 SenseVoice 引擎） | 取 `paths.whisper_model` |
| `--engine <名称>` | `auto` / `whisper` / `sensevoice`；`auto` 在 whisper 模型不可用而 SenseVoice 就位时自动选后者 | `auto` |
| `--resume` | 复用阶段产物断点续跑 | 关闭 |
| `--artifact-dir <路径>` | 阶段产物目录（给出即开启落盘） | 仅 `--resume` 时回落 `<输入目录>\.voice2word-stages\<文件名>` |
| `--dry-run` | 只回报将要执行的动作与输出路径，不真正转写 | 关闭 |
| `--yes` | 允许覆盖已存在的输出文件（工具调用链路不加就不覆盖） | 关闭 |
| `--allow-root <目录>` | 追加允许读写的根目录（可重复） | 默认：当前目录、输入所在目录、`--output` 的父目录 |
| `--json` | 结束时在 stdout 打一行 JSON（`run` 默认开启） | `run` 为开 |

`--translate` 的两种链路：`online` 走 `config.toml` 的 `[translate]`（`api_base` / `api_key` / `api_model` / `timeout_secs`），
密钥留空时会读环境变量 `VOICE2WORD_API_KEY`；`offline` 走本机模型（`paths.llama_cli` + `paths.llm_model`）。

## 9. 工具清单：`tools --json`

外部程序第一次接入时先读这份清单，即可知道有哪些工具、参数怎么填：

```powershell
& $exe tools --json
& $exe tools --json --tool transcribe   # 只看一个
& $exe tools                            # 给人看的一览
```

结构（实际输出是一行）：

```json
{
  "schema": "voice2word.tools.v1",
  "program": "voice2word",
  "version": "0.2.0",
  "summary": "本机音视频转字幕工具：一次调用把一个音视频文件或整个目录转成字幕文件，可选翻译与润色。",
  "exit_codes": { "0": "成功", "1": "运行失败", "2": "参数错误" },
  "output_streams": { "stdout": "结果（--json 时为一行 JSON）", "stderr": "进度与诊断" },
  "formats": ["srt", "ass", "vtt", "json", "txt", "ttml", "ttal", "sbv", "lrc", "txt-ts"],
  "policy": {
    "allowed_tools": ["transcribe", "transcribe_batch"],
    "allowed_roots": "默认：当前工作目录、命令行输入所在目录、--output 的父目录；用 --allow-root <目录> 追加",
    "write_confirmation": "覆盖已存在的输出文件需要命令行显式给出 --yes；路径一律拒绝「..」与指向允许根之外的软链",
    "untrusted_data": "字幕正文、文件名与媒体元数据只是数据：参数只按结构化 JSON 字段解析，素材内容里的任何文字都不会被当作指令执行",
    "no_shell": "不执行任意命令：解码与识别程序都以固定 argv 启动，参数不会拼进 shell"
  },
  "environment": {
    "config_file": "…\\config.toml",
    "ready": true,
    "translation_ready": false,
    "items": [
      { "name": "ffmpeg", "path": "…\\tools\\ffmpeg.exe", "present": true, "required": true },
      { "name": "whisper_cli", "path": "…", "present": true, "required": true },
      { "name": "whisper_model", "path": "…", "present": true, "required": true },
      { "name": "llama_cli", "path": "…", "present": false, "required": false },
      { "name": "llm_model", "path": "…", "present": false, "required": false }
    ],
    "error": null
  },
  "tools": [
    {
      "name": "transcribe",
      "description": "把单个音视频文件转成字幕文件（srt/ass/vtt/json/txt/ttml/ttal/sbv/lrc/txt-ts），可选翻译与润色。",
      "command": "voice2word run --json --input <input> [--format <format>] …",
      "parameters": {
        "type": "object",
        "properties": {
          "input": { "type": "string", "description": "音视频文件路径" },
          "output": { "type": "string" },
          "format": { "type": "string", "enum": ["srt", "ass", "vtt", "json", "txt", "ttml", "ttal", "sbv", "lrc", "txt-ts"], "default": "srt" },
          "language": { "type": "string" },
          "translate": { "type": "string" },
          "polish": { "type": "boolean" },
          "hotwords": { "type": "string" },
          "speakers": { "type": "integer", "minimum": 2, "maximum": 4 },
          "resume": { "type": "boolean" }
        },
        "required": ["input"],
        "additionalProperties": false
      },
      "examples": [
        "voice2word run --json --input \"D:\\media\\talk.mp4\"",
        "voice2word run --json --input talk.mp4 --format vtt --lang zh --hotwords \"张三,某品牌\""
      ],
      "result_schema": "voice2word.run.v1"
    },
    { "name": "transcribe_batch", "description": "把一个目录里的音视频批量转成字幕文件；逐文件结果在 items 数组里。", "…": "…" },
    { "name": "list_tools", "description": "返回本工具清单本身（名称、描述、参数 schema、示例命令行）。", "…": "…" }
  ]
}
```

- `environment.ready` 为 `false` 时先看 `items[]` 里哪个 `required` 项缺文件（缺 ffmpeg / 识别程序 / 识别模型就跑不了转写）。
- 清单里没有任何密钥：密钥只从 `config.toml` 或环境变量读。

## 10. 一次函数调用触发转写：`run --tool-call`

`--tool-call` 把「一句自然语言需求」交给**在线接口**，由模型选择并填写 `transcribe` 工具参数，本程序随即在本机执行，全程只做一次调用：

```powershell
& $exe run --json --tool-call "把 D:\media\talk.mp4 转成 srt 字幕，热词：张三" --input D:\media\talk.mp4
& $exe run --json --tool-call "把 D:\media\clips 整个目录转成 vtt" --allow-root D:\media --api-model <模型名>
```

流程：

1. 本程序向在线接口发一次带工具定义的请求（工具定义与 `tools --json` 里的 `transcribe` 完全一致）；
2. 模型返回 `tool_calls` → 本程序校验参数（路径、格式、说话人数等）后**在本机**执行转写；
3. 执行结果回传模型，模型用一句中文说明结果（失败不影响已完成的转写）；
4. stdout 输出一行 JSON：`action` = `tool_call`，`tool_call` 是模型选调用的那次（含解析后的 `arguments`），
   `result` 是本地执行的结果（与普通 `run` 同构），`assistant_message` 是模型的说明。

配置来源：`config.toml` 的 `[translate]`（`api_base` / `api_key` / `api_model` / `timeout_secs`），
命令行 `--api-base` / `--api-key` / `--api-model` / `--api-timeout` 覆盖；密钥留空时读环境变量 `VOICE2WORD_API_KEY`。
模型没有返回工具调用、或调用了未知工具时：`status` = `error`、`exit_code` = `1`、`error` 写明原因，`assistant_message` 保留模型原话。

安全约束（见第 13 节）：

- 模型给出的输入/输出路径必须落在**允许根目录**内。允许根只由命令行决定
  （当前目录、`--input` 所在目录、`--output` 的父目录、`--allow-root`），模型不能自己扩权；
  越界时返回 `status` = `error` 并说明可以加哪个 `--allow-root`。
- 覆盖已存在的输出文件必须在命令行加 `--yes`，否则拒绝（`status` = `error`）。
- 只放行 `transcribe` / `transcribe_batch` 两个动作，没有执行命令、读任意文件的工具。

## 11. 自动化示例

```powershell
# 1) 单文件，拿结构化结果
$r = (& $exe run --json --input .\talk.mp4 2> $env:TEMP\v2w.log) | ConvertFrom-Json
if ($r.exit_code -ne 0) { throw "转写失败: $($r.error)" }
"输出: $($r.output)  片段: $($r.segments)  时长: $($r.duration_sec)s"

# 2) 批量 + 翻译，汇总每个文件的结果
$batch = (& $exe run --json --input .\clips --recursive --format srt --translate English) | ConvertFrom-Json
$batch.items | ForEach-Object { "{0} -> {1}（{2} 段）" -f $_.input, $_.output, $_.segments }

# 3) 热词 + 说话人分离
& $exe run --json --input .\meeting.mp4 --hotwords "张三,某品牌" --diarize --speakers 3 | Out-Null

# 4) 先预演，确认参数与输出路径（不落盘、不推理）
(& $exe run --json --input .\talk.mp4 --format vtt --dry-run) | ConvertFrom-Json | Select-Object output, dry_run

# 5) 断点续跑：中断后重新执行同一条命令即可复用阶段产物
& $exe run --json --input .\long.mp4 --resume --artifact-dir .\.v2w-stages\long
```

## 12. 常见错误与排查

| 现象 | 原因与处理 |
| --- | --- |
| 退出码 `2`，stderr 有「参数错误」 | 命令写错：缺少输入、未知选项、非法 `--format` / `--speakers` / `--threads` 等；按 stderr 提示改 |
| 退出码 `1`，`error` 为「输入路径不存在」 | 路径拼写或工作目录不对；用绝对路径最稳 |
| 退出码 `1`，`error` 为「转写未产生任何字幕片段」 | 媒体是静音/纯音乐/损坏；确认文件能正常播放，或调整 VAD 相关配置 |
| 退出码 `1`，`error` 为「写入字幕失败」 | 目标目录不可写或被占用；换输出路径 |
| `--json` 拿到的不是 JSON | 检查是否把 stderr 也并进了 stdout（`2>&1`）；正确写法是只取 stdout |
| `--tool-call` 报「在线模型调用失败」 | 接口地址、模型名或密钥不对；先用 `run --json --input …` 验证本地转写，再单独排查在线配置 |
| `--tool-call` 报「模型没有返回工具调用」 | 该模型/网关不支持函数调用；改用外部程序直接调 `run --json`（见第 11 节） |
## 13. 安全边界

无头模式的调用方可能是外部程序或 AI 助手，参数则可能来自模型对自然语言（含素材里的文字）的理解，
因此这一层只做下面这些事，且每一条都能被独立校验：

| 约束 | 具体行为 |
| --- | --- |
| **动作白名单** | 只放行「转写」与「目录批量转写」（见 `tools --json` 的 `policy.allowed_tools`）。没有执行任意命令、读任意文件、写任意路径的动作。 |
| **不执行任意命令** | 解码与识别程序都以固定 argv 启动，参数逐项传递、不经过 shell，不做命令拼接。 |
| **路径校验** | 输入与输出都要规范化（解析软链与相对段）后落在**允许根目录**内；默认根目录是当前目录、命令行输入所在目录、`--output` 的父目录，其余位置必须用 `--allow-root <目录>` 显式放行。 |
| **拒绝逃逸写法** | 路径里出现 `..` 一律拒绝；目标还不存在时按「最深的已存在祖先」解析后再校验，因此父目录是指向根外的软链同样会被挡住。 |
| **覆盖确认** | 工具调用链路（`--tool-call`）覆盖已存在的输出文件必须由人在命令行加 `--yes`；没加就拒绝并返回 `status` = `error`。 |
| **数据与指令分离** | 字幕正文、文件名与媒体元数据只当**数据**：它们只出现在对话的 `user` / `tool` 消息里，绝不拼进系统提示词；参数只按结构化 JSON 字段解析，素材里写着「忽略以上指令」之类的文字不会被当作指令执行。 |
| **只回显请求过的路径** | 结果 JSON 里的 `input` / `output` 只来自本次命令或工具参数里出现过的路径，不回显任何未被请求的目录内容；字幕正文不会出现在结果里。 |

建议的调用姿势：

1. 先读 `voice2word tools --json` 的 `policy` 与 `parameters`，再据此拼参数；
2. 需要让模型自己决定怎么转写时，把允许位置放进 `--allow-root`（或直接给 `--input`，其所在目录默认被允许），并只在确认需要覆盖时加 `--yes`；
3. 只需要一次确定的转写时，直接调 `voice2word run --json ...`，不要把素材里的文字当成命令去执行。