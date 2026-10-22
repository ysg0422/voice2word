#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
Voice2Word SenseVoice-Small 极速非自回归语音识别执行器
结合 Silero VAD 与 SenseVoice 非自回归 CTC 模型，实现毫秒级流式分句识别并输出实时 JSON 行。
"""

import sys
import os
import glob as _glob
import re
import json
import time
import wave
import argparse
import tempfile

MISSING_DEPS = []
try:
    import numpy as np
except ImportError:
    MISSING_DEPS.append("numpy")

try:
    import sherpa_onnx
except ImportError:
    MISSING_DEPS.append("sherpa-onnx")

if MISSING_DEPS:
    deps_str = " ".join(MISSING_DEPS)
    sys.stderr.write(
        f"[Voice2Word 错误] SenseVoice 缺少必需的 Python 依赖库: {', '.join(MISSING_DEPS)}\n"
        f"请在终端执行安装: {sys.executable} -m pip install {deps_str}\n"
    )
    sys.exit(10)

def clean_sensevoice_tags(text: str) -> str:
    """清理 SenseVoice 输出的前置特殊标记，如 <|zh|><|NEUTRAL|><|Speech|><|withitn|>"""
    cleaned = re.sub(r"<\|.*?\|>", "", text).strip()
    return cleaned

# 句末标点后切分（保留标点在句尾），覆盖中英文常用句读。
# 先按句末标点切，再对「逗号等句内停顿」补切一刀，得到 2~6s 的常规字幕粒度，
# 而不是动辄 8s 的长句。
_SENTENCE_SPLIT_RE = re.compile(r"(?<=[。！？；!?;])")
_INNER_SPLIT_RE = re.compile(r"(?<=[，,、：:])")
# 无标点超长文本的硬切兜底长度（字符）
_MAX_CHARS_PER_SUB = 42


def _hard_split(piece: str, allowed_sec: float):
    """无标点可用且确实偏长时的硬切：按字数与时长两个上限共同决定片数，取更碎的那个。

    实时流式路径与最终 `optimize_segments` 的拆分口径必须一致，否则界面上「预览是长句、
    落地后变短句」。SenseVoice 自己带标点，正常句都能在标点处切开；硬切只为兜住
    「模型整段没吐出任何标点」这类长段（VAD 密集讲话段可达 30s+）。
    字数与时长都在限内则原样返回——不硬造碎句。
    """
    n = len(piece)
    if n <= 1 or (n <= _MAX_CHARS_PER_SUB and allowed_sec <= 6.0):
        return [piece]
    # 字数上限：每片不超过 _MAX_CHARS_PER_SUB 字；时长上限：每片不超过 6s
    pieces = max(2, int(-(-n // _MAX_CHARS_PER_SUB)), int(-(-allowed_sec // 6.0)))
    pieces = min(pieces, n)
    size = -(-n // pieces)
    return [piece[i : i + size] for i in range(0, n, size)]


# 逗号细切后短于该字数的碎渣（如「然后呢，」）并入相邻片，避免 0.3s 一闪而过
_MIN_PIECE_CHARS = 6


def _merge_short_pieces(pieces):
    """把过短的片并入相邻片：逗号细切会产生「然后呢，」这类 3~5 字碎渣，
    按字符占比分到的时间不足半秒，人眼来不及读（`optimize_segments` 的短句平滑
    只在有空隙时才延展，连续句之间没有空隙，救不了）。"""
    merged = []
    for p in pieces:
        if merged and len(merged[-1]) < _MIN_PIECE_CHARS:
            merged[-1] += p
        else:
            merged.append(p)
    if len(merged) > 1 and len(merged[-1]) < _MIN_PIECE_CHARS:
        # 必须先 pop 再取 `[-1]`：写成 `merged[-2] += merged.pop()` 时，Python 的
        # 下标增强赋值会先读 `merged[-2]`、再求值 `pop()`、最后才写回 `merged[-2]`；
        # pop 让列表短了一位，写回时 `-2` 就越界（恰剩 2 个元素时必崩）。
        tail = merged.pop()
        merged[-1] += tail
    return merged


def split_sentence_timed(text: str, start_sec: float, end_sec: float):
    """把 VAD 段识别出的长文本细切为字幕级短句，按字符占比分配时间轴。

    VAD 段可能长达 30s+（密集讲话时静音边界缺失），直接整段输出为一条字幕
    会造成"一条字幕塞一整段话"。三段式切分：
      1. 按句末标点 `。！？；` 切成句子；够短（≤42 字且 ≤6 秒）的直接成条；
      2. 偏长的句子优先在句内停顿 `，、：` 处切，得到自然断点（碎渣并回邻片）；
      3. 仍偏长的片按字数/时长硬切兜底。
    字符占比分配时间轴。返回 [(text, start, end), ...]，时间严格落在 [start_sec, end_sec] 内。
    """
    duration = max(0.3, end_sec - start_sec)
    sec_per_char = duration / max(1, len(text))

    def fits(p: str) -> bool:
        return len(p) <= _MAX_CHARS_PER_SUB and len(p) * sec_per_char <= 6.0

    final = []
    for sentence in _SENTENCE_SPLIT_RE.split(text):
        sentence = sentence.strip()
        if not sentence:
            continue
        if fits(sentence):
            final.append(sentence)
            continue
        inner = [p.strip() for p in _INNER_SPLIT_RE.split(sentence) if p and p.strip()]
        if len(inner) > 1:
            for p in _merge_short_pieces(inner):
                final.extend(_hard_split(p, len(p) * sec_per_char))
        else:
            final.extend(_hard_split(sentence, len(sentence) * sec_per_char))

    if len(final) <= 1:
        return [(text, start_sec, end_sec)]

    total_chars = sum(len(p) for p in final)
    out = []
    cursor = start_sec
    last_idx = len(final) - 1
    for i, p in enumerate(final):
        share = (len(p) / total_chars) * duration
        seg_end = end_sec if i == last_idx else min(cursor + share, end_sec)
        out.append((p, cursor, seg_end))
        cursor = seg_end
    return out

def resolve_provider(requested: str) -> str:
    """把命令行/配置里的 provider 收敛到 sherpa-onnx 真正认识的取值。

    sherpa-onnx 认的字符串是 "directml"（不是 "dml"——传 "dml" 必落到
    provider.cc 的 "Unsupported string: dml. Fallback to cpu"）。但即便传对了
    "directml"，只要**绑定的 onnxruntime 没编译进 DirectML**，sherpa-onnx 仍会
    静默回落 cpu。这里只做取值收敛，实际生效与否由 `classify_actual_provider`
    按原生日志判定，并回报给上层。
    """
    p = (requested or "cpu").strip().lower()
    if p in ("dml", "directml"):
        return "directml"
    return "cpu"


# sherpa-onnx 的回落提示由 C++ 直接写 fd 2，且句柄在 import 那一刻就被绑定。
# 想在进程内捕获它，必须在 `import sherpa_onnx` **之前**接管 fd 2。
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


def read_audio_samples(input_path: str):
    """读取 16kHz 16-bit 单声道音频或从标准输入读取 WAV"""
    if input_path == "-":
        import io
        raw_data = sys.stdin.buffer.read()
        f = wave.open(io.BytesIO(raw_data))
    else:
        f = wave.open(input_path, "rb")

    with f:
        channels = f.getnchannels()
        sample_width = f.getsampwidth()
        rate = f.getframerate()
        frames = f.getnframes()
        raw_bytes = f.readframes(frames)

    samples_int16 = np.frombuffer(raw_bytes, dtype=np.int16)
    if channels > 1:
        samples_int16 = samples_int16[::channels]

    samples_float32 = samples_int16.astype(np.float32) / 32768.0
    return samples_float32, rate

#: 支持 / 需要额外 decoder 的模型族。放成常量是为了让「参数校验」和「构造」共用一份，
#: 不会出现「校验放过、构造时报错」或反过来。
SUPPORTED_MODEL_TYPES = (
    "sensevoice",
    "sense-voice",
    "paraformer",
    "fire-red-asr",
    "fireredasr",
    "dolphin",
    "dolphin-ctc",
    "nemo-ctc",
    "nemo",
    "canary",
    "nemo-canary",
    "moonshine",
    "transducer",
)
NEEDS_DECODER = ("fire-red-asr", "fireredasr", "canary", "nemo-canary", "transducer")


def discover_sibling(path, *names):
    """在 `path` 同目录里按 `names` 顺序找第一个存在的文件。

    为什么要自动发现而不是让调用方逐个填：sherpa-onnx 的发布包一律把
    `model.int8.onnx` / `tokens.txt`（多文件族的 `decoder*.onnx`）平铺在同一目录。
    让 Rust 侧为每个族硬编码文件名，等于把上游的命名方式抄进源码——上游改名就失败，
    而失败发生在「用户已经下了 1.5 GB」之后。留空即自动查找，找不到再报错。
    """
    base = os.path.dirname(os.path.abspath(path)) if path else ""
    if not base:
        return ""
    for name in names:
        candidate = os.path.join(base, name)
        if os.path.exists(candidate):
            return candidate
    return ""


def discover_by_pattern(path, *patterns):
    """在同目录按**通配**找文件（多文件族的文件名带 int8/fp16 后缀，写死会失效）。

    例：Moonshine 的编码器叫 `encode.int8.onnx`，全精度版叫 `encode.onnx`。
    用 `encode*.onnx` 一次覆盖两种，不必为每个量化档写一份配置。
    """
    base = os.path.dirname(os.path.abspath(path)) if path else ""
    if not base:
        return ""
    for pattern in patterns:
        hits = sorted(_glob.glob(os.path.join(base, pattern)))
        if hits:
            return hits[0]
    return ""


def resolve_optional_paths(args):
    """补全 tokens / decoder：留空就按模型目录自动查找。返回错误消息或 None。"""
    if not args.tokens:
        args.tokens = discover_sibling(args.model, "tokens.txt")
    if not args.tokens:
        return "词表未找到：请在 --tokens 指定，或把 tokens.txt 放在模型同目录"
    if not args.decoder:
        kind = (args.model_type or "").strip().lower().replace("_", "-")
        if kind in NEEDS_DECODER:
            args.decoder = discover_sibling(
                args.model, "decoder.int8.onnx", "decoder.onnx", "decoder.fp16.onnx"
            )
            if not args.decoder:
                return (
                    "decoder 未找到：请在 --decoder 指定，"
                    "或把 decoder*.onnx 放在模型同目录"
                )
    return None


def validate_model_type(args):
    """在**接管 fd 2 之前**校验模型族与必需文件。

    为什么必须提前：进入推理后 `_begin_native_capture()` 会把 fd 2 接管去抓 C++ 日志，
    那时再 `print(..., file=sys.stderr)` 会写进捕获缓冲区、用户什么都看不到。
    """
    kind = (args.model_type or "sensevoice").strip().lower().replace("_", "-")
    if kind not in SUPPORTED_MODEL_TYPES:
        print(
            f"不支持的 --model-type: {args.model_type}（可用的：sensevoice / paraformer / "
            "fire-red-asr / dolphin-ctc / nemo-ctc / nemo-canary）",
            file=sys.stderr,
        )
        sys.exit(1)
    # tokens / decoder 留空时先尝试自动发现，再判定是否真的缺。
    missing = resolve_optional_paths(args)
    if missing:
        print(missing, file=sys.stderr)
        sys.exit(1)


def build_recognizer(sherpa_onnx, args, provider, language):
    """按 --model-type 构造识别器。

    这里的参数名全部照抄内置 sherpa-onnx 1.13.8 的真实签名（用
    inspect.signature 逐个核对过），不靠猜：签名对不上会在 import 之后立刻 TypeError。

    缺省分支必须与改造前**完全一致**——老版本 Rust 侧不传 --model-type。
    """
    kind = (args.model_type or "sensevoice").strip().lower().replace("_", "-")

    if kind in ("sensevoice", "sense-voice"):
        return sherpa_onnx.OfflineRecognizer.from_sense_voice(
            model=args.model,
            tokens=args.tokens,
            num_threads=args.threads,
            use_itn=True,
            language=language,
            provider=provider,
        )

    if kind == "paraformer":
        return sherpa_onnx.OfflineRecognizer.from_paraformer(
            paraformer=args.model,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind in ("fire-red-asr", "fireredasr"):
        if not args.decoder:
            print("FireRedASR 需要 --decoder（encoder/decoder 两个 onnx）", file=sys.stderr)
            sys.exit(1)
        return sherpa_onnx.OfflineRecognizer.from_fire_red_asr(
            encoder=args.model,
            decoder=args.decoder,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind in ("dolphin", "dolphin-ctc"):
        return sherpa_onnx.OfflineRecognizer.from_dolphin_ctc(
            model=args.model,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind in ("nemo-ctc", "nemo"):
        return sherpa_onnx.OfflineRecognizer.from_nemo_ctc(
            model=args.model,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind == "moonshine":
        # Moonshine 是四件套：preprocessor + encoder + 两个 decoder，文件名必须成组出现。
        # 逐项按通配找，缺哪个就点名说哪个——比一句「加载失败」有用得多。
        pre = discover_by_pattern(args.model, "preprocess*.onnx")
        enc = discover_by_pattern(args.model, "encode*.onnx") or args.model
        uncached = discover_by_pattern(args.model, "uncached_decode*.onnx")
        cached = discover_by_pattern(args.model, "cached_decode*.onnx")
        missing = [
            name
            for name, value in (
                ("preprocess*.onnx", pre),
                ("encode*.onnx", enc),
                ("uncached_decode*.onnx", uncached),
                ("cached_decode*.onnx", cached),
            )
            if not value
        ]
        if missing:
            print(f"Moonshine 缺少文件：{'、'.join(missing)}", file=sys.stderr)
            sys.exit(1)
        return sherpa_onnx.OfflineRecognizer.from_moonshine(
            preprocessor=pre,
            encoder=enc,
            uncached_decoder=uncached,
            cached_decoder=cached,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind == "transducer":
        # Parakeet TDT 这类 transducer：encoder + decoder + joiner 三件套。
        joiner = discover_by_pattern(args.model, "joiner*.onnx")
        if not joiner:
            print("transducer 缺少 joiner*.onnx（与 encoder/decoder 同目录）", file=sys.stderr)
            sys.exit(1)
        if not args.decoder:
            print("transducer 缺少 decoder*.onnx", file=sys.stderr)
            sys.exit(1)
        return sherpa_onnx.OfflineRecognizer.from_transducer(
            encoder=args.model,
            decoder=args.decoder,
            joiner=joiner,
            tokens=args.tokens,
            num_threads=args.threads,
            provider=provider,
        )

    if kind in ("canary", "nemo-canary"):
        if not args.decoder:
            print("Canary 需要 --decoder（encoder/decoder 两个 onnx）", file=sys.stderr)
            sys.exit(1)
        # Canary 的 src/tgt 是语言代码；本项目 --language 传的是 zh/en 这类短码，
        # 直接透传，取不到就退回 "en"（Canary 只支持欧洲语系 + 英语）。
        src = (language or "en") if language not in ("", "auto") else "en"
        return sherpa_onnx.OfflineRecognizer.from_nemo_canary(
            encoder=args.model,
            decoder=args.decoder,
            tokens=args.tokens,
            src_lang=src,
            tgt_lang=src,
            num_threads=args.threads,
            provider=provider,
        )

    print(f"不支持的 --model-type: {args.model_type}（尚未接入该模型族）", file=sys.stderr)
    sys.exit(1)


def main():
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description="Voice2Word SenseVoice Runner")
    parser.add_argument("--model", required=True, help="Path to SenseVoice model.int8.onnx")
    # 可选：留空时自动在同目录找 tokens.txt（sherpa-onnx 的模型包都是平铺同名文件）。
    parser.add_argument("--tokens", default="", help="Path to tokens.txt（留空则按模型目录自动查找）")
    parser.add_argument("--vad-model", required=True, help="Path to silero_vad.onnx")
    parser.add_argument("--input", required=True, help="Path to input .wav or '-' for stdin")
    parser.add_argument("--output", help="Optional output JSON path")
    parser.add_argument("--threads", type=int, default=4, help="Number of CPU threads")
    parser.add_argument("--language", default="auto", help="Language: auto, zh, en, etc.")
    parser.add_argument("--total-duration", type=float, default=0.0, help="Total media duration in seconds")
    parser.add_argument("--provider", default="cpu", help="ONNX execution provider: cpu | dml")
    # 模型族。**缺省即 sensevoice**：老版本 Rust 侧不传这个参数，行为必须与以前逐字节一致。
    parser.add_argument("--model-type", default="sensevoice",
                        help="sensevoice | paraformer | fire-red-asr | dolphin-ctc | nemo-ctc | nemo-canary")
    # 少数模型族（FireRedASR / Canary）是 encoder + decoder 两个 onnx，多给一个路径。
    parser.add_argument("--decoder", default="", help="FireRedASR / Canary 的 decoder 路径")
    args = parser.parse_args()

    provider = resolve_provider(args.provider)
    validate_model_type(args)

    if not os.path.exists(args.model):
        print(f"识别模型未找到: {args.model}", file=sys.stderr)
        sys.exit(1)
    if args.decoder and not os.path.exists(args.decoder):
        print(f"decoder 模型未找到: {args.decoder}", file=sys.stderr)
        sys.exit(1)
    if not os.path.exists(args.tokens):
        print(f"SenseVoice 词表未找到: {args.tokens}", file=sys.stderr)
        sys.exit(1)
    if not os.path.exists(args.vad_model):
        print(f"Silero VAD 模型未找到: {args.vad_model}", file=sys.stderr)
        sys.exit(1)

    t0 = time.time()

    # 初始化 SenseVoice 识别器。原生回落日志只在 import 之后才可能打出，因此
    # 整个「导入 + 建会话」都必须处在 fd 2 接管区间内，否则捕获不到回落提示。
    _begin_native_capture()
    try:
        import sherpa_onnx

        recognizer = build_recognizer(
            sherpa_onnx,
            args,
            provider,
            args.language if args.language else "auto",
        )
    finally:
        _native_log = _native_log_text()
        _end_native_capture()

    # 把「请求的 provider / 实际生效的 provider」作为一条独立 JSON 事件回传。
    # 旧版 Rust 侧对未知 event 走 `_ => {}` 忽略，因此这条事件向后兼容。
    actual_provider = classify_actual_provider(provider, _native_log)
    print(json.dumps({
        "event": "provider",
        "requested": provider,
        "actual": actual_provider
    }, ensure_ascii=False), flush=True)

    # 初始化 Silero VAD 检测器
    vad_config = sherpa_onnx.VadModelConfig()
    vad_config.silero_vad.model = args.vad_model
    vad_config.silero_vad.threshold = 0.5
    vad_config.silero_vad.min_silence_duration = 0.25
    vad_config.silero_vad.min_speech_duration = 0.25
    # VAD 段上限 20s：配合句末标点细切，控制单段时间戳误差
    vad_config.silero_vad.max_speech_duration = 20.0
    vad_config.silero_vad.window_size = 512

    vad = sherpa_onnx.VoiceActivityDetector(vad_config, buffer_size_in_seconds=120)

    # 读取输入音频
    samples, sample_rate = read_audio_samples(args.input)
    if sample_rate != 16000:
        print(f"警告：采样率为 {sample_rate}，标准要求 16000Hz", file=sys.stderr)

    window_size = vad_config.silero_vad.window_size

    # 优化点 1：VAD 喂入粒度由 512 samples (32ms) 优化为 16000 samples (1s)，
    # 消除数万次 Python-C++ 交互开销，降低 CPU 调度压力
    chunk_size = 16000
    count = 0
    all_segments = []
    total_dur = args.total_duration if args.total_duration > 0 else (len(samples) / 16000.0)

    # 优化点 2：支持 decode_streams 并行批量前向解码 (首句即刻解码保首屏体感，后续 4 句一批充分发挥 SIMD 向量化)
    BATCH_SIZE = 4
    pending_streams = []
    pending_meta = []

    def flush_pending_streams():
        nonlocal count
        if not pending_streams:
            return
        recognizer.decode_streams(pending_streams)
        for s, (start_sec, end_sec) in zip(pending_streams, pending_meta):
            raw_text = s.result.text
            clean_text = clean_sensevoice_tags(raw_text)
            if clean_text:
                # 长段按句末标点细切为字幕级短句，字符占比分配时间轴，
                # 杜绝"一条字幕塞一整段话"（VAD 密集讲话段可达 30s+）
                for txt, s0, e0 in split_sentence_timed(clean_text, start_sec, end_sec):
                    count += 1
                    ratio = min(1.0, e0 / total_dur) if total_dur > 0 else 0.5
                    seg_obj = {
                        "event": "segment",
                        "index": count,
                        "start": s0,
                        "end": e0,
                        "text": txt,
                        "progress": round(ratio, 4)
                    }
                    all_segments.append({
                        "index": count,
                        "start": s0,
                        "end": e0,
                        "text": txt,
                        "polished": txt
                    })
                    print(json.dumps(seg_obj, ensure_ascii=False), flush=True)
        pending_streams.clear()
        pending_meta.clear()

    # 分块喂入 VAD 并流式识别语音段
    for i in range(0, len(samples), chunk_size):
        chunk = samples[i:i + chunk_size]
        vad.accept_waveform(chunk)
        while not vad.empty():
            seg = vad.front
            speech_samples = seg.samples
            start_sec = round(seg.start / 16000.0, 3)
            dur_sec = round(len(speech_samples) / 16000.0, 3)
            end_sec = round(start_sec + dur_sec, 3)

            stream = recognizer.create_stream()
            stream.accept_waveform(16000, speech_samples)
            pending_streams.append(stream)
            pending_meta.append((start_sec, end_sec))

            vad.pop()

            # 首句 1 句即出，后续按批处理并发
            threshold = 1 if count == 0 else BATCH_SIZE
            if len(pending_streams) >= threshold:
                flush_pending_streams()

    # 处理末尾剩余片段
    vad.flush()
    while not vad.empty():
        seg = vad.front
        speech_samples = seg.samples
        start_sec = round(seg.start / 16000.0, 3)
        dur_sec = round(len(speech_samples) / 16000.0, 3)
        end_sec = round(start_sec + dur_sec, 3)

        stream = recognizer.create_stream()
        stream.accept_waveform(16000, speech_samples)
        pending_streams.append(stream)
        pending_meta.append((start_sec, end_sec))

        vad.pop()

    flush_pending_streams()

    elapsed = round(time.time() - t0, 3)
    finish_obj = {
        "event": "finished",
        "count": count,
        "elapsed_sec": elapsed,
        "segments": all_segments
    }

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            json.dump(finish_obj, f, ensure_ascii=False, indent=2)

    print(json.dumps(finish_obj, ensure_ascii=False), flush=True)

if __name__ == "__main__":
    main()
