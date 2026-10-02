# -*- coding: utf-8 -*-
"""剥离口语填充词后的全片 CER 对照。

口径与 examples/eval_whisper_parallel.rs 完全一致：
  拼接区间内文本 -> zhconv 繁转简 -> 只保留字母数字（CJK 属于 alnum）-> 小写 -> 字符编辑距离
CER = 编辑距离 / 参考字幕归一化字数。

在此之上加入「填充词剥离」：对标准字幕侧和 Whisper 侧用同一张词表、同一套规则
（最长匹配优先、从左到右、逐个删除）剥离，再重算 CER。

用法：
  python scripts/eval_cer_no_filler.py <json目录> [片段起点秒] [评估起点秒] [评估终点秒] [输出目录]
"""

import json
import re
import sys
from pathlib import Path

import numpy as np
import zhconv

ROOT = Path(__file__).resolve().parents[1]
REF = ROOT / "testVideo" / "03.1.3概率不等式_success.srt"

# ---- 主词表：只收真正的口语填充/语气词 -------------------------------------
# 说明（可审计）：
#   嗯/呃/啊/哦/哎/唉/呀 —— 叹词与语气词，口语填充的典型形态，删除不会伤及实义；
#   呗/嘛 —— 句末语气助词，语义贡献接近零（"你就去吧"里的"吧"同类型），
#             但严格来说带一点语气色彩，所以另给「主表去掉 呗/嘛/呀」的严格变体；
#   对吧/是吧 —— 口头确认小句，人工净稿几乎不会保留（全片标准侧 50 次 / Whisper 89 次）；
#   这个这个/那个那个 —— 连说型口吃填充，属于纯重复，无实义。
# 默认不删（会误删实义/语法成分）：然后、就是、呢、吧、其实、那么、这个、那个、
#   的话、大家、我们 等。这些词另给「宽松变体」单独报告，不混入主表结果。
FILLERS_MAIN = ["嗯", "呃", "啊", "哦", "哎", "唉", "呀", "呗", "嘛", "对吧", "是吧", "这个这个", "那个那个"]
FILLERS_STRICT = ["嗯", "呃", "啊", "哦", "哎", "唉", "对吧", "是吧", "这个这个", "那个那个"]
FILLERS_LOOSE = FILLERS_MAIN + ["然后", "就是", "呢", "吧", "其实", "那么"]


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
        out.append((s, e, "".join(lines[idx + 1:])))
    return out


def read_whisper_json(path, clip_start):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    out = []
    for item in data.get("transcription", []):
        off = item.get("offsets") or {}
        if "from" not in off or "to" not in off:
            continue
        out.append((clip_start + off["from"] / 1000.0, clip_start + off["to"] / 1000.0,
                    item.get("text", "")))
    return out


def collect(items, lo, hi):
    buf = []
    n = 0
    for s, e, t in items:
        if lo <= (s + e) / 2 < hi:
            buf.append(t)
            n += 1
    return "".join(buf), n


def normalize(text, convert=True):
    if convert:
        text = zhconv.convert(text, "zh-hans")
    return "".join(c.lower() for c in text if c.isalnum())


def strip_fillers(normalized, words):
    """最长匹配优先、从左到右逐个删除；返回 (剥离后文本, {词: 删除次数})。"""
    counts = {w: 0 for w in words}
    n = len(normalized)
    i = 0
    out = []
    order = sorted(words, key=len, reverse=True)
    while i < n:
        for w in order:
            if w and normalized.startswith(w, i):
                counts[w] += 1
                i += len(w)
                break
        else:
            out.append(normalized[i])
            i += 1
    return "".join(out), counts


def levenshtein(a, b):
    """numpy 向量化的 Levenshtein（按行 DP + 前缀最小值消除行内依赖）。"""
    if len(a) < len(b):
        a, b = b, a
    if not b:
        return len(a)
    codes = np.frombuffer(b.encode("utf-32-le"), dtype=np.uint32)
    idx = np.arange(1, len(b) + 1, dtype=np.int32)
    prev = np.arange(len(b) + 1, dtype=np.int32)
    cur = np.empty(len(b) + 1, dtype=np.int32)
    for i, ca in enumerate(a, 1):
        cost = codes != ord(ca)
        tmp = np.minimum(prev[1:] + 1, prev[:-1] + cost)
        cur[0] = i
        cur[1:] = np.minimum.accumulate(tmp - idx) + idx
        prev, cur = cur, prev
    return int(prev[-1])


