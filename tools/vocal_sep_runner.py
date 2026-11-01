#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Voice2Word 人声分离 Runner（UVR MDX-Net 类 ONNX 源分离）

在识别前把「人声 + BGM」的混合音轨分离出人声轨，提升 BGM / 背景人声重素材的
识别精度。DSP（STFT / ISTFT / 时间轴切块 / 融合重叠相加）全部用 numpy 手写：
便携 Python 运行时（tools/python）只有 numpy + onnxruntime(-directml)，没有
librosa / torch。推理走 onnxruntime。

命令行：
    python vocal_sep_runner.py --input <in.wav> --output <out.wav> --model <model.onnx>
        [--provider cpu|directml] [--self-test]

进程契约（与 Rust 侧 src/engines/vocal_sep.rs 配套）：
- 输入：pcm_s16le WAV、44100Hz、立体声（mono 复制声道；其它采样率直接报错并
  提示重新抽取——重采样在 Rust 侧 ffmpeg 现有链路完成，这里绝不悄悄转换，
  把「抽取配置错了」如实暴露出来）；
- 输出：44100Hz 立体声 pcm_s16le 的人声 WAV；
- stdout **最后一行**输出 JSON 结果，进度/警告一律写 stderr（Rust 侧只解析末行）：
  成功 `{"status":"ok","input_sec":..,"output":"<path>","provider":"cpu|directml","elapsed_sec":..}`
  失败 `{"status":"error","error":"..."}` 且退出码非 0。
