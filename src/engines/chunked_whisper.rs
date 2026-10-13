//! Whisper 切块并行：6 分钟一块、1.5 秒重叠，多进程推理后按时间戳拼接。
//! 课程设计仍走本地 whisper-cli，不换模型栈。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

use super::ffmpeg::FFmpegEngine;
use super::{SenseVoiceEngine, WhisperEngine};
use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

const CHUNK_SEC: f64 = 360.0;
/// 相邻块之间显式共享的重叠时长（秒）。
///
/// 为什么必须有重叠：长音频被切成互相独立的块后，压在块交界上的整句会被两块各转
/// 一次（交界处重复出句），或者在两块里都只转出半句（交界处丢句）。让相邻块共享
/// 一段音频后，交界句必然完整落在至少一块里，拼接阶段再用模糊匹配把两次识别合并
/// 成一条即可（见 [`stitch_chunks`]）。
///
/// 为什么取 10s：段内单句通常 2~8s，10s 足以装下交界处 1~2 句，模糊匹配才拿得到
/// 完整上下文；旧值 1.5s 连一句都装不下，边界句始终以半截形式出现，匹配无从谈起。
/// 也不能更大：重叠区每块都要重新解码并转写，等于把算力白烧在重复音频上
/// （4 小时素材切 320 块时，10s 重叠约占总转写量的 1%）。硬上限见 [`plan_chunks`] 的夹取。
const CHUNK_OVERLAP_SEC: f64 = 10.0;
const MIN_CHUNK_SEC: f64 = 45.0;
/// `ceil` 前的容差：抵消 `total / (total / n)` 的 f64 舍入，见 [`plan_chunks_target`]。
/// 远大于该处舍入误差（约 1e-15 相对量级），又远小于任何真实块长，不会改变正常分块。
const CHUNK_COUNT_EPS: f64 = 1e-9;
/// 末段兜底延展的硬上限（秒）。见 [`stitch_chunks`] 末尾。
///
/// 依据：兜底本意是「补回最后一句被重叠判定误丢的收尾语音」，真实漏删只有几秒；
/// 项目内单条字幕的时长上限是 6s（`subtitle::segment::MAX_SEGMENT_DUR`），下游
/// `optimize_segments` 还会再拆。取 2 条标准字幕的长度（12s）足以覆盖真实漏删，
/// 又能在「末块整块被丢弃」时挡住把一条字幕拉长到几十秒 / 整块时长的错误行为。
/// （不取 `MIN_CHUNK_SEC`=45s：那等于允许单条字幕被拉到接近一个整块，正是要挡的场景。）
const TAIL_EXTEND_MAX_SEC: f64 = 12.0;
/// 单条音频允许切出的**最大块数**。
///
/// 块数是 `total / 块长` 的整数（见 [`plan_chunks_target`]），而每块都要落一个临时
/// WAV 并起一个转写进程。时长字段一旦损坏（`inf`、或 1e9 秒级的伪值），块数会冲到
/// `usize::MAX`：`(0..count).map(..).collect()` 按 `size_hint` 预分配 → 容量溢出
/// panic；即使侥幸没 panic，上亿个块也会把磁盘与内存吃干。
/// 4096 块 × 最短块长 45s ≈ 51 小时素材，远超任何真实输入（4 小时课程只有 320 块），
/// 因此这个上限只可能挡住损坏值，不会改变正常素材的分块。
const MAX_CHUNK_COUNT: usize = 4096;
const DEFAULT_WORKERS: usize = 2;
/// Whisper 多进程并行的 worker 上限。
///
/// 公开给性能页推导滑条量程：用户显式指定的并行进程数在这里被夹住，滑条就不能
/// 给出超出它的档位，否则「选 6 实际跑 4」与 VAD 阈值那类静默失效同型。
pub const MAX_WORKERS: usize = 4;
/// SenseVoice 多进程并行的 worker 上限。
/// 16 核机器实测：6 进程 × 2 线程（12 线程）最快（6 分钟样本 14.5s）；
/// 8 进程反而回落到 17.3s —— 并发模型加载的磁盘争用与内存带宽成为新瓶颈。
pub const MAX_SV_WORKERS: usize = 6;

/// 切块并行的最短音频门限（秒）：短于它整轨单进程转写，不做多进程切块。
///
/// `CHUNK_SEC + MIN_CHUNK_SEC` = 360 + 45 = 405s。这是「切块是否划算」的统一判据：
/// 切块路径内部（[`transcribe_chunked`]）与管线侧决定是否走 0 磁盘内存推流
/// （`can_stream`）都读这一个函数。此前管线里直接写死 `405.0`，与这里的常量是同一
/// 门限的两份写法，改动任一处都会让两条路径悄悄分叉——收敛到一处消除该风险。
/// 该值同时等价于 [`plan_chunks_whisper`] 从单块切到多块的边界。
pub fn chunk_threshold_secs() -> f64 {
    CHUNK_SEC + MIN_CHUNK_SEC
}

#[derive(Clone, Copy, Debug)]
pub struct AudioChunk {
    pub index: usize,
    pub start: f64,
    pub duration: f64,
}

/// Whisper 长音频切块的公开入口：360s 目标块长 + [`CHUNK_OVERLAP_SEC`] 重叠。
pub fn plan_chunks_whisper(total_duration: f64) -> Vec<AudioChunk> {
    plan_chunks_target(total_duration, CHUNK_SEC)
}

/// 纯函数：把 `[0, duration]` 按 `chunk_sec` 目标块长切成**带重叠**的块，返回每块
/// 的 `(start, duration)`（秒）。切块算术全部收敛在这里，与 ffmpeg / 并行调度无关，
/// 因此每种数值边界都能直接单测。
///
/// 三条硬边界，都是「坏输入不 panic、不死循环、不爆容量」：
/// - **非有限或非正时长**按 0 处理。`inf` 会让 `ceil` 得 `inf`、`as usize` 饱和成
///   `usize::MAX`，随后 `collect()` 按 `size_hint` 预分配直接容量溢出 panic；`NaN`
///   则把每块时间戳污染成 `NaN`，原样送进 ffmpeg 的切片参数。
/// - **块数**夹在 `[2, MAX_CHUNK_COUNT]`：时长字段损坏（1e9 秒级伪值 / `f64::MAX`）
///   时否则会切出上千万块，把临时目录与内存吃干。
/// - **重叠**夹到 `[0, chunk_sec/2]`：负值 / `NaN` 当 0；超过半个块长时相邻块的
///   `start` 会倒退（后一块的起点落在前一块之前），块序与时间轴同时错乱——夹到半块
///   即保证重叠恒小于块长、每块 `start` 单调不减。
///
/// 时长不到「一个块长 + [`MIN_CHUNK_SEC`]」时返回单块 `(0.0, duration.max(0.1))`：
/// 即使 `duration` 为 0 也给一个最小正时长，避免下游拿到 0 长度切片。
pub fn plan_chunks(duration: f64, chunk_sec: f64, overlap_sec: f64) -> Vec<(f64, f64)> {
    // 非有限 / 负时长统一按 0：NaN 会把整条时间轴污染成 NaN，inf 会让块数爆容量。
    let total = if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        0.0
    };
    // 目标块长同样要过「非有限」这关，并保证不小于 `MIN_CHUNK_SEC`。
    let chunk = if chunk_sec.is_finite() {
        chunk_sec.max(MIN_CHUNK_SEC)
    } else {
        MIN_CHUNK_SEC
    };
    // 半块长是重叠的硬上限：见函数文档。
    let overlap = if overlap_sec.is_finite() {
        overlap_sec.clamp(0.0, chunk * 0.5)
    } else {
        0.0
    };
    if total <= chunk + MIN_CHUNK_SEC {
        return vec![(0.0, total.max(0.1))];
    }
    // 块数取 `total / 块长` 的向上取整，但先减 `CHUNK_COUNT_EPS` 抵消 f64 舍入：
    // 调用方（`transcribe_chunked_sensevoice`）常以 `total/chunk_count` 作块长，数学上
    // `total / 块长` 恰为该整数，浮动除法却会把它舍入到略大一点
    // （如 `4324.0 / (4324.0 / 14.0) == 14.000000000000002`），直接 `ceil` 就凭空多切
    // 一块，末轮只剩一个 worker 在跑、其余空转——正是调用方要避免的场景。
    let chunk_count = (total / chunk - CHUNK_COUNT_EPS)
        .ceil()
        .max(2.0)
        .min(MAX_CHUNK_COUNT as f64) as usize;
    // 按块数均分，而不是固定步长：固定步长会在片尾留一个短块，末轮让一个 worker 独自
    // 拖尾、其余空转。均分后各块长度只差半个重叠。
    let nominal = total / chunk_count as f64;
    let half_overlap = overlap * 0.5;
    (0..chunk_count)
        .map(|index| {
            let boundary_start = index as f64 * nominal;
            let boundary_end = ((index + 1) as f64 * nominal).min(total);
            // 首块不从负数起、末块一定收在 `total`：两端各让出半个重叠。
            let start = if index == 0 {
                0.0
            } else {
                (boundary_start - half_overlap).max(0.0)
            };
            let end = if index + 1 == chunk_count {
                total
            } else {
                (boundary_end + half_overlap).min(total)
            };
            (start, (end - start).max(0.1))
        })
        .collect()
}