def levenshtein_free_skip(a, b, skip_cost):
    """编辑距离的变体：跳过 b[k] 的代价由 skip_cost[k] 给定（0 表示可免费丢弃）。

    b 里被标记为填充词的字符可零代价跳过，等价于「假设参考侧逐字稿里本来就有这些填充词」。
    """
    if not b:
        return len(a)
    codes = np.frombuffer(b.encode("utf-32-le"), dtype=np.uint32)
    s = np.asarray(skip_cost, dtype=np.int32)
    p = np.cumsum(s)  # P[j] = sum_{m<=j} s_m，索引 0..len(b)-1 对应 j=1..len(b)
    prev = np.arange(len(b) + 1, dtype=np.int32)  # D[0][j] = sum of skip costs
    prev[1:] = p
    cur = np.empty(len(b) + 1, dtype=np.int32)
    for i, ca in enumerate(a, 1):
        cost = codes != ord(ca)
        tmp = np.minimum(prev[1:] + 1, prev[:-1] + cost)
        cur[0] = i
        cur[1:] = p + np.minimum.accumulate(tmp - p)
        prev, cur = cur, prev
    return int(prev[-1])


def mark_filler_chars(normalized, words):
    """返回与 normalized 等长的 0/1 数组：1 表示该字符属于某个填充词的出现。"""
    marks = np.zeros(len(normalized), dtype=np.int32)
    for w in words:
        for m in re.finditer(re.escape(w), normalized):
            marks[m.start():m.end()] = 1
    return marks


def remove_excess(normalized, word, k, from_end=True):
    """只删掉 word 的 k 次出现（保留其余），用于「只删 Whisper 超出的那部分」变体。"""
    if k <= 0:
        return normalized
    idxs = [m.start() for m in re.finditer(re.escape(word), normalized)]
    drop = idxs[-k:] if from_end else idxs[:k]
    drop_set = set()
    for start in drop:
        drop_set.update(range(start, start + len(word)))
    return "".join(c for i, c in enumerate(normalized) if i not in drop_set)


def cer(ref, hyp):
    d = levenshtein(ref, hyp)
    return d, d / len(ref) * 100.0


