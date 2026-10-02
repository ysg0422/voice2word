# -*- coding: utf-8 -*-
"""临时 CER 对比：把候选字幕与人工标准字幕在 05:05-14:55 区间做字符级编辑距离。"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REF = ROOT / "testVideo" / "03.1.3概率不等式_success.srt"
EVAL_START, EVAL_END = 305.0, 895.0

PUNCT = re.compile(r"[\s,，。、；；:：!！?？…—\-—~～\"'“”‘’()（）《》〈〉\[\]【】{}<>·.,;:!?/\\|+=*&^%$#@`]")


def parse_time(value):
    value = value.strip().replace(",", ".")
    parts = value.split(":")
    if len(parts) != 3:
        return None
    try:
        return int(parts[0]) * 3600 + int(parts[1]) * 60 + float(parts[2])
    except ValueError:
        return None


def read_srt(path):
    raw = Path(path).read_text(encoding="utf-8-sig", errors="replace").replace("\r\n", "\n")
    out = []
    for block in raw.strip().split("\n\n"):
        lines = block.split("\n")
        tl = next((l for l in lines if "-->" in l), None)
        if not tl:
            continue
        a, b = tl.split("-->")
        s, e = parse_time(a), parse_time(b)
        if s is None or e is None:
            continue
        idx = lines.index(tl)
        text = "".join(lines[idx + 1:])
        out.append((s, e, text))
    return out


def read_sv_json(path, offset=300.0):
    """SenseVoice runner 输出：{"segments":[{"start":..,"end":..,"text":..}]}，时间相对裁剪片段。"""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    return [
        (offset + s["start"], offset + s["end"], s.get("text", ""))
        for s in data.get("segments", [])
    ]


def collect(items, lo, hi):
    buf = []
    for s, e, t in items:
        mid = (s + e) / 2
        if lo <= mid < hi:
            buf.append(t)
    return PUNCT.sub("", "".join(buf))


def levenshtein(a, b):
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def main():
    ref_items = read_srt(REF)
    ref = collect(ref_items, EVAL_START, EVAL_END)
    print(f"参考字幕字符数: {len(ref)}")
    for cand in sys.argv[1:]:
        if cand.lower().endswith(".json"):
            items = read_sv_json(cand)
        else:
            items = read_srt(cand)
        text = collect(items, EVAL_START, EVAL_END)
        dist = levenshtein(ref, text)
        print(f"{Path(cand).name}: 字符 {len(text)} | 编辑距离 {dist} | CER {dist / len(ref) * 100:.2f}%")


if __name__ == "__main__":
    main()
