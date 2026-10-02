#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把 tools/sensevoice_runner.py 的 --output JSON 转成 SRT，供 examples/eval_gold.rs 算 CER。

用法:
  python scripts/sensevoice_json_to_srt.py <runner输出.json> <输出.srt>
"""
import json
import sys


def fmt(sec: float) -> str:
    sec = max(0.0, float(sec))
    h = int(sec // 3600)
    m = int((sec % 3600) // 60)
    s = sec % 60
    return f"{h:02d}:{m:02d}:{s:06.3f}".replace(".", ",")


def main() -> int:
    if len(sys.argv) != 3:
        print("用法: sensevoice_json_to_srt.py <runner输出.json> <输出.srt>", file=sys.stderr)
        return 2
    src, dst = sys.argv[1], sys.argv[2]
    with open(src, "r", encoding="utf-8") as f:
        obj = json.load(f)
    segs = obj.get("segments") or []
    lines = []
    for i, seg in enumerate(segs, 1):
        text = (seg.get("text") or "").replace("\r", " ").replace("\n", " ").strip()
        if not text:
            continue
        lines.append(f"{i}\n{fmt(seg['start'])} --> {fmt(seg['end'])}\n{text}\n")
    with open(dst, "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    print(f"[srt] {len(lines)} 条 -> {dst}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