/// 按指定目标块长切块（Whisper 用 360s；SenseVoice 进程并行用更短的块以获得
/// 更好的负载均衡——块数应为 worker 数的整数倍附近，避免最后一轮只有少数
/// worker 在跑、其余空转）。切块算术与重叠夹取见 [`plan_chunks`]。
pub fn plan_chunks_target(total_duration: f64, target_sec: f64) -> Vec<AudioChunk> {
    plan_chunks(total_duration, target_sec, CHUNK_OVERLAP_SEC)
        .into_iter()
        .enumerate()
        .map(|(index, (start, duration))| AudioChunk {
            index,
            start,
            duration,
        })
        .collect()
}

/// SenseVoice 多进程并行的 worker 数：每 2 个核心一个进程，上限 6。
/// 抽成独立函数以便管线展示与基准测试复用同一份策略。
pub fn sensevoice_worker_count(cores: u32) -> usize {
    (cores / 2).clamp(2, MAX_SV_WORKERS as u32) as usize
}

/// SenseVoice 并行切块的块数推导：worker 数的整数倍，且保证单块不超过 [`CHUNK_SEC`]。
///
/// 时长非有限（`ffprobe` 时长字段损坏）时直接返回 `workers`、**不进循环**：
/// `inf / 块数` 恒为 `inf`，`while` 永不终止（真死循环，连超时都等不到）。
/// 随后 [`plan_chunks_target`] 对非有限时长只给出单块，调用方据此落回整段路径。
fn sensevoice_chunk_count(total_duration: f64, workers: usize) -> usize {
    let workers = workers.max(1);
    if !total_duration.is_finite() {
        return workers;
    }
    let mut count = workers;
    while total_duration / count as f64 > CHUNK_SEC {
        count += workers;
    }
    count
}

pub fn worker_count(chunk_n: usize, requested_threads: u32, use_gpu: bool) -> usize {
    if chunk_n <= 1 {
        return 1;
    }
    // 核显/单 GPU 上两个 whisper-cli 会抢同一块显存，识别直接变糊。
    if use_gpu {
        return 1;
    }
    // 每个 whisper-cli 进程至少给 8 个线程，避免 16 线程机器被拆成
    // 4 个 4 线程进程后互相争抢内存带宽。超过 16 线程再增加进程数。
    let by_cpu = (requested_threads.max(8) / 8) as usize;
    by_cpu
        .clamp(2, MAX_WORKERS)
        .min(chunk_n)
        .max(DEFAULT_WORKERS.min(chunk_n))
}

/// 把用户设定的「转写线程数」摊到各并行进程：每进程 `threads / workers`，最少 1。
///
/// 此前这里对商做 `.max(4)`：用户设 4 线程、切块路径按核数起 2 个进程时，每进程被
/// 抬到 4 线程 → 实跑 8 线程，设置「静默翻倍」且无从察觉。进程数由核数推导、与线程
/// 设置无关，硬抬下限只会让低线程设置失效。商 ≥ 4 时（线程 ≥ 8 且进程数不多，即默认
/// 与常规档）结果与修复前逐档一致，只影响 2~7 线程的低配设置与显式多进程覆盖。
fn per_process_threads(threads: u32, workers: usize) -> u32 {
    (threads / workers.max(1) as u32).max(1)
}

/// 拼接各块的片段表：跨块重叠区的同一句话合并成一条，块内的重复句原样保留。
///
/// 切块时相邻块共享 [`CHUNK_OVERLAP_SEC`] 秒音频，于是压在交界上的整句会被两块各转
/// 一次。这里对**跨块**且落在重叠窗口内的一对做模糊匹配（[`merge_overlap`]）：判为
/// 同一句就合并成一条（保留更早的起点、取较晚的终点，避免合并后短了一截），判为
/// 不同就都保留。**只对跨块的一对生效**——块内不存在重叠来源，块内时间上并不重叠的
/// 重复句是说话人真的重复了一遍（跟读 / 听写类课程极常见），无差别套用会把那句静默
/// 吃掉。块内的真重叠（时间上互相压着）仍按更严格的包含式判定丢弃。
pub fn stitch_chunks(mut parts: Vec<(usize, f64, Vec<Segment>)>) -> Vec<Segment> {
    parts.sort_by_key(|(idx, _, _)| *idx);
    // 所有块的最大结束时间（按各自偏移还原到全局时间轴）。用于兜住「末块整体落在
    // 已接受字幕之后」这一漏删场景——见下方对末条接受片段的处理。
    let global_end = parts
        .iter()
        .map(|(_, offset, segs)| segs.iter().map(|s| s.end + offset).fold(0.0f64, f64::max))
        .fold(0.0f64, f64::max);
    let mut out: Vec<Segment> = Vec::new();
    // 已接受片段所属的块下标：重叠只存在于相邻两块之间，合并也只该对**跨块**的一对
    // 生效（见下方分支）。
    let mut last_chunk: Option<usize> = None;
    for (chunk_idx, offset, segs) in parts {
        for mut seg in segs {
            seg.start += offset;
            seg.end += offset;
            if let Some(prev) = out.last_mut() {
                let cross_chunk = last_chunk != Some(chunk_idx);
                let gap = seg.start - prev.end;
                // 跨块 + 落在重叠窗口内：模糊匹配把「同一句话的两次识别」合并成一条。
                // 命中即吃掉当前这条（保留更早的 start、取较晚的 end），不新增片段。
                // 窗口上加 0.4s 容差，吸收两侧时间戳的抖动。
                if cross_chunk && gap < CHUNK_OVERLAP_SEC + MERGE_GAP_TOLERANCE {
                    if let MergeDecision::Merged(m) =
                        merge_overlap(prev, &seg, OVERLAP_MATCH_THRESHOLD)
                    {
                        *prev = m;
                        continue;
                    }
                }
                // 块内的真重叠或已越过重叠窗口的跨块重叠：仍按更严格的包含式判定丢弃，
                // 避免把只是「像」的两句话连成一条。块内时间上不重叠的重复句不在此列，
                // 原样保留。
                if gap < -0.25 {
                    if text_similar(prev.display_text(), seg.display_text())
                        || seg.end <= prev.end + 0.15
                    {
                        continue;
                    }
                    if seg.start < prev.end {
                        seg.start = prev.end;
                    }
                    if seg.end - seg.start < 0.12 {
                        continue;
                    }
                }
            }
            out.push(seg);
            last_chunk = Some(chunk_idx);
        }
    }
    // 末块整体被上面的重叠判定吞掉时的兜底：若已接受字幕整体提前于全部块的真实
    // 结束时间收尾，说明末尾那段语音被判成了「重叠重复」而丢弃，而它其实是片尾
    // 最后一句——直接丢掉会让字幕凭空少一截。末条正是被压得最长的那一句
    // （相邻重叠触发 `seg.end <= prev.end + 0.15` 而 continue 时，它的 end 至少
    // 比真实结尾短 0.15s），因此把它的终点拉回到全局结尾即可，不新增片段。
    //
    // 但延展必须有上限：`global_end` 是「全部块的最大结束时间」，若整个末块都被
    // 判成重复而丢弃，`global_end - last.end` 会是整块的跨度（Whisper 目标块长
    // 360s），把一条字幕无上限地拉长几十秒到几分钟。真实漏删只是末尾一两句
    // （见 `TAIL_EXTEND_MAX_SEC`），超过上限就说明不是「漏了最后一句」而是末块
    // 整体失效，此时保留原 `end` 更诚实（不伪造时间轴），交给下游/人工处理。
    if let Some(last) = out.last_mut() {
        let extension = global_end - last.end;
        if extension > 0.05 && extension <= TAIL_EXTEND_MAX_SEC {
            last.end = global_end;
        }
    }
    for (i, seg) in out.iter_mut().enumerate() {
        seg.index = i + 1;
    }
    out
}

fn text_similar(a: &str, b: &str) -> bool {
    let a = a.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let b = b.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    long.contains(short.as_str()) && short.chars().count() >= 4
}

