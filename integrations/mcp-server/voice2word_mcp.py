#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Voice2Word MCP 服务器（stdio JSON-RPC 2.0，仅使用 Python 标准库）。

用法:
    python voice2word_mcp.py [--exe <voice2word 可执行文件>] [--self-test]

默认行为：进入 stdio 循环，从 stdin 逐行读取 JSON-RPC 请求（一行一个 JSON），
把响应逐行写到 stdout。服务器自身不写 stderr（仅启动参数错误时输出到 stderr）。

支持的方法:
    initialize / notifications/initialized / ping / tools/list / tools/call
"""

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

SERVER_NAME = "voice2word-mcp"
SERVER_VERSION = "0.1.0"
DEFAULT_PROTOCOL_VERSION = "2024-11-05"
SUPPORTED_FORMATS = ("srt", "ass", "vtt", "json", "txt", "ttml", "ttal")

# JSON-RPC 2.0 错误码
PARSE_ERROR = -32700
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# 仓库根：<repo>/integrations/mcp-server/voice2word_mcp.py -> parents[2]
REPO_ROOT = Path(__file__).resolve().parents[2]

# 由 --exe / -e / --exe=... 指定的可执行文件（优先级最高）
_EXE_OVERRIDE = None

# resolve_exe 的结果缓存（每个进程只解析一次）
_EXE_CACHE = None

# CLI 能力标记：仓库里可能存在只带图形界面、不认 transcribe 子命令的旧构建，
# 这类二进制里不会包含下列 CLI 相关字符串。
_CLI_MARKERS = (b"transcribe", b"--format", b"--version", b"--polish")

# 子进程异常（文件缺失 / 超时）
_CLI_ERRORS = (OSError, subprocess.TimeoutExpired)

USAGE = """Voice2Word MCP server (stdio JSON-RPC 2.0)

用法:
  python voice2word_mcp.py               # 进入 stdio 服务器循环（默认）
  python voice2word_mcp.py --self-test   # 跑一遍内置自检后退出
  python voice2word_mcp.py --exe <path>  # 指定 voice2word 可执行文件

