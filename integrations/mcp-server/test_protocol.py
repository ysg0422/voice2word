#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""voice2word_mcp.py 的 stdio JSON-RPC 协议测试（纯标准库 unittest）。

用法:
    python integrations/mcp-server/test_protocol.py
    python -m unittest discover -s integrations/mcp-server -p "test_*.py"

测试把 MCP 服务器作为子进程拉起（用 sys.executable，保证与被测同一解释器），
逐行喂 JSON 请求，再逐行读 JSON 响应。
"""

import json
import queue
import subprocess
import sys
import threading
import unittest
from pathlib import Path

# 被测脚本：与本文件同目录，不写死绝对路径
SERVER_SCRIPT = Path(__file__).resolve().with_name("voice2word_mcp.py")

# 服务器响应超时（秒）。测试只走不启动子进程的路径，正常应毫秒级返回。
READ_TIMEOUT = 20.0

EXPECTED_TOOLS = {"transcribe", "probe", "version"}


class _LineReader:
    """后台线程逐行读流，支持带超时的读取（Windows 上 select 不能用于管道）。"""

    def __init__(self, stream):
        self._queue = queue.Queue()
        self._thread = threading.Thread(target=self._pump, args=(stream,), daemon=True)
        self._thread.start()

    def _pump(self, stream):
        try:
            for line in stream:
                self._queue.put(line)
        except Exception as exc:  # pragma: no cover - 读取流异常
            self._queue.put(exc)
        finally:
            self._queue.put(None)

    def read_line(self, timeout=READ_TIMEOUT):
        try:
            item = self._queue.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError("等待服务器响应超时（%.0f 秒）" % timeout)
        if item is None:
            raise AssertionError("服务器提前关闭了 stdout，没有返回响应")
        if isinstance(item, Exception):
            raise item
        return item


class McpServerProcess:
    """以子进程方式驱动 MCP stdio 服务器。"""

    def __init__(self, script=SERVER_SCRIPT):
        self.script = str(script)
        self.proc = subprocess.Popen(
            [sys.executable, self.script],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        self.reader = _LineReader(self.proc.stdout)

    def send(self, payload):
        """发送一条 JSON-RPC 请求（dict 或原始字符串）。"""
        line = payload if isinstance(payload, str) else json.dumps(payload, ensure_ascii=False)
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def request(self, payload, timeout=READ_TIMEOUT):
        """发送请求并读取一条响应，返回解析后的 dict。"""
        self.send(payload)
        raw = self.reader.read_line(timeout=timeout)
        return json.loads(raw)

    def read_line(self, timeout=READ_TIMEOUT):
        return self.reader.read_line(timeout=timeout)

    def close(self):
        proc = self.proc
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
        for stream in (proc.stdin, proc.stdout, proc.stderr):
            try:
                if stream is not None and not stream.closed:
                    stream.close()
            except Exception:
                pass


class McpProtocolTests(unittest.TestCase):
    def setUp(self):
        self.assertTrue(
            SERVER_SCRIPT.is_file(), "找不到被测服务器脚本: %s" % SERVER_SCRIPT
        )
        self.server = McpServerProcess()
        self.addCleanup(self.server.close)

    # --- 1. initialize ---------------------------------------------------
    def test_initialize_returns_server_info_and_tools_capability(self):
        response = self.server.request(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "unittest", "version": "1.0"},
                },
            }
        )
        self.assertEqual(response.get("jsonrpc"), "2.0")
        self.assertEqual(response.get("id"), 1)
        self.assertNotIn("error", response)
        result = response["result"]
        self.assertIn("serverInfo", result)
        self.assertTrue(result["serverInfo"].get("name"))
        self.assertTrue(result["serverInfo"].get("version"))
        self.assertIn("tools", result["capabilities"])
        self.assertIsNotNone(result["capabilities"]["tools"])
        self.assertTrue(result.get("protocolVersion"))

    def test_initialize_echoes_requested_protocol_version(self):
        response = self.server.request(
            {
                "jsonrpc": "2.0",
                "id": 11,
                "method": "initialize",
                "params": {"protocolVersion": "2025-06-18"},
            }
        )
        self.assertEqual(response["result"]["protocolVersion"], "2025-06-18")

    # --- 2. tools/list ---------------------------------------------------
    def test_tools_list_returns_exactly_three_tools(self):
        response = self.server.request({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        self.assertNotIn("error", response)
        tools = response["result"]["tools"]
        self.assertEqual(len(tools), 3, "tools/list 必须恰好返回 3 个工具")
        names = [tool["name"] for tool in tools]
        self.assertEqual(set(names), EXPECTED_TOOLS)
        for tool in tools:
            self.assertIsInstance(tool.get("description"), str)
            self.assertTrue(tool["description"])
            schema = tool.get("inputSchema")
            self.assertIsInstance(schema, dict)
            self.assertEqual(schema.get("type"), "object")
        transcribe = next(tool for tool in tools if tool["name"] == "transcribe")
        props = transcribe["inputSchema"]["properties"]
        for key in ("input", "output", "format", "language", "threads", "polish"):
            self.assertIn(key, props)
        self.assertEqual(transcribe["inputSchema"].get("required"), ["input"])
        self.assertEqual(
            props["format"]["enum"], ["srt", "ass", "vtt", "json", "txt", "ttml", "ttal"]
        )

    # --- 3. ping ---------------------------------------------------------
    def test_ping_returns_empty_result(self):
        response = self.server.request({"jsonrpc": "2.0", "id": 3, "method": "ping"})
        self.assertNotIn("error", response)
        self.assertEqual(response["result"], {})

    # --- 4. 未知方法 ------------------------------------------------------
    def test_unknown_method_returns_method_not_found(self):
        response = self.server.request(
            {"jsonrpc": "2.0", "id": 4, "method": "definitely/not/a/method"}
        )
        self.assertNotIn("result", response)
        self.assertEqual(response["error"]["code"], -32601)

    # --- 附加：通知不产生响应 ---------------------------------------------
    def test_notification_produces_no_response(self):
        self.server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        # 紧随其后的 ping 必须是下一条响应；若通知产生了响应，这里会读到错误的 id
        response = self.server.request({"jsonrpc": "2.0", "id": 41, "method": "ping"})
        self.assertEqual(response.get("id"), 41)
        self.assertEqual(response.get("result"), {})

    # --- 附加：错误码 ------------------------------------------------------
    def test_parse_error_returns_minus_32700(self):
        response = self.server.request("{ this is not json ]")
        self.assertEqual(response["error"]["code"], -32700)

    def test_tools_call_unknown_tool_returns_invalid_params(self):
        response = self.server.request(
            {
                "jsonrpc": "2.0",
                "id": 42,
                "method": "tools/call",
                "params": {"name": "no_such_tool", "arguments": {}},
            }
        )
        self.assertEqual(response["error"]["code"], -32602)

    def test_transcribe_without_input_returns_invalid_params(self):
        response = self.server.request(
            {
                "jsonrpc": "2.0",
                "id": 43,
                "method": "tools/call",
                "params": {"name": "transcribe", "arguments": {"format": "srt"}},
            }
        )
        self.assertEqual(response["error"]["code"], -32602)

    def test_invalid_format_returns_invalid_params(self):
        response = self.server.request(
            {
                "jsonrpc": "2.0",
                "id": 44,
                "method": "tools/call",
                "params": {"name": "transcribe", "arguments": {"input": "a.mp4", "format": "docx"}},
            }
        )
        self.assertEqual(response["error"]["code"], -32602)


if __name__ == "__main__":
    unittest.main(verbosity=2)