/// 归一化文本后按「最长公共子序列占比」算相似度（0.0~1.0）。
///
/// 归一化（[`normalize_for_match`]）：去掉标点与空白、统一小写，只留字母 / 数字 /
/// 汉字。同一句话被相邻两块各转一次时，两次差别基本集中在标点、空格、大小写与个别
/// 断句字符上，归一化后即可正面比较。
///
/// 为什么用 LCS 占比而不是字符集 Dice：这里的差异主要是「多一个字 / 少一个字」「半句
/// 变整句」，LCS 对这类**保序**的增删最敏感；字符集 Dice 不看顺序，`甲乙丙` 与 `丙乙甲`
/// 会被判成完全相似，而字幕里顺序恰恰有意义（同音误听后的重排常出现在边界上）。
/// 片段文本只有几十个字符，O(n·m) 的 DP 开销可以忽略。
///
/// 归一化后任一侧为空（空串 / 纯标点 / 纯空白）一律返回 0.0：空文本不构成「同一句
/// 话」的证据，宁可两条都留。
pub fn text_similarity(a: &str, b: &str) -> f64 {
    let a = normalize_for_match(a);
    let b = normalize_for_match(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let lcs = lcs_len(&a, &b);
    (2.0 * lcs as f64) / (a.len() + b.len()) as f64
}

/// 归一化：转小写、只留字母 / 数字 / 汉字（去掉标点与空白）。
fn normalize_for_match(s: &str) -> Vec<char> {
    s.chars()
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// 两个字符序列的最长公共子序列长度（滚动数组，额外空间 O(min(n, m))）。
fn lcs_len(a: &[char], b: &[char]) -> usize {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut prev = vec![0usize; short.len() + 1];
    let mut cur = vec![0usize; short.len() + 1];
    for &lc in long {
        for (j, &sc) in short.iter().enumerate() {
            cur[j + 1] = if sc == lc {
                prev[j] + 1
            } else {
                cur[j].max(prev[j + 1])
            };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[short.len()]
}

/// 跨块重叠区里一对字幕被判为「同一句话」的相似度阈值。
///
/// 同一句话被相邻两块各转一次时，归一化后的 LCS 占比通常 ≥ 0.9（差异只在标点与个别
/// 断句字符）。取 0.7 留出余量：边界上常见的「半句 vs 整句」「丢一两个字」仍能命中；
/// 低于它多半是两块各自识别出的不同内容，不该并成一条。阈值只作用于**跨块**的一对
/// （见 [`stitch_chunks`]），块内重复句不受影响。
pub const OVERLAP_MATCH_THRESHOLD: f64 = 0.7;

/// 跨块合并的时间窗口容差（秒）：重叠本身是 [`CHUNK_OVERLAP_SEC`]，再加这点容差吸收
/// 两侧时间戳抖动。只有落在窗口内的一对才考虑合并——窗口外即便文本偶然相似，也是块后
/// 的新内容。
const MERGE_GAP_TOLERANCE: f64 = 0.4;

/// 相邻块边界处一对候选字幕的合并决策。
#[derive(Debug, Clone, PartialEq)]
pub enum MergeDecision {
    /// 两条是同一句话的两次识别：合并成一条——起点取更早者、终点取较晚者（避免合并后
    /// 短了一截），文本等其余字段沿用更早的那条。
    Merged(Segment),
    /// 两条不是同一句话：都保留（宁可重复，也不丢句子）。
    KeepBoth,
}

/// 纯函数：判定跨块边界的「上一块尾部」与「下一块头部」是否为同一句话，并给出合并
/// 结果。相似度取 [`text_similarity`]，达到 `threshold` 即判命中（`>=`）。
///
/// 只做文本层面的判断，不含时间窗口 / 块下标等编排逻辑——那些留在 [`stitch_chunks`]，
/// 这样「什么时候该合并」这条核心规则可以脱离拼接流程单独测。
pub fn merge_overlap(prev_tail: &Segment, next_head: &Segment, threshold: f64) -> MergeDecision {
    let similarity = text_similarity(prev_tail.display_text(), next_head.display_text());
    if similarity < threshold {
        return MergeDecision::KeepBoth;
    }
    // 文本判为同一句：以时间更早的那条为基准（保留它的文本与字段，时间轴上也更靠前），
    // 终点取两条里较晚的，避免合并后比实际语音短一截。
    let (earlier, later) = if prev_tail.start <= next_head.start {
        (prev_tail, next_head)
    } else {
        (next_head, prev_tail)
    };
    let mut merged = earlier.clone();
    merged.end = earlier.end.max(later.end);
    MergeDecision::Merged(merged)
}

/// 单块转写回调：传入（切片 WAV 路径, 该块时长秒, 建议线程数），
/// 返回块内相对时间戳的片段与（可选的）该块 VAD 耗时。
pub type ChunkTranscriber<'a> =
    dyn Fn(&Path, f64, u32) -> Result<(Vec<Segment>, f64)> + Send + Sync + 'a;

/// 单块转写结果：(块下标, 块起始秒, 块内片段, 该块 VAD 耗时秒)。
type ChunkResult = (usize, f64, Vec<Segment>, f64);

/// 通用切块并行执行器：切片 → 多 worker 并行转写 → 去重叠拼接。
///
/// 与具体引擎解耦，Whisper 与 SenseVoice 共用同一套切块 / 拼接 / 进度逻辑，
/// 保证两者的重叠去重与时间轴还原行为完全一致。
#[allow(clippy::too_many_arguments)]
pub fn run_chunked_parallel(
    ffmpeg: &FFmpegEngine,
    wav_path: &Path,
    chunks: Vec<AudioChunk>,
    workers: usize,
    per_proc_threads: u32,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
    transcribe_one: &ChunkTranscriber<'_>,
) -> Result<(Vec<Segment>, f64)> {
    let workers = workers.max(1);
    info!(
        chunks = chunks.len(),
        workers, per_proc_threads, "切块并行转写启动"
    );

    let work_dir_path = std::env::temp_dir().join(format!(
        "v2w_chunks_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    // 切块目录用守卫持有：切片阶段任何一个 `slice_wav(...)?` 失败都会直接返回，
    // 而删除代码写在函数末尾，于是整个目录（每块 30~60s 的 16kHz WAV，可达数百 MB）
    // 就永久留在 TEMP 里。守卫让它在任意出口（含切片失败、worker panic）都被递归删掉。
    let mut work_dir_guard = TempPathGuard::dir(&work_dir_path);
    let work_dir = work_dir_path.as_path();
    std::fs::create_dir_all(work_dir)?;

    let mut slice_paths: Vec<(AudioChunk, PathBuf)> = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        let out = work_dir.join(format!("c{}.wav", chunk.index));
        // 带上块号与起止时间：切片失败（源 WAV 损坏 / 时长与实测不符）时，光看
        // ffmpeg 的原始报错分不清是哪一块，也没法对照是「切到片尾之外」还是别的。
        ffmpeg
            .slice_wav(wav_path, chunk.start, chunk.duration, out.as_path())
            .with_context(|| {
                format!(
                    "切分第 {} 块失败 (start={:.2}s, duration={:.2}s)",
                    chunk.index, chunk.start, chunk.duration
                )
            })?;
        slice_paths.push((*chunk, out));
    }

    let progress_cb = progress_cb.map(Arc::new);
    let done = Arc::new(AtomicUsize::new(0));
    let total_chunks = slice_paths.len();

    let pool = rayon_pool(workers)?;
    let results: Vec<Result<ChunkResult>> = pool.install(|| {
        use rayon::prelude::*;
        slice_paths
            .par_iter()
            .map(|(chunk, path)| {
                let cb = progress_cb.clone();
                let done = done.clone();
                let (segs, chunk_vad_sec) = transcribe_one(path, chunk.duration, per_proc_threads)?;
                let finished = done.fetch_add(1, Ordering::SeqCst) + 1;
                let ratio = finished as f64 / total_chunks as f64;
                if let Some(cb) = cb.as_ref() {
                    for mut seg in segs.iter().cloned() {
                        seg.start += chunk.start;
                        seg.end += chunk.start;
                        cb(
                            ratio,
                            &format!(
                                "块 {}/{} 完成 · {}s",
                                finished, total_chunks, seg.end as u32
                            ),
                            Some(seg),
                        );
                    }
                    if segs.is_empty() {
                        cb(
                            ratio,
                            &format!("块 {}/{} 无语音", finished, total_chunks),
                            None,
                        );
                    }
                }
                Ok((chunk.index, chunk.start, segs, chunk_vad_sec))
            })
            .collect()
    });

    let mut parts = Vec::new();
    let mut total_vad_sec = 0.0;
    for item in results {
        match item {
            Ok((idx, start, segs, vad_sec)) => {
                parts.push((idx, start, segs));
                total_vad_sec += vad_sec;
            }
            Err(err) => {
                warn!(error = %err, "切块转写失败");
                // 失败即清理：守卫在 return 时也会兜底，这里提前删是为了尽快释放磁盘
                work_dir_guard.remove_now();
                return Err(err);
            }
        }
    }

    // 全部切片已转写完，立刻删掉整个切块目录释放磁盘
    work_dir_guard.remove_now();
    let mut merged = stitch_chunks(parts);
    // 与整段路径（`whisper.transcribe_with_model` / `sensevoice.transcribe` 末尾）
    // 同一个收尾：切块路径此前跳过了 `optimize_segments`，于是「并行」与「整段」
    // 两种模式产出的字幕粒度不一致——并行模式下跨块拼起来的超长句不会被拆到
    // 6 秒内、短句也不平滑，同一台机器换个开关就换一套字幕形态。这里补齐，
    // 让两条路径的最终字幕同形（该函数幂等，管线若再调一次结果不变）。
    crate::subtitle::optimize_segments(&mut merged);
    info!(segments = merged.len(), total_vad_sec, "切块字幕已拼接");
    if let Some(cb) = progress_cb {
        cb(1.0, &format!("转写完成，共 {} 个片段", merged.len()), None);
    }
    Ok((merged, total_vad_sec))
}

/// SenseVoice 长音频多进程切块并行。
///
/// 实测（16 核 AMD、6 分钟样本）：单进程 8 线程 32.6s、16 线程反而 41.6s；而
/// 6 进程 × 2 线程仅需 14.5s。原因是 sherpa-onnx 单会话的 ONNX intra-op 线程
/// 扩展性很差（线程越多越慢），进程级并行才能真正吃满多核。因此长音频改为
/// 按块切分、每块独立起一个 runner 进程，再按时间戳拼接。
// 参数是「引擎 + 输入 + 线程/并行提示 + 进度回调」的完整既有签名，调用点分布在
// `core/pipeline.rs` 与集成测试；拆成参数结构体属于超出本轮范围的接口改动。
#[allow(clippy::too_many_arguments)]
pub fn transcribe_chunked_sensevoice(
    ffmpeg: &FFmpegEngine,
    sensevoice: &SenseVoiceEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads_hint: u32,
    // 用户在设置页显式指定的并行进程数；None = 按核数自动推导
    workers_override: Option<usize>,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
) -> Result<(Vec<Segment>, f64)> {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4) as u32;
    // 每 2 个核心一个 worker，上限 6 个进程（实测 16 核下 6 进程 × 2 线程最优）；
    // 用户在设置页显式指定时以其为准，但不超过实测上限。
    let workers = match workers_override {
        Some(w) if w >= 1 => w.clamp(1, MAX_SV_WORKERS),
        _ => sensevoice_worker_count(cores),
    };
    // 单会话线程数固定 2：实测每进程 2 线程的吞吐优于 4 线程（线程越多单会话越慢），
    // 多进程才是提升方向。总占用 = workers × 2，16 核下 12 线程，留余量给 UI 与系统。
    let per_proc_threads = 2u32;

    // 单进程时切块只会反复冷启动加载模型，比整段识别更慢，直接走整段路径
    if workers <= 1 {
        return sensevoice.transcribe(
            wav_path,
            language,
            Some(threads_hint.clamp(
                super::sensevoice::MIN_THREADS,
                super::sensevoice::MAX_THREADS,
            )),
            Some(total_duration),
            progress_cb,
        );
    }

    // 块数取 worker 数的整数倍，保证每一轮都满载（否则最后一轮只有少数 worker
    // 在跑、其余空转）；同时保证单块不超过 360s，避免单块过长拖尾。
    let chunk_count = sensevoice_chunk_count(total_duration, workers);
    let target = total_duration / chunk_count as f64;
    let chunks = plan_chunks_target(total_duration, target);

    if chunks.len() <= 1 {
        // 音频太短：单进程更划算（省去多份模型加载），线程数按实测饱和点收敛
        return sensevoice.transcribe(
            wav_path,
            language,
            Some(threads_hint.clamp(
                super::sensevoice::MIN_THREADS,
                super::sensevoice::MAX_THREADS,
            )),
            Some(total_duration),
            progress_cb,
        );
    }

    let lang = language.map(|s| s.to_string());
    let transcribe_one = |path: &Path, dur: f64, th: u32| -> Result<(Vec<Segment>, f64)> {
        sensevoice.transcribe(path, lang.as_deref(), Some(th), Some(dur), None)
    };
    // 实际块数可能少于 worker 数：`plan_chunks_target` 在多数情况下会按 target 重算
    // 块数（target 恰好等于 total/chunk_count 时通常保持一致，但短音频被 `MIN_CHUNK_SEC`
    // 兜成单块、或 target 被下限抬高时块数会变少）。多出来的线程只会空转，且进度
    // 分母取的是实际块数，因此这里收敛到实际块数。
    let workers = workers.min(chunks.len());
    // 收尾的 `optimize_segments` 已在 `run_chunked_parallel` 内统一执行
    run_chunked_parallel(
        ffmpeg,
        wav_path,
        chunks,
        workers,
        per_proc_threads,
        progress_cb,
        &transcribe_one,
    )
}

// 同上：参数逐层透传到 `run_chunked_parallel`，就地 allow 而不改公开签名。
#[allow(clippy::too_many_arguments)]
pub fn transcribe_chunked(
    ffmpeg: &FFmpegEngine,
    whisper: &WhisperEngine,
    wav_path: &Path,
    total_duration: f64,
    language: Option<&str>,
    threads: Option<u32>,
    model_override: Option<&Path>,
    use_gpu: bool,
    // 用户在设置页显式指定的并行进程数；None = 按核数自动推导
    workers_override: Option<usize>,
    progress_cb: Option<crate::engines::SegmentProgressCb>,
) -> Result<(Vec<Segment>, f64)> {
    // GPU 单进程切块只会反复加载模型并丢掉跨块上下文，又慢又不准。
    if use_gpu || total_duration <= chunk_threshold_secs() {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }

    let chunks = plan_chunks_whisper(total_duration);
    if chunks.len() <= 1 {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }

    // 显式值同样要过 `MAX_WORKERS`：这个上限的存在理由是「每个 whisper-cli 至少
    // 8 线程，进程过多会互抢内存带宽」，与用户怎么选无关。此前只夹到 `chunks.len()`，
    // 而 SenseVoice 路径已正确夹到 `MAX_SV_WORKERS`——于是 16 核机器上滑条能给到
    // 6，Whisper 就真的起 6 个进程，把自动推导路径永远不会触到的上限冲破。
    let workers = match workers_override {
        Some(w) if w >= 1 => w.clamp(1, MAX_WORKERS).min(chunks.len()),
        _ => worker_count(chunks.len(), threads.unwrap_or(8), use_gpu),
    };
    // 单进程时切块只会反复冷启动加载模型，比整段识别更慢，直接走整段路径
    if workers <= 1 {
        return whisper.transcribe_with_model(
            wav_path,
            language,
            threads,
            Some(total_duration),
            model_override,
            progress_cb,
        );
    }
    let per_proc_threads = if use_gpu {
        threads.unwrap_or(8).max(4)
    } else {
        per_process_threads(threads.unwrap_or(8), workers)
    };
    let lang = language.map(|s| s.to_string());
    let model = model_override.map(|p| p.to_path_buf());
    let transcribe_one = |path: &Path, dur: f64, th: u32| -> Result<(Vec<Segment>, f64)> {
        whisper.transcribe_with_model_single_processor(
            path,
            lang.as_deref(),
            Some(th),
            Some(dur),
            model.as_deref(),
            None,
        )
    };
    let (merged, total_vad_sec) = run_chunked_parallel(
        ffmpeg,
        wav_path,
        chunks,
        workers,
        per_proc_threads,
        progress_cb,
        &transcribe_one,
    )?;
    // 收尾的 `optimize_segments` 已在 `run_chunked_parallel` 内统一执行
    Ok((merged, total_vad_sec))
}

fn rayon_pool(workers: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("v2w-whisper-{i}"))
        .build()
        .with_context(|| format!("创建 Whisper 切块线程池失败 (workers={workers})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_audio_is_single_chunk() {
        let c = plan_chunks_whisper(120.0);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].start, 0.0);
    }

    /// 门限是「单块 vs 多块」的统一判据：等于门限时仍是单块，略过即切成多块。
    /// 管线侧 `can_stream` 与切块路径都读 `chunk_threshold_secs()`，此处锁住它与
    /// `plan_chunks_whisper` 的边界一致，防止两处再次各写一个数字后悄悄分叉。
    #[test]
    fn chunk_threshold_matches_plan_chunks_boundary() {
        assert_eq!(chunk_threshold_secs(), 405.0);
        assert_eq!(plan_chunks_whisper(chunk_threshold_secs()).len(), 1);
        assert!(plan_chunks_whisper(chunk_threshold_secs() + 1.0).len() >= 2);
    }

    #[test]
    fn long_audio_is_chunked_with_overlap() {
        let c = plan_chunks_whisper(32.0 * 60.0);
        assert!(c.len() >= 5, "32min -> {:?}", c.len());
        assert!(c[1].start < 32.0 * 60.0 / c.len() as f64);
        let longest = c.iter().map(|x| x.duration).fold(0.0, f64::max);
        let shortest = c.iter().map(|x| x.duration).fold(f64::INFINITY, f64::min);
        // 首尾块各只向一侧让出半个重叠，中间块两侧各让半个：块长的极差恰为半个重叠。
        assert!(
            longest - shortest < CHUNK_OVERLAP_SEC * 0.5 + 0.1,
            "unbalanced chunks: {:?}",
            c
        );
        let last = c.last().unwrap();
        assert!((last.start + last.duration - 32.0 * 60.0).abs() < 0.05);
    }

    #[test]
    fn stitch_drops_overlap_duplicate() {
        let a = vec![Segment::new(1, 350.0, 358.0, "切比雪夫不等式")];
        let b = vec![Segment::new(1, 0.2, 8.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, a), (1, 358.5, b)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "切比雪夫不等式");
    }

    /// 末块唯一一条句子因「与上一块尾部重复」被判丢时，音频的真实结尾不能一起丢：
    /// 末条已接受片段的终点要拉回到全部块的真实结束时间（不新增片段）。
    #[test]
    fn stitch_recovers_media_end_when_tail_duplicate_is_dropped() {
        let head = vec![Segment::new(1, 250.0, 300.0, "切比雪夫不等式")];
        // 末块（偏移 300s）只剩这一句，文本与上一块尾部相同：按重叠去重规则会被丢弃
        let tail = vec![Segment::new(1, 0.1, 8.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 300.0, tail)]);
        assert_eq!(out.len(), 1, "重复句仍应被丢掉: {out:?}");
        assert_eq!(out[0].text, "切比雪夫不等式");
        assert!(
            (out[0].end - 308.0).abs() < 1e-9,
            "末条应延展到音频真实结尾，而不是停在 300s: {out:?}"
        );
    }

    /// 上限内的正常兜底：末尾被误丢的一句只有几秒时仍应补回真实结尾。
    #[test]
    fn stitch_tail_recovery_within_cap_extends() {
        let head = vec![Segment::new(1, 250.0, 300.0, "切比雪夫不等式")];
        let tail = vec![Segment::new(1, 0.1, 4.0, "切比雪夫不等式")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 300.0, tail)]);
        assert_eq!(out.len(), 1);
        assert!(
            (out[0].end - 304.0).abs() < 1e-9,
            "上限内的漏删仍应补回结尾: {out:?}"
        );
    }

    /// 末段兜底延展必须封顶：`global_end` 是「全部块的最大结束时间」，被丢弃的末句若
    /// 拖得很长，`global_end - last.end` 会远大于真实漏删，旧实现会把一条字幕无上限地
    /// 拉长几十秒。这里用块内真重叠（时间上互相压着）构造丢弃场景：末句一路拖到 90s，
    /// 不封顶时 `out[0].end` 会被拉到 90s，须保留原 `end`。
    #[test]
    fn stitch_tail_recovery_is_capped() {
        let segs = vec![
            Segment::new(1, 0.0, 30.0, "切比雪夫不等式"),
            // 与上一句时间上互相压着且文本相同：按块内重叠去重丢弃；终点却拖到 90s。
            Segment::new(2, 29.0, 90.0, "切比雪夫不等式"),
        ];
        let out = stitch_chunks(vec![(0, 0.0, segs)]);
        assert_eq!(out.len(), 1, "块内重叠重复句应被丢弃: {out:?}");
        assert!(
            (out[0].end - 30.0).abs() < 1e-9,
            "延展超限时必须保留原 end，而不是把单条字幕拉长: {out:?}"
        );
    }

    /// 反向断言：末块是不同的新句、整体落在上一块之后时必须原样保留，
    /// 不能被去重规则误删、也不该被上一条兜底逻辑吞掉。
    #[test]
    fn stitch_keeps_distinct_tail_segment() {
        let head = vec![Segment::new(1, 300.0, 358.5, "前面这半句话")];
        let tail = vec![Segment::new(1, 0.1, 4.0, "后面这半句是新的内容")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 358.5, tail)]);
        assert_eq!(out.len(), 2, "不同文本的新句不得被去重规则误删: {out:?}");
        assert!(out[1].start > out[0].end, "新句应落在上一句之后: {out:?}");
    }

    /// 回归：用户显式指定的并行进程数不得冲破 `MAX_WORKERS`。
    ///
    /// 此前 `transcribe_chunked` 里的显式分支只夹到 `chunks.len()`，漏了
    /// `MAX_WORKERS`；SenseVoice 分支（`transcribe_chunked_sensevoice`）却正确夹了
    /// `MAX_SV_WORKERS`。于是 16 核机器上滑条能给到 6，Whisper 就真起 6 个
    /// whisper-cli——而自动推导路径永远算不出超过 4 的值。上限的存在理由（每进程
    /// 至少 8 线程、进程过多互抢内存带宽）与用户怎么选无关，显式值同样要过。
    #[test]
    fn explicit_worker_override_respects_max_workers() {
        // 与 `transcribe_chunked` 中显式分支完全同一行表达式。
        let resolve = |w: usize, chunk_n: usize| -> usize {
            match Some(w) {
                Some(w) if w >= 1 => w.clamp(1, MAX_WORKERS).min(chunk_n),
                _ => worker_count(chunk_n, 8, false),
            }
        };
        // 块数足够多时，任何显式值都不得超过上限
        for w in 1..=16 {
            assert!(
                resolve(w, 32) <= MAX_WORKERS,
                "显式指定 {w} 个进程越过了上限 {MAX_WORKERS}"
            );
        }
        assert_eq!(resolve(6, 32), MAX_WORKERS, "6 应被夹到上限");
        assert_eq!(resolve(16, 32), MAX_WORKERS);
        // 块数更少时仍以块数为准，不凭空造出多于块数的进程
        assert_eq!(resolve(6, 3), 3);
        assert_eq!(resolve(1, 8), 1, "显式 1 应保留（走整段路径）");
        // 自动推导路径也永远不超过上限
        for threads in [2u32, 8, 16, 32, 64] {
            assert!(worker_count(64, threads, false) <= MAX_WORKERS);
        }
    }

    /// 回归：切块并行把「转写线程数」摊到各进程时，用户设的低线程数不该被硬抬
    /// 下限放大。旧实现每进程 `.max(4)`：设 4 线程 + 2 进程 → 实跑 8 线程，
    /// 设置被静默翻倍且无从察觉。
    #[test]
    fn per_process_threads_does_not_inflate_low_thread_settings() {
        // 修复前的旧值（`.max(4)`）作对照：以下正是被放大的档位。
        assert_eq!(
            per_process_threads(4, 2),
            2,
            "4 线程分 2 进程应为 2，而非旧值 4"
        );
        assert_eq!(
            per_process_threads(2, 2),
            1,
            "2 线程分 2 进程应为 1，而非旧值 4"
        );
        assert_eq!(
            per_process_threads(8, 4),
            2,
            "8 线程分 4 进程应为 2，而非旧值 4"
        );
        // 商 >= 4 的常规档与修复前逐档一致，证明无回归。
        assert_eq!(per_process_threads(8, 2), 4);
        assert_eq!(per_process_threads(16, 4), 4);
        // 不变量：进程数 x 每进程线程数不再系统性超过用户设定（每进程至少 1 线程）。
        for threads in 1u32..=64 {
            for workers in 1usize..=MAX_WORKERS {
                let per = per_process_threads(threads, workers);
                assert!(per >= 1, "每进程至少 1 线程");
                assert!(
                    per * workers as u32 <= threads.max(workers as u32),
                    "线程被放大：{threads} 线程 x {workers} 进程 -> 每进程 {per}"
                );
            }
        }
        // workers = 0 不得 panic（`.max(1)` 兜底），也不凭空放大。
        assert_eq!(per_process_threads(8, 0), 8);
    }

    /// 回归：SenseVoice 与 Whisper 两条路径的进程上限是**不同**的两个常量，
    /// 且 SenseVoice 更宽（6 > 4）。设置页滑条量程按引擎取，正是靠这个不等式；
    /// 若哪天两个上限被改成相等或反向（SenseVoice 更窄），滑条就会把本可跑满的
    /// 档位砍掉、或给出超过引擎上限的档位——两者都是界面与实际不符。
    #[test]
    fn sv_and_whisper_worker_limits_are_ordered() {
        // 常量不等式放进 `const` 块，避免 clippy 的 `assertions_on_constants`。
        const { assert!(MAX_SV_WORKERS > MAX_WORKERS) };
        for cores in [1u32, 2, 4, 8, 16, 32, 64] {
            let n = sensevoice_worker_count(cores);
            assert!(
                (2..=MAX_SV_WORKERS).contains(&n),
                "自动进程数 {n}（{cores} 核）越出 [2, {MAX_SV_WORKERS}]"
            );
        }
        for threads in [2u32, 8, 16, 32, 64] {
            assert!(
                worker_count(64, threads, false) <= MAX_WORKERS,
                "自动进程数越出 Whisper 上限"
            );
        }
    }

    /// 回归：SenseVoice 路径的显式进程数必须夹到 `MAX_SV_WORKERS`(6)，
    /// 而不是 Whisper 的 `MAX_WORKERS`(4)。设置页在 SenseVoice 下量程给到 6，
    /// 若这里误夹到 4，用户选 5/6 就是静默降级。
    #[test]
    fn explicit_sv_worker_override_respects_max_sv_workers() {
        // 与 `transcribe_chunked_sensevoice` 中显式分支同一行表达式。
        let resolve = |w: usize| -> usize {
            match Some(w) {
                Some(w) if w >= 1 => w.clamp(1, MAX_SV_WORKERS),
                _ => sensevoice_worker_count(16),
            }
        };
        for w in 1..=16 {
            assert!(
                resolve(w) <= MAX_SV_WORKERS,
                "显式 {w} 个进程越过了 SenseVoice 上限 {MAX_SV_WORKERS}"
            );
        }
        // 5/6 档必须真的可用（Whisper 的上限 4 会砍掉这两档）
        assert_eq!(resolve(5), 5, "SenseVoice 下 5 进程应当可选");
        assert_eq!(resolve(6), MAX_SV_WORKERS);
        assert_eq!(resolve(8), MAX_SV_WORKERS, "8 应被夹到上限");
        assert_eq!(resolve(1), 1, "显式 1 应保留（走整段路径）");
    }
    #[test]
    fn cpu_workers_keep_eight_threads_per_process() {
        assert_eq!(worker_count(6, 8, false), 2);
        assert_eq!(worker_count(6, 16, false), 2);
        assert_eq!(worker_count(6, 32, false), 4);
    }

    #[test]
    fn target_chunk_len_controls_chunk_count() {
        // 32 分钟样片：360s 目标 -> 6 块；更短的目标块长应切出更多块
        assert_eq!(plan_chunks_target(1942.0, 360.0).len(), 6);
        assert!(plan_chunks_target(1942.0, 150.0).len() >= 12);
        // 太短的音频仍是单块
        assert_eq!(plan_chunks_target(60.0, 45.0).len(), 1);
    }

    #[test]
    fn target_chunks_cover_timeline_and_overlap() {
        let chunks = plan_chunks_target(1000.0, 120.0);
        assert_eq!(chunks[0].start, 0.0);
        let last = chunks.last().unwrap();
        assert!(
            (last.start + last.duration - 1000.0).abs() < 0.05,
            "末块应贴住片尾"
        );
        for w in chunks.windows(2) {
            assert!(
                w[0].start + w[0].duration > w[1].start,
                "相邻块必须重叠，否则边界语音会被切断"
            );
        }
    }

    /// 4 小时长音频：步长按实际块数均分，末块必须贴住片尾、相邻块严格重叠，
    /// 否则后半段音频会无人转写。
    #[test]
    fn plan_chunks_target_long_audio_still_covers_tail() {
        let chunks = plan_chunks_target(4.0 * 3600.0, 45.0);
        let last = chunks.last().unwrap();
        assert!(
            (last.start + last.duration - 4.0 * 3600.0).abs() < 0.05,
            "末块必须贴住片尾，否则后半段音频无人转写: {last:?}"
        );
        for w in chunks.windows(2) {
            assert!(
                w[0].start + w[0].duration > w[1].start,
                "相邻块必须重叠，否则边界语音会被切断"
            );
        }
    }

    /// 回归：调用方以 `target = total/chunk_count` 求切块时，块数必须还原成该整数。
    ///
    /// `transcribe_chunked_sensevoice` 先用整数运算算出 worker 整数倍的块数，再以
    /// `total/块数` 作目标块长。数学上 `total / (total / n) == n`，但 f64 除法会把它
    /// 舍入到略大于 `n`（如 `4324.0 / (4324.0 / 14.0) == 14.000000000000002`），
    /// 直接 `ceil` 就凭空多切一块：末轮只剩一个 worker 在跑、其余空转。
    #[test]
    fn plan_chunks_target_restores_integer_count_from_float_roundoff() {
        let total = 4324.0f64;
        let count = 14usize;
        let chunks = plan_chunks_target(total, total / count as f64);
        assert_eq!(
            chunks.len(),
            count,
            "target = total/{count} 时块数必须仍为 {count}，浮点 ceil 不得多切一块"
        );
        let last = chunks.last().unwrap();
        assert!(
            (last.start + last.duration - total).abs() < 0.05,
            "还原块数后的末块仍须贴住片尾: {last:?}"
        );
    }

    /// 反例保护：上面的容差只应抵消浮点舍入，不能把「恰为整数倍」的商少算一块
    /// （少切一块会让末块变长，覆盖面从另一头被削减）。
    #[test]
    fn plan_chunks_target_keeps_exact_multiples_intact() {
        assert_eq!(plan_chunks_target(720.0, 360.0).len(), 2);
        assert_eq!(plan_chunks_target(2.0 * 720.0, 360.0).len(), 4);
        // 4 小时 / 45s = 320 块，恰好整除
        assert_eq!(plan_chunks_target(4.0 * 3600.0, 45.0).len(), 320);
    }

    /// SenseVoice 并行路径的核心不变量：`transcribe_chunked_sensevoice` 按 worker
    /// 整数倍算出块数 `cc`、再以 `total/cc` 作目标块长；只要目标块长不被
    /// `MIN_CHUNK_SEC` 抬高、也不落进「整段更划算」的单块区间，
    /// `plan_chunks_target` 就必须还原出恰好 `cc` 块——多切则末轮有 worker 空转，
    /// 少切则每块被拉长、并行度下降。
    #[test]
    fn sensevoice_chunk_count_is_worker_multiple() {
        // 与 `transcribe_chunked_sensevoice` 中整数块数推导同一套表达式。
        let sv_count = |total: f64, w: usize| {
            let mut cc = w;
            while total / cc as f64 > CHUNK_SEC {
                cc += w;
            }
            cc
        };
        let mut checked = 0usize;
        for w in 2..=MAX_SV_WORKERS {
            for i in 0..10_300usize {
                let total = 200.0 + i as f64 * 0.37;
                if total > 4000.0 {
                    break;
                }
                let cc = sv_count(total, w);
                let target = total / cc as f64;
                // 两条有意为之的偏差（不属本不变量，见 `plan_chunks_target` 注释）：
                // 音频短到只值一整段；目标块长低于下限而被抬高。
                let lower = target.max(MIN_CHUNK_SEC);
                if total > lower + MIN_CHUNK_SEC && target >= MIN_CHUNK_SEC {
                    let got = plan_chunks_target(total, target).len();
                    assert_eq!(
                        got, cc,
                        "total={total} workers={w}: 期望 {cc} 块（worker 整数倍），实得 {got}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 10_000, "覆盖面过窄：只检查了 {checked} 组");
    }

    /// 拼接结果里不得残留相邻的「时间几乎重叠」字幕：这是块边界被同一句切两半时
    /// 最常见的形态，也是下游 `optimize_segments` 会当成鬼影剔掉的那一类。
    #[test]
    fn stitch_output_has_no_overlapping_neighbours() {
        let a = vec![
            Segment::new(1, 0.0, 3.0, "第一句内容"),
            Segment::new(2, 3.0, 8.0, "第二句内容比较长"),
        ];
        let b = vec![
            Segment::new(1, 0.1, 4.0, "第三句内容"),
            Segment::new(2, 4.0, 9.0, "第四句内容比较长"),
        ];
        let out = stitch_chunks(vec![(0, 0.0, a), (1, 358.0, b)]);
        for w in out.windows(2) {
            assert!(w[0].end <= w[1].start + 1e-9, "拼接后仍存在交叉区间: {w:?}");
        }
        let last = out.last().unwrap();
        assert!(
            (last.end - 367.0).abs() < 0.6,
            "末句终点应接近音频真实结尾（367s）: {last:?}"
        );
    }

    fn write_silent_wav(path: &Path, seconds: u32, rate: u32) {
        let samples = seconds * rate;
        let data_len = samples * 2;
        let mut buf = Vec::with_capacity(44 + data_len as usize);
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len).to_le_bytes());
        buf.extend_from_slice(b"WAVE");
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&rate.to_le_bytes());
        buf.extend_from_slice(&(rate * 2).to_le_bytes());
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.resize(44 + data_len as usize, 0);
        std::fs::write(path, &buf).unwrap();
    }

    /// 通用切块执行器：用假转写器验证时间轴偏移、去重叠拼接与重新编号。
    /// 仅切片需要真实 ffmpeg，缺失时自动跳过。
    #[test]
    fn run_chunked_parallel_offsets_and_stitches() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav = std::env::temp_dir().join("v2w_test_chunk_src.wav");
        write_silent_wav(&wav, 100, 16000);
        // 与泄漏用例串行：本用例调用期间会新建 `v2w_chunks_*` 目录，若与泄漏用例的
        // 前后快照窗口重叠，会让对方把这里的目录误判成自己的残留。
        let _serial = chunk_test_guard();

        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);
        assert_eq!(chunks.len(), 2);

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let transcribe_one = |p: &Path, dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            assert!(p.exists(), "传给转写器的切片必须已落在磁盘上: {:?}", p);
            assert!(dur > 0.0, "块时长必须为正: {dur}");
            // 每块都返回一个块内 0~1s 的片段，用于验证外部偏移是否正确施加
            Ok((vec![Segment::new(1, 0.0, 1.0, format!("块{n}"))], 0.0))
        };
        let (segs, _vad) = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &transcribe_one)
            .expect("切块并行应成功");

        assert_eq!(segs.len(), 2, "两块各产出一条片段: {segs:?}");
        assert!((segs[0].start - 0.0).abs() < 1e-6);
        assert!(
            segs[1].start > 40.0,
            "第二块应被偏移到时间轴后段: {}",
            segs[1].start
        );
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "拼接后应重新连续编号");

        let _ = std::fs::remove_file(&wav);
    }

    /// 执行器收尾必须与整段路径同形：跨块拼出的超长句要在 `run_chunked_parallel`
    /// 出口就被拆到 6 秒内。此前切块路径整个跳过了 `optimize_segments`，
    /// 于是「开并行」与「不开并行」得到两套不同粒度的字幕。
    #[test]
    fn run_chunked_parallel_optimizes_merged_output() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav =
            std::env::temp_dir().join(format!("v2w_test_chunk_opt_{}.wav", std::process::id()));
        write_silent_wav(&wav, 100, 16000);
        let _serial = chunk_test_guard();

        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);
        assert_eq!(chunks.len(), 2);
        // 带标点的 10 秒长句：`optimize_segments` 应把它拆成 6 秒以内的多条
        let long = "前面这半句讲的是背景，后面这半句讲的是结论。";
        let transcribe_one = |_p: &Path, _dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            Ok((vec![Segment::new(1, 0.0, 10.0, long)], 0.0))
        };
        let (segs, _vad) = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &transcribe_one)
            .expect("切块并行应成功");

        assert!(segs.len() >= 2, "10 秒长句应在执行器出口被拆短: {segs:?}");
        for s in &segs {
            assert!(s.duration() <= 6.0 + 1e-9, "执行器出口仍残留超长句: {s:?}");
        }
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2, "拆分后应重新连续编号");

        let _ = std::fs::remove_file(&wav);
    }

    /// 本模块内所有会调用 `run_chunked_parallel` 的用例共用一把锁。
    ///
    /// 该函数会在 `%TEMP%` 下按 `v2w_chunks_{pid}_{millis}` 建目录，而泄漏用例靠
    /// 「调用前后目录集合的差」判定残留。若两个用例并行，兄弟用例在这段窗口里新建的
    /// 目录会被算成泄漏用例的残留（假失败）。串行化后，窗口内只可能留下本用例自己
    /// 创建的目录，集合差才真正等价于「本用例是否泄漏」。
    static CHUNK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 取得串行锁；前一个用例若 panic 会毒化锁，这里恢复内部值以免连累后续用例。
    fn chunk_test_guard() -> std::sync::MutexGuard<'static, ()> {
        CHUNK_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 快照 `%TEMP%` 下「本进程」创建的切块目录名，用于断言「失败后没有新增残留」。
    ///
    /// 过滤 `v2w_chunks_{pid}_` 前缀：其它测试二进制（各自独立进程）并发跑时也会建
    /// 同前缀目录，但 pid 不同，据此把它们排除，只对本进程的目录负责。
    fn chunk_dirs_in_temp() -> std::collections::HashSet<String> {
        let prefix = format!("v2w_chunks_{}_", std::process::id());
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return Default::default();
        };
        entries
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect()
    }

    /// 验证泄漏修复：切片已建好、转写中途失败时，切块临时目录必须被清掉。
    ///
    /// 这条路径过去会直接 `return Err(err)` 跳过函数末尾的 `remove_dir_all`，
    /// 把整个目录（每块数十 MB 的 WAV）永久留在 TEMP 里。
    #[test]
    fn chunk_failure_leaves_no_temp_dir() {
        let cfg: toml::Value = std::fs::read_to_string("config.toml")
            .ok()
            .and_then(|c| toml::from_str(&c).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let Some(ffmpeg_path) = cfg["paths"]["ffmpeg"].as_str() else {
            eprintln!("跳过：config.toml 未配置 ffmpeg");
            return;
        };
        if !Path::new(ffmpeg_path).exists() {
            eprintln!("跳过：ffmpeg 不存在");
            return;
        }

        let wav =
            std::env::temp_dir().join(format!("v2w_test_chunk_fail_{}.wav", std::process::id()));
        write_silent_wav(&wav, 100, 16000);
        let ffmpeg = FFmpegEngine::new(ffmpeg_path);
        let chunks = plan_chunks_target(100.0, 50.0);

        // 只比较「调用后新增」的目录：本模块其它用例也会建 `v2w_chunks_*`，所以先拿
        // 串行锁保证窗口内没有别的用例在并发建目录，再按本进程 pid 过滤，把集合差
        // （after - before 必须为空）限定在「本次调用自己可能创建的目录」上。
        let _serial = chunk_test_guard();
        let before = chunk_dirs_in_temp();
        let failing = |_p: &Path, _dur: f64, _th: u32| -> Result<(Vec<Segment>, f64)> {
            anyhow::bail!("模拟块转写失败")
        };
        let res = run_chunked_parallel(&ffmpeg, &wav, chunks, 2, 2, None, &failing);
        let after = chunk_dirs_in_temp();

        assert!(res.is_err(), "转写器返回 Err 时执行器必须把错误上抛");
        let leaked: Vec<&String> = after.difference(&before).collect();
        assert!(
            leaked.is_empty(),
            "失败后切块目录必须被删除，残留: {leaked:?}"
        );

        let _ = std::fs::remove_file(&wav);
    }
}

