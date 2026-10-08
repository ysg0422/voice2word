#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Voice2Word CT-Transformer 离线标点恢复脚本
使用 sherpa-onnx 在纯 CPU 环境下毫秒级恢复标点与断句
"""

import sys
import os
import json
import argparse
import time
import tempfile


def resolve_provider(requested: str) -> str:
    """把命令行/配置里的 provider 收敛到 sherpa-onnx 真正认识的取值。

    sherpa-onnx 认的字符串是 "directml"（不是 "dml"，传 "dml" 必落到
    provider.cc 的 "Unsupported string: dml. Fallback to cpu"）。而即便传对
    了 "directml"，只要**绑定的 onnxruntime 没编译进 DirectML**，仍会静默
    回落 cpu。这里显式收敛，与 SenseVoice runner 口径一致。
    """
    p = (requested or "cpu").strip().lower()
    if p in ("dml", "directml"):
        return "directml"
    return "cpu"


# sherpa-onnx 的 provider 回落提示由 C++ 直接写 fd 2，且句柄在 import 那一刻
# 就被绑定。想在进程内捕获它，必须在 `import sherpa_onnx` **之前**接管 fd 2。
_NATIVE_LOG = {"file": None, "saved": None}


def _begin_native_capture():
    try:
        saved = os.dup(2)
        tmp = tempfile.TemporaryFile()
        os.dup2(tmp.fileno(), 2)
    except OSError:
        return
    _NATIVE_LOG["file"] = tmp
    _NATIVE_LOG["saved"] = saved


def _native_log_text() -> str:
    tmp = _NATIVE_LOG["file"]
    if tmp is None:
        return ""
    try:
        tmp.seek(0)
        data = tmp.read()
        tmp.seek(0, os.SEEK_END)
        return data.decode("utf-8", "replace")
    except (OSError, ValueError):
        return ""


def _end_native_capture():
    """恢复 fd 2，并把捕获到的原生日志原样转发回真实 stderr（不吞任何诊断）。"""
    tmp, saved = _NATIVE_LOG["file"], _NATIVE_LOG["saved"]
    if tmp is None or saved is None:
        return
    sys.stderr.flush()
    body = _native_log_text()
    try:
        if body:
            os.write(saved, body.encode("utf-8", "replace"))
    except OSError:
        pass
    try:
        os.dup2(saved, 2)
        os.close(saved)
    except OSError:
        pass
    _NATIVE_LOG["file"] = None
    _NATIVE_LOG["saved"] = None
    try:
        tmp.close()
    except OSError:
        pass


# 原生日志里出现任一特征串，即说明请求的 provider 没被吃下、已回落 cpu
_FALLBACK_MARKERS = ("Fallback to cpu", "Unsupported string:", "Available providers:")


def classify_actual_provider(requested: str, native_log: str) -> str:
    """按 sherpa-onnx 的原生日志判定实际生效的 provider。

    请求 cpu 时永远是 cpu；请求非 cpu 时只要日志里出现回落特征串，就说明这个
    provider 没生效——如实回报 cpu，让上层能把「静默回落」变成可见告警。
    """
    if requested == "cpu":
        return "cpu"
    text = native_log or ""
    if any(marker in text for marker in _FALLBACK_MARKERS):
        return "cpu"
    return requested


def main():
    parser = argparse.ArgumentParser(description="Voice2Word Punctuation Runner")
    parser.add_argument("--model", required=True, help="Path to CT-Transformer model.onnx")
    parser.add_argument("--input", help="Path to input json file")
    parser.add_argument("--output", help="Path to output json file")
    parser.add_argument("--threads", type=int, default=4, help="CPU threads for ONNX runtime")
    parser.add_argument("--provider", default="cpu", help="ONNX execution provider: cpu | dml")
    args = parser.parse_args()

    provider = resolve_provider(args.provider)

    # 初始化模型。原生回落日志只在 import 之后才可能打出，因此整个「导入 + 建会话」
    # 都必须处在 fd 2 接管区间内，否则捕获不到 sherpa-onnx 的回落提示。
    _begin_native_capture()
    try:
        import sherpa_onnx

        config = sherpa_onnx.OfflinePunctuationConfig(
            model=sherpa_onnx.OfflinePunctuationModelConfig(
                ct_transformer=args.model,
                num_threads=args.threads,
                provider=provider
            )
        )
        punct = sherpa_onnx.OfflinePunctuation(config)
    finally:
        _native_log = _native_log_text()
        _end_native_capture()

    actual_provider = classify_actual_provider(provider, _native_log)

    # 读取输入
    if args.input:
        with open(args.input, "r", encoding="utf-8") as f:
            items = json.load(f)
    else:
        raw_bytes = sys.stdin.buffer.read()
        items = json.loads(raw_bytes.decode("utf-8"))

    t0 = time.time()
    results = []
    for item in items:
        idx = item.get("index", 0)
        text = item.get("text", "")
        if text and text.strip():
            polished = punct.add_punctuation(text.strip())
        else:
            polished = text
        results.append({
            "index": idx,
            "polished": polished
        })

    elapsed = time.time() - t0

    output_data = {
        "count": len(results),
        "elapsed_sec": elapsed,
        "provider": {
            "requested": provider,
            "actual": actual_provider
        },
        "results": results
    }

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            json.dump(output_data, f, ensure_ascii=False, indent=2)
    else:
        out_bytes = json.dumps(output_data, ensure_ascii=False).encode("utf-8")
        sys.stdout.buffer.write(out_bytes)
        sys.stdout.buffer.flush()

if __name__ == "__main__":
    main()