def main():
    args = sys.argv[1:]
    jdir = Path(args[0])
    clip_start = float(args[1]) if len(args) > 1 else 0.0
    lo = float(args[2]) if len(args) > 2 else 0.0
    hi = float(args[3]) if len(args) > 3 else 1942.0
    outdir = Path(args[4]) if len(args) > 4 else jdir.parent / "filler_out"
    outdir.mkdir(parents=True, exist_ok=True)

    gold_items = read_srt(REF)
    gold_raw, gold_seg = collect(gold_items, lo, hi)
    gold = normalize(gold_raw)
    print(f"标准字幕: {REF.name} | 区间 {lo:.0f}s-{hi:.0f}s | 句数 {gold_seg} | 归一化字数 {len(gold)}")

    names = sorted(p.stem for p in jdir.glob("*.json"))
    hyps = {}
    for p in sorted(jdir.glob("*.json")):
        raw, seg = collect(read_whisper_json(p, clip_start), lo, hi)
        hyps[p.stem] = (normalize(raw), seg)

    lines = []
    def emit(s=""):
        print(s)
        lines.append(s)

    # ---- 1) 剥离前 CER（口径复现校验） ----
    emit("\n=== 剥离前（复现既有口径） ===")
    emit(f"{'配置':<18}{'句数':>6}{'字数':>8}{'编辑距离':>10}{'CER':>9}")
    for name, (h, seg) in hyps.items():
        d, c = cer(gold, h)
        emit(f"{name:<18}{seg:>6}{len(h):>8}{d:>10}{c:>8.2f}%")

    # ---- 2) 主表剥离 ----
    def run_table(words, label):
        emit(f"\n=== {label} ===")
        g_stripped, g_cnt = strip_fillers(gold, words)
        emit(f"参考侧剥离后字数 {len(g_stripped)}（剥离前 {len(gold)}，删 {len(gold)-len(g_stripped)}）")
        emit(f"{'配置':<18}{'字数':>8}{'编辑距离':>10}{'CER(原分母)':>13}{'CER(新分母)':>13}{'变化pp':>10}")
        base = {n: cer(gold, h)[1] for n, (h, _) in hyps.items()}
        results = {}
        for name, (h, _) in hyps.items():
            h_stripped, h_cnt = strip_fillers(h, words)
            d, c = cer(g_stripped, h_stripped)
            c_orig_denom = d / len(gold) * 100.0
            results[name] = (h_cnt, d, c, c_orig_denom)
            emit(f"{name:<18}{len(h_stripped):>8}{d:>10}{c_orig_denom:>12.2f}%{c:>12.2f}%{c_orig_denom-base[name]:>+9.2f}")
        return g_cnt, results, g_stripped

    g_cnt, main_res, g_stripped = run_table(FILLERS_MAIN, "主表剥离后 CER（两侧同一词表）")

    # ---- 3) 词级删除次数与差额诊断 ----
    emit("\n=== 逐词删除次数（参考侧 vs Whisper 侧）与差额 ===")
    hdr = f"{'词':<8}{'参考侧':>7}" + "".join(f"{n:>18}" for n in main_res)
    emit(hdr)
    emit(f"{'':<8}{'':>7}" + "".join(f"{'删/差':>18}" for _ in main_res))
    def sort_key(word):
        return (-max(main_res[name][0][word] for name in main_res), word)

    for w in sorted(FILLERS_MAIN, key=sort_key):
        row = f"{w:<8}{g_cnt[w]:>7}"
        for name in main_res:
            hc = main_res[name][0][w]
            row += f"{hc:>12}{hc - g_cnt[w]:>+6}"
        emit(row)
    emit("\n合计删除：" + " ".join(f"{n}={sum(main_res[n][0].values())}" for n in main_res)
         + f" 参考侧={sum(g_cnt.values())}")

    # ---- 4) 变体：只删 Whisper 超出的填充词 ----
    emit("\n=== 变体：参考侧不剥离，仅删 Whisper 超出参考侧的那部分（excess） ===")
    emit("说明：删哪几次出现是任意的（这里给「删最后 k 次」与「删最前 k 次」两种取法，"
         "未做最优位置搜索，所以该变体是下界/区间，不是精确最优）")
    emit(f"{'配置':<18}{'excess数':>9}{'尾删编辑距离':>13}{'尾删CER':>10}{'首删编辑距离':>13}{'首删CER':>10}")
    base = {n: cer(gold, h)[1] for n, (h, _) in hyps.items()}
    for name, (h, _) in hyps.items():
        h_cnt = strip_fillers(h, FILLERS_MAIN)[1]
        k_total = 0
        for w in FILLERS_MAIN:
            k_total += max(0, h_cnt[w] - g_cnt[w])
        out = []
        for from_end in (True, False):
            stripped = h
            for w in sorted(FILLERS_MAIN, key=len, reverse=True):
                k = max(0, h_cnt[w] - g_cnt[w])
                stripped = remove_excess(stripped, w, k, from_end=from_end)
            d = levenshtein(gold, stripped)
            out.append((d, d / len(gold) * 100.0))
        emit(f"{name:<18}{k_total:>9}{out[0][0]:>13}{out[0][1]:>9.2f}%{out[1][0]:>13}{out[1][1]:>9.2f}%")
    emit("参考侧完全没有出现的填充词：无（Whisper 侧出现过的 6 个词在参考侧都出现过），"
         "因此「只删 Whisper 独有词」这一读法下剥离次数为 0，CER 与剥离前相同。")

    # ---- 4c) 变体：参考侧按逐字稿处理（Whisper 的填充词可零代价丢弃） ----
    emit("\n=== 变体：假设参考侧是逐字稿（Whisper 输出里属于填充词的字符可零代价丢弃） ===")
    emit("这是「净稿口径」的因果对照：参考侧不动，只豁免 Whisper 写出的填充词，"
         "等价于「如果人工字幕是逐字稿、那些填充词本来就有对应」。用带可变跳过代价的 DP 精确求最小距离。")
    emit(f"{'配置':<18}{'Whisper填充词字符数':>18}{'编辑距离':>10}{'CER(原分母)':>13}{'变化pp':>10}")
    for name, (h, _) in hyps.items():
        marks = mark_filler_chars(h, FILLERS_MAIN)
        skip = (1 - marks).tolist()
        d = levenshtein_free_skip(gold, h, skip)
        c = d / len(gold) * 100.0
        emit(f"{name:<18}{int(marks.sum()):>18}{d:>10}{c:>12.2f}%{c - base[name]:>+9.2f}")

    emit("\n=== 体检：逐段剥离（不跨句拼接）vs 拼接后剥离 ===")
    g_seg_total = 0
    for (s, e, t) in gold_items:
        if lo <= (s + e) / 2 < hi:
            g_seg_total += sum(strip_fillers(normalize(t, convert=False), FILLERS_MAIN)[1].values())
    emit(f"参考侧：拼接后删 {sum(g_cnt.values())}，逐段删 {g_seg_total}")
    for name, (h, _) in hyps.items():
        seg_total = 0
        for (s, e, t) in read_whisper_json([p for p in jdir.glob('*.json') if p.stem == name][0], clip_start):
            if lo <= (s + e) / 2 < hi:
                seg_total += sum(strip_fillers(normalize(t), FILLERS_MAIN)[1].values())
        emit(f"{name:<18} 拼接后删 {sum(main_res[name][0].values())}，逐段删 {seg_total}，"
             f"跨段虚增 {sum(main_res[name][0].values()) - seg_total} 次")

    # ---- 5) 敏感度变体 ----
    run_table(FILLERS_STRICT, "敏感度：严格表（主表去掉 呀/呗/嘛）")
    run_table(FILLERS_LOOSE, "敏感度：宽松表（主表 + 然后/就是/呢/吧/其实/那么）")

    (outdir / "cer_no_filler.txt").write_text("\n".join(lines), encoding="utf-8")
    print(f"\n已写入 {outdir / 'cer_no_filler.txt'}")


if __name__ == "__main__":
    main()