/// 切块规划的数值边界与重叠去重的范围：这两处出的错都不会报错，只会静默少字 /
/// 空转，因此逐条钉死。
#[cfg(test)]
mod chunk_boundary_tests {
    use super::{
        plan_chunks_target, plan_chunks_whisper, sensevoice_chunk_count, stitch_chunks, CHUNK_SEC,
        MAX_CHUNK_COUNT,
    };
    use crate::subtitle::Segment;

    /// 非有限时长不得进分块算术。
    ///
    /// `(inf / target - eps).ceil()` 是 `inf`，`as usize` 饱和成 `usize::MAX`：
    /// `(0..count).map(..).collect()` 会按 `size_hint` 预分配 → 容量溢出 panic。
    /// `NaN` 好一点也坏一点：不 panic，但每块的 `start` / `duration` 全成 `NaN`，
    /// 直接进 ffmpeg 的切片参数。两者都必须退回单块（调用方随后落回整段路径）。
    #[test]
    fn non_finite_duration_collapses_to_single_chunk() {
        for total in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let chunks = plan_chunks_whisper(total);
            assert_eq!(chunks.len(), 1, "非有限时长 {total} 必须退回单块");
            assert_eq!(chunks[0].start, 0.0);
            assert!(
                chunks[0].duration.is_finite() && chunks[0].duration > 0.0,
                "单块必须拿到有限的正时长，而不是 NaN/inf: {:?}",
                chunks[0]
            );
        }
    }

    /// 极大时长不得让块数无上限：损坏的时长字段（1e9 秒）曾会切出上千万块，
    /// 每块一个临时 WAV + 一个转写进程。封顶只该挡住这种值，真实素材逐档不变。
    #[test]
    fn chunk_count_is_capped_for_absurd_durations() {
        for chunks in [
            plan_chunks_whisper(1.0e9),
            plan_chunks_target(1.0e9, 45.0),
            plan_chunks_target(f64::MAX, 45.0),
        ] {
            assert!(
                chunks.len() <= MAX_CHUNK_COUNT,
                "块数 {} 越过上限 {MAX_CHUNK_COUNT}",
                chunks.len()
            );
            assert!(chunks.len() >= 2, "封顶不该把长素材压成单块");
            for c in &chunks {
                assert!(
                    c.start.is_finite() && c.duration.is_finite() && c.duration > 0.0,
                    "封顶后的几何仍须有限且为正: {c:?}"
                );
            }
        }
        // 真实量级：4 小时 / 45s 恰好 320 块，不受封顶影响
        assert_eq!(plan_chunks_target(4.0 * 3600.0, 45.0).len(), 320);
        assert!(plan_chunks_whisper(32.0 * 60.0).len() <= MAX_CHUNK_COUNT);
    }

    /// SenseVoice 的块数推导：必须是 worker 的整数倍（否则末轮有 worker 空转）、
    /// 单块不超过 `CHUNK_SEC`，且**非有限时长必须直接返回**——`inf / 块数` 恒为
    /// `inf`，`while` 永不终止，真死循环连超时都等不到。
    #[test]
    fn sensevoice_chunk_count_terminates_and_stays_worker_multiple() {
        for workers in [1usize, 2, 4, 6] {
            assert_eq!(
                sensevoice_chunk_count(f64::INFINITY, workers),
                workers,
                "inf 时长必须立即返回，不得进 while 循环"
            );
            assert_eq!(sensevoice_chunk_count(f64::NAN, workers), workers);
            assert_eq!(sensevoice_chunk_count(f64::NEG_INFINITY, workers), workers);
        }
        // workers = 0 不得 panic（`.max(1)` 兜底）也不得死循环
        assert_eq!(sensevoice_chunk_count(f64::INFINITY, 0), 1);

        for workers in 1usize..=6 {
            for total in [0.0f64, 100.0, 405.0, 1000.0, 4324.0, 4.0 * 3600.0] {
                let count = sensevoice_chunk_count(total, workers);
                assert!(count >= workers, "块数不得少于一轮 workers");
                assert_eq!(count % workers, 0, "块数必须是 worker 的整数倍");
                assert!(
                    total / count as f64 <= CHUNK_SEC + 1e-9,
                    "total={total} workers={workers}: 单块 {} 超过 {CHUNK_SEC}s",
                    total / count as f64
                );
            }
        }
    }

    /// 块内的重复句必须保留。重叠只存在于相邻两块之间，同一块内不存在重叠来源：
    /// 时间上并不重叠的两句完全相同的字幕，是说话人真的重复了一遍（跟读 / 听写类
    /// 课程极常见）。旧实现无条件套用重叠去重，会把第二句静默吃掉。
    #[test]
    fn repeated_sentence_inside_one_chunk_is_kept() {
        let repeated = vec![
            Segment::new(1, 10.0, 12.0, "我们把这句话再读一遍"),
            Segment::new(2, 12.4, 14.4, "我们把这句话再读一遍"),
        ];
        let out = stitch_chunks(vec![(0, 0.0, repeated)]);
        assert_eq!(
            out.len(),
            2,
            "块内时间不重叠的重复句不得被当成跨块重叠丢掉: {out:?}"
        );
        assert_eq!(out[0].text, "我们把这句话再读一遍");
        assert_eq!(out[1].text, "我们把这句话再读一遍");
    }

    /// 反向保护：跨块的重复句（同一句话被相邻两块各转了一次，后块落在重叠区里）
    /// 仍必须只保留一条，且保留更早的起点——这正是边界合并的本意，修块内误删不能
    /// 把它一起放掉。
    #[test]
    fn duplicate_sentence_across_chunk_boundary_is_dropped() {
        let head = vec![Segment::new(1, 350.0, 358.0, "我们把这句话再读一遍")];
        let tail = vec![Segment::new(1, 0.2, 8.0, "我们把这句话再读一遍")];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 358.5, tail)]);
        assert_eq!(out.len(), 1, "跨块重复句仍应被丢掉: {out:?}");
        assert_eq!(out[0].start, 350.0);
    }
}