"""

import argparse
import json
import math
import sys
import time
import wave

# 与 sensevoice_runner 同口径：缺依赖时给出可操作的安装指引并以专用退出码结束，
# 而不是把一段裸 traceback 丢给调用方。
MISSING_DEPS = []
try:
    import numpy as np
except ImportError:
    MISSING_DEPS.append("numpy")

if MISSING_DEPS:
    sys.stderr.write(
        f"[Voice2Word 错误] 人声分离缺少必需的 Python 依赖库: {', '.join(MISSING_DEPS)}\n"
        f"请在终端执行安装: {sys.executable} -m pip install {' '.join(MISSING_DEPS)}\n"
    )
    sys.exit(10)


# ───────────────────────── MDX-Net 超参常量表 ─────────────────────────
# 来源：UVR（Ultimate Vocal Remover）MDX-Net 系列的推理约定（UVR `onnx_mdx.py`
# 的 n_fft / hop 配置）与其 ONNX 导出模型的训练配置。集中放在一处，是因为这些
# 数必须与模型训练时逐位一致；散落在代码里极易改漏一处，导致「输出能听但全是
# 相位噪声」这类很难归因的坏结果。

#: 采样率：MDX-Net 系列按 44.1kHz 训练。非 44100 输入直接报错（见模块文档）。
SAMPLE_RATE = 44100

#: STFT 点数 / 帧移：UVR 的 MDX-Net 推理用 n_fft=6144、hop=1024
#: （≈139ms 分析窗、23ms 移帧）。改这两个数会与模型的频率/时间分辨率错位。
N_FFT = 6144
HOP = 1024

#: center pad 宽度：librosa `stft(center=True, pad_mode='reflect')` 的语义——
#: 两端各 reflect 补 N_FFT//2，使首尾帧「居中」于首尾样本；重建后裁掉。
CENTER_PAD = N_FFT // 2

#: 频率维 bin 数 dim_f：rfft(6144) 共 3073 bin，UVR 惯例丢掉 Nyquist bin、
#: 只取前 3072（dim_f=3072）。模型输出同样只有 3072 bin，回填时 Nyquist 置 0。
DEFAULT_DIM_F = N_FFT // 2

#: 时间维块长 dim_t 的兜底值：正常从模型输入形状读取（UVR 常见 256），
#: 只有模型输入是**动态形状**（ONNX 导出时未固化）时才用它。
DEFAULT_DIM_T = 256

#: 单次处理的输出样本上限（60s）：谱的工作集与该值成正比（60s 立体声谱约
#: 100+MB），整段长音频一次建谱会把内存吃爆。分片执行与整段执行在数学上完全
#: 等价——每个输出样本只依赖其覆盖帧附近的局部数据，且所有归一化（逐块 MAX
#: 归一化、窗² 加权归一化）都是逐块/逐样本的，不依赖远端信息。自测里有一条
#: 专门钉住「分片 ≡ 整段」。
SLAB_OUTPUT_SAMPLES = SAMPLE_RATE * 60

#: 幅度补偿表（UVR model_data.json 的 per-model `compensate`）：部分模型输出
#: 整体偏大/偏小，UVR 用一个常数拉回后再出 wav。按**模型文件名**（小写、去掉
#: `.onnx`）查表，查不到按 1.0（不补偿）。
#: ⚠ 表内条目为 1.0 占位：离线环境拿不到 UVR 的 model_data.json 逐项核对。
#: 上线后需用真模型与 UVR 输出对照校准（比较分离轨 RMS / 试听），把真实
#: compensate 值填进来。补偿是常数乘，位置在「融合之后、ISTFT 之前」，
#: 与逐块乘回等价（融合权重归一化后和恒为 1）。
COMPENSATION_BY_MODEL = {
    "uvr-mdx-net-inst_hq_3": 1.0,
    "uvr-mdx-net-inst_hq_2": 1.0,
    "uvr-mdx-net-inst_hq_1": 1.0,
    "uvr-mdx-net-voc_ft": 1.0,
    "uvr-mdx-net-main": 1.0,
}


class VocalSepError(Exception):
    """可操作的业务失败：main() 把 message 原样写进 stderr 与 JSON 结果。"""


# ───────────────────────── provider 收敛与回落可见化 ─────────────────────────

def resolve_provider(requested: str) -> str:
    """把命令行/配置里的 provider 收敛到本 runner 真正认识的取值。

    语义抄 `punc_runner.resolve_provider`（两处必须同口径）：onnxruntime 的
    DirectML 执行提供器名是 `DmlExecutionProvider`，配置里习惯写 `dml`；
    而即便请求对了，绑定的 onnxruntime 未编译进 DirectML 时仍会**静默回落**
    cpu。这里只做取值收敛（dml → directml），实际生效与否由建会话时判定并
    在 stderr 打出回落告警（绝不静默），最终以 JSON 的 `provider` 回报实际值。
    """
    p = (requested or "cpu").strip().lower()
    if p in ("dml", "directml"):
        return "directml"
    return "cpu"


def ort_provider_name(provider: str) -> str:
    """runner 取值 → onnxruntime 的执行提供器名。"""
    return "DmlExecutionProvider" if provider == "directml" else "CPUExecutionProvider"


# ───────────────────────── WAV 读写（只做格式校验，不做重采样） ─────────────────────────

def read_wav_stereo(path: str):
    """读入 pcm_s16le WAV，返回 float32 立体声数组 (2, N)，幅度约 [-1, 1)。

    mono 复制双声道；采样率/位深/声道数不合契约时**报错并给出重新抽取的指引**
    （悄悄转换会把上游 ffmpeg 抽取配置的错误藏起来，等识别精度掉了才被发现）。
    """
    try:
        with wave.open(path, "rb") as w:
            n_channels = w.getnchannels()
            samp_width = w.getsampwidth()
            sample_rate = w.getframerate()
            n_frames = w.getnframes()
            raw = w.readframes(n_frames)
    except (OSError, wave.Error) as exc:
        raise VocalSepError(f"读取输入 WAV 失败（{path}）：{exc}") from exc

    if sample_rate != SAMPLE_RATE:
        raise VocalSepError(
            f"输入 WAV 采样率是 {sample_rate}Hz，人声分离要求 {SAMPLE_RATE}Hz："
            f"请在抽取音频时重新抽取（ffmpeg -ar 44100 -ac 2 -c:a pcm_s16le），不要事后重采样"
        )
    if samp_width != 2:
        raise VocalSepError(
            f"输入 WAV 采样位深是 {samp_width * 8}bit，要求 pcm_s16le（16bit）：请重新抽取"
        )
    if n_channels not in (1, 2):
        raise VocalSepError(
            f"输入 WAV 是 {n_channels} 声道，要求立体声（mono 会自动复制声道）：请重新抽取为 -ac 2"
        )

    x = np.frombuffer(raw, dtype=np.int16).reshape(-1, n_channels).T.astype(np.float32)
    x /= 32768.0
    if n_channels == 1:
        # mono → 立体声：复制声道（不是补零），保证输出永远是 2 声道契约。
        x = np.repeat(x, 2, axis=0)
    if not np.isfinite(x).all():
        raise VocalSepError(f"输入 WAV 含 NaN/Inf 样本（{path}）：源文件已损坏，请重新抽取")
    return x


def write_wav_stereo(path: str, y) -> None:
    """把 float32 立体声数组 (2, N) 写成 44100Hz pcm_s16le WAV。

    先裁剪到 [-1, 1] 再转 int16：分离结果没有硬上界（补偿、掩码都可能过冲），
    不裁剪的话 int16 回绕会产生刺耳的爆音——裁剪是数值卫生，不是音量处理。
    """
    clipped = np.clip(y, -1.0, 1.0)
    pcm = np.round(clipped * 32767.0).astype(np.int16)
    interleaved = np.ascontiguousarray(pcm.T).reshape(-1)
    try:
        with wave.open(path, "wb") as w:
            w.setnchannels(2)
            w.setsampwidth(2)
            w.setframerate(SAMPLE_RATE)
            w.writeframes(interleaved.tobytes())
    except OSError as exc:
        raise VocalSepError(f"写入输出 WAV 失败（{path}）：{exc}") from exc


# ───────────────────────── numpy 手写 STFT / ISTFT ─────────────────────────

def hann_periodic(n: int):
    """周期 hann 窗（分母 n），与 librosa 的 `window='hann'` 同口径。

    为什么不用 `np.hanning(n)`：那是**对称**窗（分母 n-1），相邻两窗在 2 倍
    重叠（步长 n/2）下之和不再是常数 1，融合归一化的「常数和」性质就不成立。
    """
    return np.hanning(n + 1)[:n].astype(np.float32)


# 分析窗与「窗² 加权重叠相加」用的平方窗，全程共用一份（长度恒为 N_FFT）。
_STFT_WINDOW = hann_periodic(N_FFT)
_STFT_WINDOW_SQ = _STFT_WINDOW * _STFT_WINDOW


def stft_block(xp, f_lo: int, f_hi: int):
    """xp（center-padded 立体声）上全局帧区间 [f_lo, f_hi) 的短时谱。

    帧 k 覆盖 xp[k*HOP : k*HOP + N_FFT]（先乘 hann 窗再 rfft），返回
    (2, N_FFT//2 + 1, f_hi - f_lo) complex64——**含全部 rfft bin**（Nyquist 在内），
    是否截到 dim_f 由调用方按模型形状决定。

    直接从 xp 取帧意味着帧的定义与「整段一次算」逐位一致（xp 本身就是全局
    reflect pad 的结果），分片执行不需要在片边界再造一次 reflect pad。
    """
    n_frames = f_hi - f_lo
    if n_frames <= 0:
        raise VocalSepError(f"stft_block 帧区间为空：[{f_lo}, {f_hi})")
    start = f_lo * HOP
    seg = xp[:, start : start + (n_frames - 1) * HOP + N_FFT]
    # sliding_window_view 得到 (2, 滑窗数, N_FFT)，按 HOP 抽帧恰好 n_frames 个。
    frames = np.lib.stride_tricks.sliding_window_view(seg, N_FFT, axis=-1)[:, ::HOP, :]
    windowed = frames * _STFT_WINDOW
    spec = np.fft.rfft(windowed, n=N_FFT, axis=-1)  # (2, n_frames, F_full)
    # 统一成 complex64：numpy 2.x 对 float32 输入给 complex64，但显式收口一次，
    # 避免版本差异把整条谱悄悄升成 complex128（长音频直接翻倍内存）。
    return spec.transpose(0, 2, 1).astype(np.complex64, copy=False)


def istft_ola(spec_full, frame_lo: int, t0: int, t1: int):
    """把帧区间 [frame_lo, frame_lo + nf) 的谱重建回 xp 坐标样本 [t0, t1)。

    rfft 逆变换 + 窗² 加权重叠相加（与 stft 同参）：分析时乘过一次 hann，
    合成再乘一次 ⇒ 总权重 hann²，按 hann² 的累加和归一化。归一化后每个样本的
    有效权重和恒为 1（常数和），恒等谱可精确重建出原波形。
    注意谱的排布是 (2, F, nf)——频率维在**轴 1**，逆变换必须打在轴 1 上
    （打在轴 -1 上就成了「沿帧逆变换」，重建出来的是完全无关的垃圾）。
    """
    n_frames = spec_full.shape[-1]
    frames = np.fft.irfft(spec_full, n=N_FFT, axis=1).astype(np.float32, copy=False)
    buf_len = (n_frames - 1) * HOP + N_FFT
    acc = np.zeros((2, buf_len), dtype=np.float32)
    wsum = np.zeros(buf_len, dtype=np.float32)
    for k in range(n_frames):
        s = k * HOP
        acc[:, s : s + N_FFT] += frames[:, :, k] * _STFT_WINDOW
        wsum[s : s + N_FFT] += _STFT_WINDOW_SQ
    # 有效区间内 wsum 必 > 0（每个样本至少被一个「窗权重非 0」的帧覆盖）；
    # 缓冲两端的孤立 0 用哨兵兜住，避免 0/0 变成 NaN 一路带下去。
    np.maximum(wsum, 1e-12, out=wsum)
    acc /= wsum
    lo = t0 - frame_lo * HOP
    return acc[:, lo : lo + (t1 - t0)]


# ───────────────────────── 切块 + MAX 归一化 + hann² 融合 ─────────────────────────

def compensation_for(model_path: str) -> float:
    """按模型文件名查幅度补偿（小写、去 `.onnx`），查不到按 1.0。"""
    name = model_path.replace("\\", "/").rsplit("/", 1)[-1].lower()
    if name.endswith(".onnx"):
        name = name[: -len(".onnx")]
    return COMPENSATION_BY_MODEL.get(name, 1.0)


def model_outputs_instrumental(model_name: str) -> bool:
    """UVR 命名惯例判定模型直接输出的是哪一侧。

    模型名含 `Inst`（不分大小写，如 `UVR-MDX-NET-Inst_HQ_3`）＝**伴奏模型**：
    ONNX 直接输出的就是伴奏，人声要靠「原声 − 输出」拿；其余（`Kim_Vocal_2`
    这类）默认是**人声模型**：ONNX 直接输出人声。

    为什么必须区分：真机验证踩过——把 Inst 模型的输出当人声写出，纯正弦
    （根本没有人声）会原样通过「人声」输出，管线等于没分离。
    """
    return "inst" in model_name.lower()


def resolve_output_side(model_name: str, stems: str) -> str:
    """决定最终产物取「模型输出」还是「原声 − 模型输出」，纯函数便于单测。

    - `stems="vocals"`（默认）+ 伴奏模型 → residual（相减得人声）
    - `stems="vocals"` + 人声模型 → direct（输出即人声）
    - `stems="instrumental"` + 伴奏模型 → direct（输出即伴奏）
    - `stems="instrumental"` + 人声模型 → residual（相减得伴奏）

    即「模型侧与目标侧相同 → direct，不同 → residual」。
    """
    if stems not in ("vocals", "instrumental"):
        raise VocalSepError(f"--stems 只接受 vocals / instrumental，收到: {stems}")
    model_is_inst = model_outputs_instrumental(model_name)
    want_vocals = stems == "vocals"
    return "residual" if model_is_inst == want_vocals else "direct"


def _apply_model(model, seg, dim_f: int, dim_t: int):
    """单个 (2, dim_f, dim_t) 复数块 → 人声复数估计。

    UVR 的 MAX 归一化：块内按 max(|x|) 归一后推理，结果乘回——模型只见过
    归一化谱，绝对幅度进出都不参与推理，数值范围稳定。

    输出形状分支（以**实际输出形状**为准，不是猜模型语义）：
    - (1, 4, dim_f, dim_t)：与输入同形 ⇒ 直接就是人声的复数谱
      （2 声道 × (实部, 虚部)，平面序与输入一致：ch0_re, ch0_im, ch1_re, ch1_im）；
    - (1, 2, dim_f, dim_t)：每声道一个**实值**图（人声幅度谱/掩码），按实虚部
      广播乘到输入复数谱上（实部虚部同乘一个增益）⇒ 掩码语义；
    - 其它形状：直接报错，不猜。
    """
    max_abs = float(np.abs(seg).max())
    if max_abs == 0.0:
        # 全零块不推理：除以 0 会炸，且人声估计理应也是 0。
        return np.zeros_like(seg)
    norm = seg / max_abs

    inp = np.empty((1, 4, dim_f, dim_t), dtype=np.float32)
    inp[0, 0] = norm[0].real
    inp[0, 1] = norm[0].imag
    inp[0, 2] = norm[1].real
    inp[0, 3] = norm[1].imag

    out = np.asarray(model(inp), dtype=np.float32)
    # 数值卫生：NaN/Inf 一出现就说明模型/输入有问题（量化损坏、显存溢出、
    # 输入谱已坏），继续跑下去只会把整段音频毁掉——立刻报错并点名原因。
    if not np.isfinite(out).all():
        raise VocalSepError("模型输出出现 NaN/Inf：模型文件或输入谱已损坏（检查模型下载是否完整、provider 是否稳定）")

    if out.shape == (1, 4, dim_f, dim_t):
        est = np.empty((2, dim_f, dim_t), dtype=np.complex64)
        est[0] = out[0, 0] + 1j * out[0, 1]
        est[1] = out[0, 2] + 1j * out[0, 3]
    elif out.shape == (1, 2, dim_f, dim_t):
        # 实值图按「实虚部广播」：掩码语义，实部虚部同乘一个增益。
        est = out[0] * norm
    else:
        raise VocalSepError(
            f"模型输出形状 {out.shape} 不受支持：期望 (1, 4, {dim_f}, {dim_t}) 或 (1, 2, {dim_f}, {dim_t})，"
            f"这不是 UVR MDX-Net 类模型（或导出方式不同），请换模型/联系维护者"
        )
    return est * max_abs


def blend_estimate(model, spec, spec_lo: int, dim_f: int, dim_t: int, compensation: float,
                   target_lo: int, target_hi: int, total_frames: int):
    """对全局帧区间 [target_lo, target_hi) 计算人声估计（含切块与融合）。

    时间轴切块：每块 dim_t 帧、步长 dim_t//2（2 倍重叠）；融合窗取 hann²，
    重叠相加后按 hann² 累加和归一化——归一化后有效权重和恒为 1（常数和），
    两块给出相同估计时结果严格等于该估计。块网格是**全局**的（s = c·step），
    因此分片执行与整段执行的切块完全对齐。
    """
    step = dim_t // 2
    blend_win_sq = hann_periodic(dim_t) ** 2

    n_frames = target_hi - target_lo
    acc = np.zeros((2, dim_f, n_frames), dtype=np.complex64)
    wsum = np.zeros(n_frames, dtype=np.float32)

    # 会影响目标帧的块：起点 s ∈ (target_lo - dim_t, target_hi)，且 s 落在全局
    # 网格上（c ≥ -1：c=-1 的前置块兜住首帧的窗权重 0，见下）。
    c_min = max(-1, (target_lo - dim_t) // step)
    c_max = (target_hi - 1) // step
    for c in range(c_min, c_max + 1):
        s = c * step
        if s >= target_hi or s + dim_t <= target_lo:
            continue
        # 块内逐帧对齐全局帧号：全局范围 [0, total_frames) 外的帧视为 0
        # （首尾不足一块时的零填充，与「整段算」的处理一致）。
        seg = np.zeros((2, dim_f, dim_t), dtype=np.complex64)
        g_lo = max(0, s, spec_lo)
        g_hi = min(total_frames, s + dim_t, spec_lo + spec.shape[-1])
        if g_hi > g_lo:
            seg[:, :, g_lo - s : g_hi - s] = spec[:, :, g_lo - spec_lo : g_hi - spec_lo]

        est = _apply_model(model, seg, dim_f, dim_t)

        j_lo = max(target_lo, s)
        j_hi = min(target_hi, s + dim_t)
        w = blend_win_sq[j_lo - s : j_hi - s].reshape(1, 1, -1)
        acc[:, :, j_lo - target_lo : j_hi - target_lo] += est[:, :, j_lo - s : j_hi - s] * w
        wsum[j_lo - target_lo : j_hi - target_lo] += blend_win_sq[j_lo - s : j_hi - s]

    # 目标帧必被至少一个「窗权重非 0」的块覆盖（块网格步长是 dim_t/2，
    # 且 c=-1 的前置块把首帧的 w[0]=0 兜住了）；哨兵只防意外，不掩盖 0 权重。
    if (wsum <= 0).any():
        raise VocalSepError("融合权重出现 0（内部错误）：切块网格未能覆盖全部目标帧")
    out = acc / wsum
    if compensation != 1.0:
        out *= compensation
    return out


def separate_signal(x, model, dim_f: int, dim_t: int, compensation: float,
                    slab_samples: int = SLAB_OUTPUT_SAMPLES):
    """完整分离流程：center pad → 分片建谱 → 切块推理融合 → ISTFT → 裁掉 pad。

    分片只是执行细节（见 [`SLAB_OUTPUT_SAMPLES`]）：每片只对自己负责的输出样本
    区间求解，覆盖帧取全、归一化逐块/逐样本，结果与整段一次算严格一致。
    """
    if dim_t < 2:
        raise VocalSepError(f"dim_t={dim_t} 非法：至少为 2（切块步长 dim_t//2 ≥ 1）")
    if dim_f > N_FFT // 2:
        raise VocalSepError(f"dim_f={dim_f} 非法：超过 rfft 可用 bin 数 {N_FFT // 2}（N_FFT={N_FFT}）")

    n_samples = x.shape[-1]
    if n_samples <= CENTER_PAD:
        raise VocalSepError(
            f"输入音频过短（{n_samples} 样本 ≤ {CENTER_PAD}）：无法做 center pad，至少需要约 1 秒"
        )
    xp = np.pad(x, ((0, 0), (CENTER_PAD, CENTER_PAD)), mode="reflect")
    total_frames = n_samples // HOP + 1  # librosa stft(center=True) 的帧数语义
    out = np.zeros_like(x)

    t_begin = CENTER_PAD
    t_end = CENTER_PAD + n_samples
    for t0 in range(t_begin, t_end, slab_samples):
        t1 = min(t0 + slab_samples, t_end)
        # 输出样本 [t0, t1) 的覆盖帧 = [TL, TH)（含首尾，右开）：
        # 帧 k 覆盖 xp[k*HOP : k*HOP+N_FFT] ⇒ 样本 t 的覆盖帧上界 t//HOP、
        # 下界 (t-N_FFT)//HOP + 1。
        tl = max(0, (t0 - N_FFT) // HOP + 1)
        th = min(total_frames, (t1 - 1) // HOP + 1)
        # 再向外扩 dim_t 帧：融合块的支撑域就是这么多（块内 MAX 归一化只依赖
        # 块自身的 dim_t 帧），扩完之后目标帧用到的每一块都在本片内算齐。
        f_lo = max(0, tl - dim_t - 1)
        f_hi = min(total_frames, th + dim_t + 1)
        spec = stft_block(xp, f_lo, f_hi)[:, :dim_f, :]
        est = blend_estimate(model, spec, f_lo, dim_f, dim_t, compensation, tl, th, total_frames)
        full = np.zeros((2, est.shape[1] + 1, est.shape[2]), dtype=np.complex64)
        full[:, :dim_f, :] = est
        y_seg = istft_ola(full, tl, t0, t1)
        out[:, t0 - t_begin : t1 - t_begin] = y_seg
    return out


# ───────────────────────── ONNX 模型封装（惰性导入 onnxruntime） ─────────────────────────

class OnnxSepModel:
    """ONNX 会话薄封装：调用签名与自测用的假模型一致（`(1,4,dim_f,dim_t)` 进）。"""

    def __init__(self, session, input_name: str):
        self.session = session
        self.input_name = input_name

    def __call__(self, inp):
        return self.session.run(None, {self.input_name: inp})[0]


def _static_dim(d):
    """ONNX 形状里的静态维（动态维会是 str / None）。"""
    return d if isinstance(d, int) and d > 0 else None


def infer_dims(session):
    """从模型输入形状 `[1, 4, dim_f, dim_t]` 推断 dim_f / dim_t。

    4 个平面 = 2 声道 × (实部, 虚部)。动态维按 UVR 惯例兜底（dim_f=3072、
    dim_t=256）并在 stderr 点名——形状猜错的表现是「输出能听但糊」，必须留痕。
    """
    inputs = session.get_inputs()
    if len(inputs) != 1:
        raise VocalSepError(
            f"模型输入有 {len(inputs)} 个（期望 1 个）：这不是 UVR MDX-Net 类单输入模型"
        )
    shape = inputs[0].shape
    if len(shape) != 4:
        raise VocalSepError(
            f"模型输入形状 {shape} 不是 [1, 4, dim_f, dim_t]：不是 UVR MDX-Net 类模型"
        )
    batch, ch, dim_f_raw, dim_t_raw = shape
    if _static_dim(batch) not in (None, 1):
        raise VocalSepError(f"模型输入 batch 维是 {batch}，期望 1：不支持批量推理模型")
    if _static_dim(ch) not in (None, 4):
        raise VocalSepError(f"模型输入通道维是 {ch}，期望 4（2 声道 × 实虚部）：不是 UVR MDX-Net 类模型")

    dim_f = _static_dim(dim_f_raw)
    dim_t = _static_dim(dim_t_raw)
    if dim_f is None or dim_t is None:
        print(
            f"[vocal_sep][警告] 模型输入含动态形状 {shape}，按 UVR 惯例取 "
            f"dim_f={DEFAULT_DIM_F}, dim_t={DEFAULT_DIM_T} 兜底",
            file=sys.stderr,
        )
    dim_f = DEFAULT_DIM_F if dim_f is None else dim_f
    dim_t = DEFAULT_DIM_T if dim_t is None else dim_t
    if dim_f > N_FFT // 2:
        raise VocalSepError(
            f"模型 dim_f={dim_f} 超过 N_FFT={N_FFT} 的可用 bin 数 {N_FFT // 2}：请核对模型是否 MDX-Net 类"
        )
    if dim_t < 2:
        raise VocalSepError(f"模型 dim_t={dim_t} 非法：至少为 2")
    return dim_f, dim_t


def create_onnx_model(model_path: str, provider_req: str):
    """惰性导入 onnxruntime、建会话，返回 (model, dim_f, dim_t, actual_provider)。

    回落语义与 `punc_runner` 同口径：请求 directml 而绑定的 onnxruntime 没编译
    进 DirectML 时会**静默回落** cpu——这里把回落显式写到 stderr，并把实际生效
    的 provider 放进 stdout JSON，让上层能把「静默回落」变成可见告警。
    """
    try:
        import onnxruntime as ort
    except ImportError as exc:
        raise VocalSepError(
            "缺少 onnxruntime：人声分离推理需要 Python 包 onnxruntime(-directml)。"
            "请执行 `pip install onnxruntime-directml==1.23.0`（或用 "
            "scripts/build_python_runtime.ps1 重建内置便携运行时）后重试"
        ) from exc

    want = ort_provider_name(provider_req)
    available = list(ort.get_available_providers())
    if want not in available:
        print(
            f"[vocal_sep][回落] 当前 onnxruntime 未提供 {want}（可用：{available}），改用 CPU 执行",
            file=sys.stderr,
        )
        want = "CPUExecutionProvider"
    try:
        session = ort.InferenceSession(model_path, providers=[want])
    except Exception as exc:
        if want != "CPUExecutionProvider":
            # 驱动/DLL 问题（DirectML.dll 缺位、显卡驱动异常）时 ORT 可能直接抛错。
            # 回落 CPU 重试并留痕，而不是把一次可恢复的失败变成整条流水线失败。
            print(
                f"[vocal_sep][回落] 以 {want} 建会话失败（{exc}），改用 CPU 重试",
                file=sys.stderr,
            )
            session = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
        else:
            raise VocalSepError(f"加载人声分离模型失败（{model_path}）：{exc}") from exc

    actual = "directml" if any(p.startswith("Dml") for p in session.get_providers()) else "cpu"
    if actual != provider_req:
        print(
            f"[vocal_sep][回落] 请求 provider={provider_req}，实际生效 provider={actual}",
            file=sys.stderr,
        )

    dim_f, dim_t = infer_dims(session)
    input_name = session.get_inputs()[0].name
    return OnnxSepModel(session, input_name), dim_f, dim_t, actual


# ───────────────────────── --self-test（不加载模型，纯 numpy 自检） ─────────────────────────

def _snr_db(ref, est) -> float:
    """信号噪声比（dB）：10·log10(信号功率 / 误差功率)。"""
    ref64 = np.asarray(ref, dtype=np.float64)
    err = ref64 - np.asarray(est, dtype=np.float64)
    p_ref = float(np.sum(ref64 * ref64))
    p_err = float(np.sum(err * err))
    if p_err <= 0.0:
        return float("inf")
    return 10.0 * math.log10(p_ref / p_err)


def _self_test_signal(sec: float = 3.0):
    """合成测试信号：双声道正弦（440/660Hz）+ 少量噪声。固定随机种子保证可复现。"""
    n = int(SAMPLE_RATE * sec)
    t = np.arange(n, dtype=np.float32) / SAMPLE_RATE
    rng = np.random.default_rng(20261022)
    left = 0.5 * np.sin(2 * np.pi * 440.0 * t) + 0.02 * rng.standard_normal(n)
    right = 0.4 * np.sin(2 * np.pi * 660.0 * t) + 0.02 * rng.standard_normal(n)
    return np.stack([left, right]).astype(np.float32)


class IdentityModel:
    """恒等假模型（输出同形 (1,4,dim_f,dim_t)）：直接回传输入谱 = 人声即混合音。"""

    def __call__(self, inp):
        return inp.copy()


class OnesMaskModel:
    """全 1 掩码假模型（输出 (1,2,dim_f,dim_t)）：掩码恒 1 ⇒ 估计 = 输入谱。"""

    def __call__(self, inp):
        return np.ones((1, 2, inp.shape[2], inp.shape[3]), dtype=np.float32)


def _cola_error(window, hop: int) -> float:
    """窗在给定步长下重叠相加的最大偏离（越接近 0 越是「常数和」）。

    按**周期**移位求和：周期窗在「重叠数 = 窗长/步长」的全部移位上求和，
    只有 DC 分量存活 ⇒ 和恒为常数（hann@2 倍重叠 = 1，hann²@6 倍重叠 = 2.25）。
    这正是「步长取窗长一半/六分之一」的数学依据，也是输出无增益纹波的保证。
    """
    n = len(window)
    w = window.astype(np.float64)
    total = np.zeros(n, dtype=np.float64)
    k = 0
    while k * hop < n:
        total += np.roll(w, -k * hop)
        k += 1
    return float(np.max(np.abs(total - total.mean())))


def run_self_test() -> bool:
    """四项 DSP 自检 + 一条方向判定自检。全部通过返回 True。

    不加载任何 ONNX 模型（用恒等假模型走完整切块/融合路径），因此它验证的是
    **DSP 管线本身**的正确性；真实模型的效果仍需真模型素材验证。
    """
    t0 = time.time()
    checks = []

    def check(name: str, ok: bool, detail: str):
        checks.append((name, ok, detail))
        mark = "✓" if ok else "✗"
        print(f"[自测] {mark} {name}: {detail}", file=sys.stderr)

    x = _self_test_signal()

    # ① STFT/ISTFT 往返：全 bin 谱（不截 Nyquist）过一遍 rfft/irfft + 窗² 重叠
    # 相加，误差只应有浮点量级 ⇒ SNR > 60dB。
    xp = np.pad(x, ((0, 0), (CENTER_PAD, CENTER_PAD)), mode="reflect")
    total_frames = x.shape[-1] // HOP + 1
    spec_full = stft_block(xp, 0, total_frames)
    y_rt = istft_ola(spec_full, 0, CENTER_PAD, CENTER_PAD + x.shape[-1])
    snr_rt = _snr_db(x, y_rt)
    check("① STFT/ISTFT 往返", snr_rt > 60.0, f"SNR={snr_rt:.1f}dB（要求 > 60dB）")

    # ② 重叠相加常数和：(a) 融合窗 hann @ 步长 dim_t/2 的和恒为 1；
    # (b) ISTFT 的 hann² @ 步长 N_FFT/6 的和恒为常数；(c) 切块融合对**恒常输入**
    # 的输出恒为同一常数（覆盖网格、前置块、归一化全程走真代码）。任何一条破了，
    # 输出就会有周期性增益纹波。
    dim_t_probe = 32
    dim_f_probe = 64  # 融合与频率无关，小谱跑得快
    err_a = _cola_error(hann_periodic(dim_t_probe), dim_t_probe // 2)
    err_b = _cola_error(_STFT_WINDOW_SQ, HOP)
    probe_frames = 200
    ones_spec = np.ones((2, dim_f_probe, probe_frames), dtype=np.complex64)
    try:
        ones_est = blend_estimate(
            OnesMaskModel(), ones_spec, 0, dim_f_probe, dim_t_probe, 1.0, 0, probe_frames, probe_frames
        )
        err_c = float(np.max(np.abs(ones_est.real - 1.0)))
    except VocalSepError as exc:
        err_c = float("inf")
        print(f"[自测] 融合常数和探测抛错：{exc}", file=sys.stderr)
    check(
        "② 重叠相加常数和",
        err_a < 1e-3 and err_b < 1e-3 and err_c < 1e-3,
        f"hann@2倍重叠偏差={err_a:.2e}、hann²@6倍重叠偏差={err_b:.2e}、"
        f"融合恒常输入偏差={err_c:.2e}（均要求 < 1e-3）",
    )

    # ③ 恒等假模型走完整切块/融合/分片路径后输出 ≈ 输入（含 dim_f 截断与
    # Nyquist 置 0 的损失）⇒ SNR > 40dB。两种输出形状分支都要过。
    y_id4 = separate_signal(x, IdentityModel(), DEFAULT_DIM_F, 32, 1.0)
    snr_id4 = _snr_db(x, y_id4)
    y_id2 = separate_signal(x, OnesMaskModel(), DEFAULT_DIM_F, 32, 1.0)
    snr_id2 = _snr_db(x, y_id2)
    check(
        "③ 恒等假模型全流程",
        snr_id4 > 40.0 and snr_id2 > 40.0,
        f"同形恒等 SNR={snr_id4:.1f}dB、全 1 掩码 SNR={snr_id2:.1f}dB（均要求 > 40dB）",
    )

    # ④ 分片执行 ≡ 整段执行：强制 8192 样本一片（≈0.19s，约 15 片）跑同一
    # 假模型，与整段一次跑的结果必须几乎逐位一致——这是「长音频分片不改变
    # 结果」的钉子。
    y_one = separate_signal(x, IdentityModel(), DEFAULT_DIM_F, 32, 1.0, slab_samples=10 ** 9)
    y_multi = separate_signal(x, IdentityModel(), DEFAULT_DIM_F, 32, 1.0, slab_samples=8192)
    snr_slab = _snr_db(y_one, y_multi)
    check("④ 分片执行与整段等价", snr_slab > 60.0, f"SNR={snr_slab:.1f}dB（要求 > 60dB）")

    # ⑤ 干/湿声方向判定：真机踩过「Inst 伴奏模型的输出被当人声写出」的坑
    # （纯正弦原样通过人声输出），四种「模型侧 × 目标侧」组合逐一钉死。
    expect = {
        ("UVR-MDX-NET-Inst_HQ_3.onnx", "vocals"): "residual",
        ("UVR-MDX-NET-Inst_HQ_3.onnx", "instrumental"): "direct",
        ("Kim_Vocal_2.onnx", "vocals"): "direct",
        ("kim_vocal_2.onnx", "instrumental"): "residual",
    }
    side_ok = all(
        resolve_output_side(model, stems) == want for (model, stems), want in expect.items()
    )
    check(
        "⑤ 干/湿声方向判定",
        side_ok,
        "四种「模型侧 × 目标侧」组合全部按「同侧 direct、异侧 residual」出向",
    )

    elapsed = round(time.time() - t0, 3)
    failed = [name for name, ok, _ in checks if not ok]
    if failed:
        emit_result({"status": "error", "error": f"自测未通过：{'、'.join(failed)}（详见 stderr）",
                     "self_test": True, "elapsed_sec": elapsed})
        return False
    emit_result({"status": "ok", "self_test": True, "elapsed_sec": elapsed})
    return True


# ───────────────────────── 主流程 ─────────────────────────

def emit_result(obj) -> None:
    """结果 JSON 写 stdout 末行（Rust 侧只解析末行；前面的行留给调试噪音）。"""
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def main() -> int:
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    if hasattr(sys.stderr, "reconfigure"):
        sys.stderr.reconfigure(encoding="utf-8")

    parser = argparse.ArgumentParser(description="Voice2Word Vocal Separation Runner (UVR MDX-Net)")
    parser.add_argument("--input", help="输入 WAV（pcm_s16le、44100Hz、立体声/mono）")
    parser.add_argument("--output", help="输出 WAV（44100Hz 立体声 pcm_s16le 人声）")
    parser.add_argument("--model", help="UVR MDX-Net 类 ONNX 模型路径")
    parser.add_argument("--provider", default="cpu", help="ONNX 执行后端: cpu | dml")
    parser.add_argument("--stems", default="vocals", help="要哪一侧: vocals（默认）| instrumental")
    parser.add_argument("--self-test", action="store_true", help="只跑 DSP 自检（不加载模型）")
    args = parser.parse_args()

    if args.self_test:
        return 0 if run_self_test() else 1

    missing = [name for name in ("input", "output", "model") if not getattr(args, name)]
    if missing:
        emit_result({"status": "error", "error": f"缺少必需参数: {', '.join('--' + m for m in missing)}"})
        return 1

    provider = resolve_provider(args.provider)
    started = time.time()
    try:
        print(f"[vocal_sep] 读取输入: {args.input}", file=sys.stderr)
        x = read_wav_stereo(args.input)
        input_sec = x.shape[-1] / float(SAMPLE_RATE)

        print(f"[vocal_sep] 加载模型: {args.model}（provider={provider}）", file=sys.stderr)
        model, dim_f, dim_t, actual_provider = create_onnx_model(args.model, provider)
        compensation = compensation_for(args.model)
        if compensation != 1.0:
            print(f"[vocal_sep] 幅度补偿 ×{compensation}", file=sys.stderr)

        print(f"[vocal_sep] 开始分离（输入 {input_sec:.1f}s，dim_f={dim_f}, dim_t={dim_t}）", file=sys.stderr)
        y_model = separate_signal(x, model, dim_f, dim_t, compensation)
        # 干/湿声方向：模型输出侧 ≠ 目标侧时取残差（见 resolve_output_side 文档）。
        side = resolve_output_side(args.model, args.stems)
        if side == "residual":
            print(f"[vocal_sep] 目标={args.stems}、模型输出为另一侧 → 取残差（原声 − 模型输出）", file=sys.stderr)
            y = x - y_model
        else:
            y = y_model
        print(f"[vocal_sep] 写出{args.stems}: {args.output}", file=sys.stderr)
        write_wav_stereo(args.output, y)
    except VocalSepError as exc:
        emit_result({"status": "error", "error": str(exc)})
        print(f"[vocal_sep][错误] {exc}", file=sys.stderr)
        return 1
    except Exception as exc:  # 兜底：任何未预期的失败都要走同一契约，不留裸 traceback 给 JSON 通道
        emit_result({"status": "error", "error": f"{type(exc).__name__}: {exc}"})
        raise

    emit_result({
        "status": "ok",
        "input_sec": round(input_sec, 3),
        "output": args.output,
        "stems": args.stems,
        "side": side,
        "provider": actual_provider,
        "elapsed_sec": round(time.time() - started, 3),
    })
    return 0


if __name__ == "__main__":
    sys.exit(main())