也可用环境变量 VOICE2WORD_EXE 指定可执行文件。
"""


class RpcError(Exception):
    """带 JSON-RPC 错误码的异常（映射为 error 响应）。"""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


# --------------------------------------------------------------------------
# 可执行文件定位
# --------------------------------------------------------------------------
def _explicit_candidates(explicit):
    """显式指定的可执行文件（--exe / VOICE2WORD_EXE），优先级最高。"""
    result = []
    if explicit:
        result.append(Path(explicit))
    if _EXE_OVERRIDE:
        result.append(Path(_EXE_OVERRIDE))
    env_exe = os.environ.get("VOICE2WORD_EXE")
    if env_exe:
        result.append(Path(env_exe))
    return result


def _auto_candidates():
    """仓库内自动探测的候选（约定顺序）。"""
    return [
        REPO_ROOT / "target" / "release" / "voice2word.exe",
        REPO_ROOT / "target" / "debug" / "voice2word.exe",
        REPO_ROOT / "voice2word.exe",
    ]


def exe_candidates(explicit=None):
    """按约定优先级返回候选路径列表（不含 PATH 查找结果）。"""
    return _explicit_candidates(explicit) + _auto_candidates()


def _supports_cli(path):
    """纯读文件判断候选二进制是否包含 CLI 标记字符串（无副作用）。

    只用于跳过明显不支持 CLI 的旧构建：读不出内容或包含全部标记时都返回 True，
    因此不会改变"按顺序取第一个"的语义。
    """
    if os.environ.get("VOICE2WORD_NO_PROBE"):
        return True
    try:
        data = Path(path).read_bytes()
    except OSError:
        return True
    return all(marker in data for marker in _CLI_MARKERS)


def resolve_exe(explicit=None):
    """定位 voice2word 可执行文件，返回字符串路径（可能不存在）。

    顺序: --exe / VOICE2WORD_EXE -> target/release -> target/debug -> 仓库根 -> PATH。
    显式指定的路径直接采用；自动候选按顺序取第一个"存在且带 CLI 标记"的，
    若都不带标记则回退为顺序中的第一个（即完全不筛选）。
    """
    global _EXE_CACHE
    if _EXE_CACHE is not None:
        return _EXE_CACHE

    for candidate in _explicit_candidates(explicit):
        try:
            if candidate.is_file():
                _EXE_CACHE = str(candidate)
                return _EXE_CACHE
        except OSError:
            continue

    existing = []
    for candidate in _auto_candidates():
        try:
            if candidate.is_file():
                existing.append(str(candidate))
        except OSError:
            continue
    found = shutil.which("voice2word") or shutil.which("voice2word.exe")
    if found:
        existing.append(found)

    chosen = None
    for path in existing:
        if _supports_cli(path):
            chosen = path
            break
    if chosen is None:
        chosen = existing[0] if existing else "voice2word"

    _EXE_CACHE = chosen
    return _EXE_CACHE


def _cli_timeout(env_name, default):
    """从环境变量读取超时秒数（正数），非法或未设置时用默认值。"""
    raw = os.environ.get(env_name)
    if raw:
        try:
            value = float(raw)
            if value > 0:
                return value
        except ValueError:
            pass
    return default


def run_cli(args, timeout=None):
    """运行 voice2word，返回 (exe, CompletedProcess)。异常交由调用方处理。"""
    if timeout is None:
        timeout = _cli_timeout("VOICE2WORD_TIMEOUT", 3600.0)
    exe = resolve_exe()
    proc = subprocess.run(
        [exe] + list(args),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
    )
    return exe, proc


def _summarize(text, limit=4000):
    """截断过长的文本，避免响应体爆掉。"""
    text = text.strip()
    if len(text) <= limit:
        return text
    return text[:limit] + "\n...(已截断，共 %d 字符)" % len(text)


def _cli_failure_text(prefix, exc):
    """子进程失败时的文本：附带当前解析到的可执行文件与排查提示。"""
    return (
        "%s: %s\n当前解析到的可执行文件: %s\n"
        "提示：可用 --exe <路径> 启动参数或环境变量 VOICE2WORD_EXE 指定正确的 voice2word 可执行文件。"
        % (prefix, exc, resolve_exe())
    )


def _tool_result(text, is_error=False):
    """MCP tools/call 结果结构。"""
    return {"content": [{"type": "text", "text": text}], "isError": bool(is_error)}


# --------------------------------------------------------------------------
# 工具实现
# --------------------------------------------------------------------------
def tool_transcribe(arguments):
    """把入参映射为 `voice2word transcribe ...` 并执行。"""
    if not isinstance(arguments, dict):
        raise RpcError(INVALID_PARAMS, "transcribe 的 arguments 必须是对象")

    source = arguments.get("input")
    if not isinstance(source, str) or not source.strip():
        raise RpcError(INVALID_PARAMS, "transcribe 缺少必填参数 input（非空字符串）")

    args = ["transcribe", source]

    output = arguments.get("output")
    if output is not None:
        if not isinstance(output, str) or not output.strip():
            raise RpcError(INVALID_PARAMS, "output 必须是非空字符串")
        args += ["-o", output]

    fmt = arguments.get("format")
    if fmt is not None:
        if not isinstance(fmt, str) or fmt.strip().lower() not in SUPPORTED_FORMATS:
            raise RpcError(
                INVALID_PARAMS,
                "format 必须是以下之一: " + "|".join(SUPPORTED_FORMATS),
            )
        args += ["--format", fmt.strip().lower()]

    language = arguments.get("language")
    if language is not None:
        if not isinstance(language, str) or not language.strip():
            raise RpcError(INVALID_PARAMS, "language 必须是非空字符串")
        args += ["--lang", language]

    threads = arguments.get("threads")
    if threads is not None:
        if isinstance(threads, bool) or not isinstance(threads, int) or threads < 1:
            raise RpcError(INVALID_PARAMS, "threads 必须是 >=1 的整数")
        args += ["--threads", str(threads)]

    polish = arguments.get("polish")
    if polish is not None:
        if not isinstance(polish, bool):
            raise RpcError(INVALID_PARAMS, "polish 必须是布尔值")
        if polish:
            args.append("--polish")

    resume = arguments.get("resume")
    if resume is not None:
        if not isinstance(resume, bool):
            raise RpcError(INVALID_PARAMS, "resume 必须是布尔值")
        if resume:
            args.append("--resume")

    artifact_dir = arguments.get("artifact_dir")
    if artifact_dir is not None:
        if not isinstance(artifact_dir, str) or not artifact_dir.strip():
            raise RpcError(INVALID_PARAMS, "artifact_dir 必须是非空字符串")
        args += ["--artifact-dir", artifact_dir]

    try:
        exe, proc = run_cli(args)
    except _CLI_ERRORS as exc:
        return _tool_result(_cli_failure_text("执行 voice2word transcribe 失败", exc), True)

    stdout = (proc.stdout or "").strip()
    stderr = (proc.stderr or "").strip()

    if proc.returncode == 0:
        text = stdout or "转写完成（stdout 无输出）"
        if stderr:
            text += "\n--- stderr ---\n" + _summarize(stderr)
        return _tool_result(text, False)

    parts = ["voice2word 退出码: %d" % proc.returncode]
    if stderr:
        parts.append("stderr:\n" + _summarize(stderr))
    if stdout:
        parts.append("stdout:\n" + _summarize(stdout))
    return _tool_result("\n".join(parts), True)


def tool_probe(arguments):
    """探测 ffprobe / ffmpeg 是否可用。"""
    result = {}
    for name in ("ffprobe", "ffmpeg"):
        path = shutil.which(name)
        result[name] = {"available": bool(path), "path": path}
    return _tool_result(json.dumps(result, ensure_ascii=False, indent=2), False)


def tool_version(arguments):
    """运行 `voice2word --version`，返回版本字符串。"""
    try:
        exe, proc = run_cli(["--version"], timeout=_cli_timeout("VOICE2WORD_VERSION_TIMEOUT", 30.0))
    except _CLI_ERRORS as exc:
        return _tool_result(_cli_failure_text("执行 voice2word --version 失败", exc), True)

    stdout = (proc.stdout or "").strip()
    stderr = (proc.stderr or "").strip()
    if proc.returncode == 0:
        return _tool_result(stdout or "unknown", False)
    return _tool_result(
        "voice2word --version 退出码: %d\n%s" % (proc.returncode, _summarize(stderr or stdout)),
        True,
    )


TOOLS = [
    {
        "name": "transcribe",
        "description": (
            "调用 voice2word transcribe 转写音视频并导出字幕。"
            "支持 srt/ass/vtt/json/txt/ttml/ttal 格式，返回 CLI 的 stdout 与 stderr 摘要。"
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "输入音视频文件路径（必填）",
                },
                "output": {
                    "type": "string",
                    "description": "输出字幕路径，对应 CLI -o/--output",
                },
                "format": {
                    "type": "string",
                    "enum": list(SUPPORTED_FORMATS),
                    "description": "输出格式，对应 CLI --format",
                },
                "language": {
                    "type": "string",
                    "description": "源语言代码（如 zh/en/ja/auto），对应 CLI --lang",
                },
                "threads": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "推理线程数，对应 CLI --threads",
                },
                "polish": {
                    "type": "boolean",
                    "description": "强制开启润色/标点，对应 CLI --polish",
                },
                "resume": {
                    "type": "boolean",
                    "description": "复用阶段产物目录中已有的识别结果（断点续跑），对应 CLI --resume",
                },
                "artifact_dir": {
                    "type": "string",
                    "description": "阶段产物目录，对应 CLI --artifact-dir（默认 <输入目录>/.voice2word-stages/<文件名>）",
                },
            },
            "required": ["input"],
        },
    },
    {
        "name": "probe",
        "description": "探测本机 ffprobe / ffmpeg 是否可用，返回各自的可用性与路径。",
        "inputSchema": {"type": "object", "properties": {}, "required": []},
    },
    {
        "name": "version",
        "description": "返回 voice2word 可执行文件的版本字符串（等价于 voice2word --version）。",
        "inputSchema": {"type": "object", "properties": {}, "required": []},
    },
]

HANDLERS = {
    "transcribe": tool_transcribe,
    "probe": tool_probe,
    "version": tool_version,
}


# --------------------------------------------------------------------------
# JSON-RPC 处理
# --------------------------------------------------------------------------
def _ok(request_id, result):
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _error(request_id, code, message):
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    }


def handle_message(msg):
    """处理单条 JSON-RPC 消息。通知返回 None，其余返回响应 dict。"""
    has_id = "id" in msg
    request_id = msg.get("id")
    method = msg.get("method")

    if not isinstance(method, str) or not method:
        if not has_id:
            return None
        raise RpcError(INVALID_PARAMS, "缺少 method 字段")

    params = msg.get("params")
    if params is None:
        params = {}
    if not isinstance(params, dict):
        if not has_id:
            return None
        raise RpcError(INVALID_PARAMS, "params 必须是对象")

    if method == "initialize":
        requested = params.get("protocolVersion")
        protocol_version = (
            requested if isinstance(requested, str) and requested else DEFAULT_PROTOCOL_VERSION
        )
        result = {
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
        }
        return _ok(request_id, result) if has_id else None

    if method.startswith("notifications/"):
        # 通知（含 notifications/initialized）：不产生响应
        return None

    if method == "ping":
        return _ok(request_id, {}) if has_id else None

    if method == "tools/list":
        return _ok(request_id, {"tools": TOOLS}) if has_id else None

    if method == "tools/call":
        name = params.get("name")
        if not isinstance(name, str) or not name:
            if not has_id:
                return None
            raise RpcError(INVALID_PARAMS, "tools/call 缺少 name 字段")
        handler = HANDLERS.get(name)
        if handler is None:
            if not has_id:
                return None
            raise RpcError(INVALID_PARAMS, "未知工具: %s" % name)
        arguments = params.get("arguments")
        if arguments is None:
            arguments = {}
        result = handler(arguments)
        return _ok(request_id, result) if has_id else None

    if not has_id:
        return None
    raise RpcError(METHOD_NOT_FOUND, "未知方法: %s" % method)


def process_line(line):
    """处理一行输入，返回响应 dict 或 None（通知/空行）。"""
    line = line.strip()
    if not line:
        return None
    try:
        msg = json.loads(line)
    except Exception:
        return _error(None, PARSE_ERROR, "JSON 解析失败")

    if not isinstance(msg, dict):
        return _error(None, PARSE_ERROR, "请求必须是 JSON 对象")

    has_id = "id" in msg
    request_id = msg.get("id")
    try:
        return handle_message(msg)
    except RpcError as exc:
        if not has_id:
            return None
        return _error(request_id, exc.code, exc.message)
    except Exception as exc:  # 兜底：单条请求出错不能让进程崩掉
        if not has_id:
            return None
        return _error(request_id, INTERNAL_ERROR, "内部错误: %s" % exc)


def serve(stdin=None, stdout=None):
    """stdio 主循环：逐行读请求，逐行写响应。"""
    stdin = sys.stdin if stdin is None else stdin
    stdout = sys.stdout if stdout is None else stdout
    for raw in stdin:
        try:
            response = process_line(raw)
        except Exception as exc:  # 绝不让进程崩掉
            response = _error(None, INTERNAL_ERROR, "内部错误: %s" % exc)
        if response is None:
            continue
        stdout.write(json.dumps(response, ensure_ascii=False) + "\n")
        stdout.flush()


# --------------------------------------------------------------------------
# 入口
# --------------------------------------------------------------------------
def _reconfigure_streams():
    """统一按 UTF-8 读写，避免 Windows 默认代码页导致中文乱码。"""
    try:
        sys.stdin.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass
    try:
        sys.stdout.reconfigure(encoding="utf-8", newline="\n")
    except Exception:
        pass
    try:
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except Exception:
        pass


def run_self_test():
    """内置自检：跑几条代表性请求，打印请求/响应。"""
    samples = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "ping"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 4, "method": "no/such-method"},
    ]
    ok = True
    for sample in samples:
        response = process_line(json.dumps(sample))
        sys.stdout.write(
            json.dumps({"request": sample, "response": response}, ensure_ascii=False) + "\n"
        )

    init = handle_message(samples[0])
    ok = ok and init["result"]["capabilities"].get("tools") is not None
    ok = ok and handle_message(samples[1]) is None
    ok = ok and handle_message(samples[2])["result"] == {}
    ok = ok and len(handle_message(samples[3])["result"]["tools"]) == 3
    try:
        handle_message(samples[4])
        ok = False
    except RpcError as exc:
        ok = ok and exc.code == METHOD_NOT_FOUND

    sys.stdout.write("exe: %s\n" % resolve_exe())
    sys.stdout.write("self-test: %s\n" % ("PASS" if ok else "FAIL"))
    sys.stdout.flush()
    return 0 if ok else 1


def main(argv=None):
    global _EXE_OVERRIDE
    argv = list(sys.argv[1:] if argv is None else argv)

    self_test = False
    index = 0
    while index < len(argv):
        arg = argv[index]
        if arg in ("--exe", "-e"):
            index += 1
            if index >= len(argv):
                sys.stderr.write("--exe 需要一个路径参数\n")
                return 2
            _EXE_OVERRIDE = argv[index]
        elif arg.startswith("--exe="):
            _EXE_OVERRIDE = arg.split("=", 1)[1]
        elif arg == "--self-test":
            self_test = True
        elif arg in ("-h", "--help"):
            sys.stderr.write(USAGE)
            return 0
        else:
            sys.stderr.write("未知参数: %s\n%s" % (arg, USAGE))
            return 2
        index += 1

    _reconfigure_streams()

    if self_test:
        return run_self_test()

    try:
        serve()
    except KeyboardInterrupt:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())