/// 重叠切块 + 边界模糊合并的专项用例：切块规划的数值边界、相似度的边界、合并决策，
/// 以及拼接后的时间顺序与连续编号。
///
/// 只测纯函数，不碰 ffmpeg / 子进程。
#[cfg(test)]
mod overlap_merge_tests {
    use super::{
        merge_overlap, plan_chunks, stitch_chunks, text_similarity, MergeDecision,
        CHUNK_OVERLAP_SEC, MAX_CHUNK_COUNT, OVERLAP_MATCH_THRESHOLD,
    };
    use crate::subtitle::Segment;

    fn seg(start: f64, end: f64, text: &str) -> Segment {
        Segment::new(1, start, end, text)
    }

    // ---------- 相似度函数的边界 ----------

    #[test]
    fn similarity_identical_is_one() {
        let s = text_similarity("我们把这句话再读一遍", "我们把这句话再读一遍");
        assert!((s - 1.0).abs() < 1e-9, "相同文本应为 1.0，实得 {s}");
    }

    #[test]
    fn similarity_disjoint_is_zero() {
        assert_eq!(text_similarity("甲乙丙丁戊", "12345"), 0.0);
    }

    #[test]
    fn similarity_empty_or_punctuation_only_is_zero() {
        assert_eq!(text_similarity("", ""), 0.0);
        assert_eq!(text_similarity("", "有内容"), 0.0);
        assert_eq!(text_similarity("有内容", ""), 0.0);
        assert_eq!(text_similarity("，。！？ ", "……——  "), 0.0);
    }

