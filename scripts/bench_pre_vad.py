# ─────────────────────────────────────────────────────────────────────────────
# 【已暂停 · 2026-09-29】
# 本脚本依赖 tools/whisper-186-test/ 目录下的可执行文件（whisper-vad-speech-segments.exe /
# whisper-quantize.exe 等）。该目录已不在本仓库中，脚本当前无法运行。
# 它对应的支线（外置 Silero VAD 预压缩 / 模型量化）已由用户确认按「依赖缺失，暂停」结案；
# 这两条支线的产出并未丢失：Q5 量化模型本身已在生产中正常使用。
# 若日后把 tools/whisper-186-test/ 补回仓库，可直接重跑本脚本。
# ─────────────────────────────────────────────────────────────────────────────
import json, re, subprocess, sys, time, wave
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
def _resolve_ffmpeg() -> Path:
    """与 config.rs 同序解析 ffmpeg：config.local.toml → config.toml → 便携默认。
    原先这里硬编码了开发机的绝对路径，换台机器脚本直接不可用。"""
    import re
    for name in ("config.local.toml", "config.toml"):
        cfg = ROOT / name
        if not cfg.exists():
            continue
        m = re.findall(r'^\s*ffmpeg\s*=\s*[\'"]([^\'"]+)[\'"]', cfg.read_text(encoding="utf-8"), re.M)
        if m:
            v = Path(m[-1])
            return v if v.is_absolute() else ROOT / v
    return ROOT / "tools/ffmpeg.exe"


FFMPEG = _resolve_ffmpeg()
VAD = ROOT / "tools/whisper-186-test/Release/whisper-vad-speech-segments.exe"
CLI = ROOT / "tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe"
MODEL = ROOT / "models/whisper/ggml-small-q5_0.bin"
VAD_MODEL = ROOT / "models/whisper/ggml-silero-v6.2.0.bin"
VIDEO = ROOT / "testVideo/03.1.3概率不等式.mp4"
OUT = ROOT / "target/whisper_speed_bench"

def run(args):
    return subprocess.run(
        args, check=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8", errors="replace",
    ).stdout

def vad_ranges(wav, threshold):
    text = run([str(VAD), "-f", str(wav), "-vm", str(VAD_MODEL), "-t", "4", "-vt", str(threshold), "-vsd", "250", "-np", "-ug"])
    out = []
    # whisper.cpp 的 speech-segments 工具输出 10ms tick（60000 = 600 秒），不是 ms。
    for start, end in re.findall(r"Speech segment \d+: start = ([0-9.]+), end = ([0-9.]+)", text):
        out.append((float(start) / 100.0, float(end) / 100.0))
    if not out:
        raise RuntimeError("VAD 没有输出语音区间\n" + text)
    return out

def compress_wav(source, target, ranges, pad=0.03, gap=0.12):
    with wave.open(str(source), "rb") as r:
        params = r.getparams()
        frames = r.readframes(r.getnframes())
    width, rate, channels = params.sampwidth, params.framerate, params.nchannels
    frame_size = width * channels
    chunks = []
    mapped = []
    compressed = 0.0
    silence = b"\x00" * int(gap * rate) * frame_size
    for index, (start, end) in enumerate(ranges):
        start = max(0.0, start - pad)
        end = end + pad
        i0 = int(start * rate) * frame_size
        i1 = min(len(frames), int(end * rate) * frame_size)
        if index:
            chunks.append(silence)
            compressed += gap
        chunks.append(frames[i0:i1])
        actual = (i1 - i0) / frame_size / rate
        mapped.append((compressed, compressed + actual, start, end))
        compressed += actual
    with wave.open(str(target), "wb") as w:
        w.setparams(params._replace(nframes=0))
        w.writeframes(b"".join(chunks))
    return mapped, compressed

def main():
    OUT.mkdir(parents=True, exist_ok=True)
    source = OUT / "clip_300_600_x1_00.wav"
    if not source.exists():
        run([str(FFMPEG), "-hide_banner", "-loglevel", "error", "-y", "-ss", "300", "-t", "600", "-i", str(VIDEO), "-map", "0:a:0", "-vn", "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(source)])
    for threshold in (0.50, 0.55):
        vad_started = time.perf_counter()
        ranges = vad_ranges(source, threshold)
        vad_elapsed = time.perf_counter() - vad_started
        compressed = OUT / f"pre_vad_{threshold:.2f}.wav"
        mapping, compressed_sec = compress_wav(source, compressed, ranges)
        prefix = OUT / f"pre_vad_{threshold:.2f}"
        asr_started = time.perf_counter()
        run([str(CLI), "-m", str(MODEL), "-f", str(compressed), "-l", "zh", "-t", "16", "-p", "1", "-bo", "1", "-bs", "1", "-mc", "32", "-sns", "-oj", "-of", str(prefix), "-nf", "-ng", "--prompt", "以下是普通话录音。", "--carry-initial-prompt"])
        data = json.loads(Path(str(prefix) + ".json").read_text(encoding="utf-8"))
        asr_elapsed = time.perf_counter() - asr_started
        for item in data.get("transcription", []):
            a = item.get("offsets", {}).get("from", 0) / 1000.0
            b = item.get("offsets", {}).get("to", 0) / 1000.0
            def restore(t):
                for c0, c1, o0, o1 in mapping:
                    if c0 <= t <= c1:
                        return o0 + (t - c0)
                    if t < c0:
                        return o0
                return mapping[-1][3]
            item["offsets"]["from"] = round(restore(a) * 1000)
            item["offsets"]["to"] = round(restore(b) * 1000)
        Path(str(prefix) + "_mapped.json").write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
        print(f"threshold={threshold:.2f} vad_segments={len(ranges)} speech={sum(b-a for a,b in ranges):.1f}s compressed={compressed_sec:.1f}s vad={vad_elapsed:.2f}s asr={asr_elapsed:.2f}s total={vad_elapsed + asr_elapsed:.2f}s")

if __name__ == "__main__":
    main()
