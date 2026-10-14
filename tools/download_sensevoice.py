#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Voice2Word SenseVoice 模型下载脚本
支持从 hf-mirror.com 与 github releases 下载 SenseVoice-Small INT8 与 Silero VAD 模型
"""

import os
import sys
import time
import urllib.request

DEST_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "models", "sensevoice")
os.makedirs(DEST_DIR, exist_ok=True)

FILES = [
    {
        "name": "tokens.txt",
        "urls": [
            "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/raw/main/tokens.txt",
            "https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/python-api-examples/tokens.txt"
        ],
        "size": 315894
    },
    {
        "name": "silero_vad.onnx",
        "urls": [
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
            "https://hf-mirror.com/csukuangfj/silero-vad/resolve/main/silero_vad.onnx"
        ],
        "size": 643854
    },
    {
        "name": "model.int8.onnx",
        "urls": [
            "https://hf-mirror.com/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main/model.int8.onnx"
        ],
        "size": 239233841
    }
]

def download_file(url, target_path, expected_size=0):
    print(f"正在从 {url} 下载至 {target_path} ...")
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    
    with urllib.request.urlopen(req, timeout=30) as response, open(target_path, "wb") as out_file:
        total_length = response.getheader("content-length")
        total_size = int(total_length) if total_length else expected_size
        downloaded = 0
        last_print = 0
        start_t = time.time()
        
        while True:
            buffer = response.read(1024 * 1024)
            if not buffer:
                break
            downloaded += len(buffer)
            out_file.write(buffer)
            
            now = time.time()
            if now - last_print >= 1.0 or (total_size and downloaded >= total_size):
                last_print = now
                speed = downloaded / (now - start_t) / 1024 / 1024 if now > start_t else 0
                if total_size:
                    pct = downloaded * 100.0 / total_size
                    print(f"  进度: {downloaded / 1024 / 1024:.1f}MB / {total_size / 1024 / 1024:.1f}MB ({pct:.1f}%) - {speed:.2f} MB/s", flush=True)
                else:
                    print(f"  已下载: {downloaded / 1024 / 1024:.1f}MB - {speed:.2f} MB/s", flush=True)

    print(f"下载完成: {target_path} (大小: {os.path.getsize(target_path)} 字节)")

def main():
    for item in FILES:
        target = os.path.join(DEST_DIR, item["name"])
        if os.path.exists(target) and os.path.getsize(target) > 0:
            print(f"文件已存在，跳过: {target}")
            continue
        
        success = False
        for url in item["urls"]:
            try:
                download_file(url, target, item["size"])
                success = True
                break
            except Exception as e:
                print(f"从 {url} 下载失败: {e}，尝试备用地址...")
                if os.path.exists(target):
                    try:
                        os.remove(target)
                    except:
                        pass
        if not success:
            print(f"无法下载 {item['name']}，请检查网络！", file=sys.stderr)
            sys.exit(1)

    print("SenseVoice 模型下载就绪！")

if __name__ == "__main__":
    main()