    #[test]
    fn similarity_ignores_punctuation_whitespace_and_case() {
        let en = text_similarity("Hello, World!", "  hello world  ");
        assert!(
            (en - 1.0).abs() < 1e-9,
            "标点/空白/大小写差异应被抹平，实得 {en}"
        );
        let zh = text_similarity("切比雪夫不等式，对吗？", "切比雪夫不等式 对吗");
        assert!((zh - 1.0).abs() < 1e-9, "中英标点差异应被抹平，实得 {zh}");
    }

    #[test]
    fn similarity_handles_mixed_cjk_and_latin() {
        // 归一化后 "abc中文def"(8) 与 "abc中def"(7)，LCS = 7 -> 2*7/15 = 14/15
        let sim = text_similarity("abc中文 def", "ABC中DEF");
        assert!((sim - 14.0 / 15.0).abs() < 1e-9, "实得 {sim}");
        assert!(
            text_similarity("abc中文 def", "ABCX") < sim,
            "缺得越多占比越低"
        );
    }

    // ---------- 阈值与合并决策 ----------

    #[test]
    fn merge_overlap_threshold_boundary_merges_at_threshold() {
        let a = seg(10.0, 20.0, "我们把这句话再读一遍");
        let b = seg(18.0, 24.0, "我们把这句话再读一遍");
        let exact = text_similarity(a.display_text(), b.display_text());
        // 判据是 `>=`：阈值取到相似度本身仍应命中
        assert!(
            matches!(merge_overlap(&a, &b, exact), MergeDecision::Merged(_)),
            "相似度恰好等于阈值应命中"
        );
        // 阈值再抬高一丁点：落到阈值之下，两条都留
        assert_eq!(merge_overlap(&a, &b, exact + 1e-6), MergeDecision::KeepBoth);
    }

