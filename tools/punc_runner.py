#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Voice2Word CT-Transformer 离线标点恢复脚本
使用 sherpa-onnx 在纯 CPU 环境下毫秒级恢复标点与断句
"""

import sys
import json
import argparse
import time

def main():
    parser = argparse.ArgumentParser(description="Voice2Word Punctuation Runner")
    parser.add_argument("--model", required=True, help="Path to CT-Transformer model.onnx")
    parser.add_argument("--input", help="Path to input json file")
    parser.add_argument("--output", help="Path to output json file")
    parser.add_argument("--threads", type=int, default=4, help="CPU threads for ONNX runtime")
    args = parser.parse_args()

    import sherpa_onnx

    # 初始化模型
    config = sherpa_onnx.OfflinePunctuationConfig(
        model=sherpa_onnx.OfflinePunctuationModelConfig(
            ct_transformer=args.model,
            num_threads=args.threads,
            provider="cpu"
        )
    )
    punct = sherpa_onnx.OfflinePunctuation(config)

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
