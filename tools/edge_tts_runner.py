#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Voice2Word Edge-TTS 语音合成脚本
基于微软 Edge-TTS 免费云端接口，实现单句与多句批量时间轴配音导出
"""

import sys
import os
import json
import argparse
import asyncio

try:
    import edge_tts
except ImportError:
    edge_tts = None


def parse_args():
    parser = argparse.ArgumentParser(description="Voice2Word Edge-TTS 语音合成")
    parser.add_argument("--text", type=str, default="", help="待合成文本")
    parser.add_argument("--voice", type=str, default="zh-CN-XiaoxiaoNeural", help="TTS 声音名称")
    parser.add_argument("--rate", type=str, default="+0%", help="语速微调 (如 +10% 或 -5%)")
    parser.add_argument("--pitch", type=str, default="+0Hz", help="语调微调")
    parser.add_argument("--volume", type=str, default="+0%", help="音量微调")
    parser.add_argument("--output", type=str, default="", help="输出音频文件路径")
    parser.add_argument("--batch-json", type=str, default="", help="批量任务 JSON 路径")
    return parser.parse_args()


async def synthesize_one(text: str, voice: str, rate: str, pitch: str, volume: str, output_path: str):
    if not text.strip():
        with open(output_path, "wb") as f:
            pass
        return
    communicate = edge_tts.Communicate(
        text=text,
        voice=voice,
        rate=rate,
        pitch=pitch,
        volume=volume,
    )
    await communicate.save(output_path)


async def main_async(args):
    if edge_tts is None:
        sys.stderr.write("错误: 未找到 edge-tts 库，请先通过 pip install edge-tts 安装。\n")
        sys.exit(2)

    if args.batch_json:
        if not os.path.exists(args.batch_json):
            sys.stderr.write(f"错误: 批量配置文件不存在: {args.batch_json}\n")
            sys.exit(1)
        with open(args.batch_json, "r", encoding="utf-8") as f:
            items = json.load(f)
        for idx, item in enumerate(items):
            t = item.get("text", "")
            v = item.get("voice", args.voice)
            r = item.get("rate", args.rate)
            p = item.get("pitch", args.pitch)
            vol = item.get("volume", args.volume)
            out = item.get("output", "")
            if out:
                os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
                await synthesize_one(t, v, r, p, vol, out)
            print(f"[PROGRESS] {idx + 1}/{len(items)}", flush=True)
    elif args.output:
        os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
        await synthesize_one(args.text, args.voice, args.rate, args.pitch, args.volume, args.output)
        print(f"[DONE] 成功合成音频: {args.output}", flush=True)
    else:
        sys.stderr.write("错误: 缺少 --output 或 --batch-json 参数\n")
        sys.exit(1)


def main():
    args = parse_args()
    asyncio.run(main_async(args))


if __name__ == "__main__":
    main()