    #[test]
    fn merge_overlap_keeps_earlier_start_and_later_end() {
        let prev = seg(100.0, 108.0, "我们把这句话再读一遍");
        let next = seg(106.0, 114.0, "我们把这句话再读一遍");
        // 无论谁先传入，结果都以时间更早者为准
        for (a, b) in [(&prev, &next), (&next, &prev)] {
            match merge_overlap(a, b, OVERLAP_MATCH_THRESHOLD) {
                MergeDecision::Merged(m) => {
                    assert_eq!(m.start, 100.0, "start 应取更早者");
                    assert_eq!(m.end, 114.0, "end 应取较晚者，不能停在 108");
                }
                other => panic!("应判为同一句并合并，实得 {other:?}"),
            }
        }
    }

    #[test]
    fn merge_overlap_keeps_both_when_dissimilar() {
        let prev = seg(100.0, 108.0, "我们现在看第二个定理");
        let next = seg(108.2, 115.0, "它的证明用到了柯西不等式");
        assert_eq!(
            merge_overlap(&prev, &next, OVERLAP_MATCH_THRESHOLD),
            MergeDecision::KeepBoth
        );
    }

    #[test]
    fn stitch_merge_keeps_ascending_time_and_continuous_index() {
        let head = vec![
            seg(0.0, 5.0, "第一句内容"),
            seg(5.0, 10.0, "我们把这句话再读一遍"),
        ];
        // 第二块从 10s 起：头部那句与上一块尾部重复，随后是一句新内容
        let tail = vec![
            seg(0.05, 5.0, "我们把这句话再读一遍"),
            seg(5.0, 10.0, "下一句是新内容"),
        ];
        let out = stitch_chunks(vec![(0, 0.0, head), (1, 10.0, tail)]);
        assert_eq!(out.len(), 3, "边界重复句应合并成一条: {out:?}");
        assert_eq!(out[1].start, 5.0, "合并后保留更早的 start: {out:?}");
        assert_eq!(out[1].end, 15.0, "合并后 end 取较晚者: {out:?}");
        for w in out.windows(2) {
            assert!(w[0].start <= w[1].start, "拼接后必须保持时间升序: {out:?}");
        }
        for (i, s) in out.iter().enumerate() {
            assert_eq!(s.index, i + 1, "index 必须从 1 连续: {out:?}");
        }
    }

    // ---------- 切块规划的边界 ----------

    #[test]
    fn plan_chunks_zero_duration_is_single_minimal_chunk() {
        let spans = plan_chunks(0.0, 360.0, CHUNK_OVERLAP_SEC);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, 0.0);
        assert!(spans[0].1 > 0.0 && spans[0].1.is_finite(), "须给最小正时长");
    }

    #[test]
    fn plan_chunks_exactly_one_chunk_stays_single() {
        assert_eq!(plan_chunks(360.0, 360.0, CHUNK_OVERLAP_SEC).len(), 1);
        // 门限 = 块长 + MIN_CHUNK_SEC = 405：等于门限仍是单块
        assert_eq!(plan_chunks(405.0, 360.0, CHUNK_OVERLAP_SEC).len(), 1);
    }

    #[test]
    fn plan_chunks_just_over_boundary_splits_with_overlap() {
        let spans = plan_chunks(405.5, 360.0, CHUNK_OVERLAP_SEC);
        assert_eq!(spans.len(), 2, "刚过门限应切成两块: {spans:?}");
        assert_eq!(spans[0].0, 0.0);
        assert!(spans[0].0 + spans[0].1 > spans[1].0, "相邻块必须重叠");
        let last = spans.last().unwrap();
        assert!((last.0 + last.1 - 405.5).abs() < 1e-9, "末块必须贴住片尾");
    }

    #[test]
    fn plan_chunks_clamps_overlap_larger_than_chunk() {
        let chunk = 100.0;
        let spans = plan_chunks(1000.0, chunk, 500.0);
        assert!(spans.len() > 1);
        for w in spans.windows(2) {
            let overlap = w[0].0 + w[0].1 - w[1].0;
            assert!(overlap > 0.0, "相邻块仍须重叠: {w:?}");
            assert!(overlap < chunk, "重叠必须夹到小于块长，实得 {overlap}");
            assert!(w[1].0 >= w[0].0, "start 必须单调不减");
        }
        for s in &spans {
            assert!(s.1 <= chunk * 1.5 + 1e-9, "块长失控: {s:?}");
        }
    }

    #[test]
    fn plan_chunks_non_finite_and_huge_terminate() {
        for d in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let spans = plan_chunks(d, 360.0, CHUNK_OVERLAP_SEC);
            assert_eq!(spans.len(), 1, "坏时长 {d} 必须退回单块");
            assert_eq!(spans[0].0, 0.0);
            assert!(spans[0].1.is_finite() && spans[0].1 > 0.0);
        }
        // 极大时长：块数封顶、绝不无界，几何全部有限
        let spans = plan_chunks(1.0e9, 45.0, CHUNK_OVERLAP_SEC);
        assert!(
            spans.len() >= 2 && spans.len() <= MAX_CHUNK_COUNT,
            "块数须封顶，实得 {}",
            spans.len()
        );
        for s in &spans {
            assert!(
                s.0.is_finite() && s.1.is_finite() && s.1 > 0.0,
                "几何须有限: {s:?}"
            );
        }
        // 非有限的块长 / 重叠也不得让几何变 NaN
        for s in plan_chunks(1000.0, f64::NAN, f64::NAN) {
            assert!(
                s.0.is_finite() && s.1.is_finite() && s.1 > 0.0,
                "几何须有限: {s:?}"
            );
        }
    }
}
