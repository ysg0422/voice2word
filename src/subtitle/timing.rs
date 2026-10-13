//! 整轨时间轴变换的纯规划层。
//!
//! 字幕到手时的时间轴几乎从不与成片对齐：视频剪掉了 12 秒，整条轨就得平移；
//! 片头插了 30 秒，某个时间点之后的每一句都得后移；一条按 25 分钟剪辑做的轨
//! 要复用到 22 分钟的成片上，就得整体缩放；TXT 导入生成的合成时间轴还要按真实
//! 时长重新铺满。今天这些活全靠手工拖行，几百句就是几百次拖拽。
//!
//! 本模块是这块能力的**纯规划层**：吃 `&[Segment]` 与一个 [`TimeShift`]，吐一份
//! [`RetimePlan`]（每条的新区间 + 改了多少条 + 有没有压出重叠/零长片段）。它
//! **从不修改输入**，也不碰 IO、不碰 UI，因此可以脱离 GPUI 直接单测，界面也能
//! 先预览结果再决定是否落库。
//!
//! 几条刻意为之的约定（为什么这样算，见各条目注释）：
//!
//! - **负位移不能让首条被削短**：`start` 被夹到 0 时，`end` 同步右移同样的量。
//!   否则一条 `(0.2, 3.2)` 在 `-1.0` 位移后会变成 `(0.0, 2.2)`，时长凭空少 1 秒。
//! - **非法跨度绝不产出 `NaN`/`inf`**：缩放的分母为 0 或非有限时原样返回。
//!   一个 `NaN` 起点会让片段从时间轴上彻底消失，还会被写进 DB 再也捞不回来。
//! - **铺满用累积求和，末尾钉死在目标值**：逐条相乘再定位会在片尾攒出几毫秒
//!   误差，22 分钟的片子末尾就会与画面错开，逐帧对不上。
//! - **本层只报告不修复**：重叠/零长由 [`RetimePlan`] 的两个标志如实汇报，
//!   要「压掉」重叠时再调用 [`resolve_overlaps`]。
//!
//! 除了「整轨变换」，这里还放着**逐句时长的可读性规划**：阅读速度（字符/秒）超限的
//! 句子该延长多久、能延长到哪（[`cps_violations`] / [`plan_cps_fix`]）。它与整轨
//! 变换共用同一套约束（不改文本、不与邻居重叠、时长下限 [`MIN_EDIT_DUR`]），放进
//! 同一个模块是为了让「改时间」这件事只有一处口径——散成两处后，一处按 0.1 秒保底、
//! 另一处按 0.25 秒保底，用户就会遇到「按这个按钮能修好、按那个按钮下次载入丢句」。

use std::cmp::Ordering;

use crate::subtitle::segment::{Segment, MIN_EDIT_DUR};
use crate::subtitle::stats;

/// 判定「起点真的变了」的浮点容差（秒）。
const CHANGED_TOL: f64 = 1e-6;

/// 判定「相邻区间重叠」的浮点容差（秒）。
const OVERLAP_TOL: f64 = 1e-9;

/// 整轨时间轴变换。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeShift {
    /// 全部平移 `delta` 秒（正数往后）
    ShiftAll { delta: f64 },
    /// 只平移 `from_secs` 之后的片段（用于「开头插了一段」）
    ShiftAfter { from_secs: f64, delta: f64 },
    /// 线性缩放：把 `source_span` 映射到 `target_span`
    Scale { source_span: f64, target_span: f64 },
    /// 按段落长度比例，把整轨重新铺满 `target_secs`（TXT 导入的合成时间轴）
    FitToDuration { target_secs: f64 },
}

/// 一次变换的规划结果。
#[derive(Debug, Clone, PartialEq)]
pub struct RetimePlan {
    /// 与输入等长的新 `(start, end)` 序列
    pub times: Vec<(f64, f64)>,
    /// 有多少条的开始时间真的变了（浮点比较用 1e-6 容差）
    pub changed: usize,
    /// 是否可能产生重叠（变换后相邻区间 start < 前一条 end）
    pub overlaps: bool,
    /// 变换后是否出现非正时长的片段
    pub degenerate: bool,
}

/// 规划一次整轨变换。**不修改输入**。
pub fn plan_retime(segments: &[Segment], shift: TimeShift) -> RetimePlan {
    let base: Vec<(f64, f64)> = segments.iter().map(|s| (s.start, s.end)).collect();
    let times = match shift {
        TimeShift::ShiftAll { delta } => shift_all(&base, delta),
        TimeShift::ShiftAfter { from_secs, delta } => shift_after(&base, from_secs, delta),
        TimeShift::Scale {
            source_span,
            target_span,
        } => scale_times(&base, source_span, target_span),
        TimeShift::FitToDuration { target_secs } => fit_to_duration(&base, target_secs),
    };
    finish(&base, times)
}

/// 变换后若发现重叠，给出「把重叠压掉」的建议序列（仍不改输入）。
///
/// 规则：按 start 升序，逐条把 start 抬到 `前一条 end` 之后（若因此压到比
/// `MIN_EDIT_DUR` 还短，则改为收前一条的 end）；最后做一次「终点至少比起点晚
/// `MIN_EDIT_DUR`」的自纠。返回的序列保证：升序、无重叠、每条时长 >= `MIN_EDIT_DUR`、
/// start >= 0。
pub fn resolve_overlaps(times: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = times.to_vec();
    // 按 start 升序；起点相等时保持原顺序，NaN 用 Equal 兜底以免 panic。
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));

    // 先让每条自身合法：起点 >= 0，终点至少比起点晚 MIN_EDIT_DUR。
    for cue in out.iter_mut() {
        let start = cue.0.max(0.0);
        cue.0 = start;
        cue.1 = cue.1.max(start + MIN_EDIT_DUR);
    }

    // 逐条把 start 抬到前一条 end；抬完若自己短于下限，就改为收前一条的 end，
    // 让本条保住原有终点——这就是「优先压前一条」的取舍。
    let mut i = 1;
    while i < out.len() {
        let prev_end = out[i - 1].1;
        if out[i].0 < prev_end {
            if out[i].1 - prev_end >= MIN_EDIT_DUR {
                out[i].0 = prev_end;
            } else {
                let shrunk = out[i].1 - MIN_EDIT_DUR;
                out[i - 1].1 = shrunk;
                out[i].0 = shrunk;
            }
        }
        i += 1;
    }

    // 自纠：收前一条 end 可能把它自己压短、或压进更前面一条，这里自左向右重跑
    // 一遍「起点 >= 前一条 end、时长 >= MIN_EDIT_DUR」，把上一步的连带副作用抹平。
    let mut cursor = 0.0_f64;
    for cue in out.iter_mut() {
        let start = cue.0.max(cursor).max(0.0);
        let end = cue.1.max(start + MIN_EDIT_DUR);
        cue.0 = start;
        cue.1 = end;
        cursor = end;
    }
    out
}

/// 相邻字幕之间要求的**最小空隙**（秒）。
///
/// 为什么值得单独做：识别器给出的时间轴经常让两条字幕首尾相贴（甚至互相压几毫秒），
/// 播放时上一句还没从屏幕上消失、下一句就已经开始读——观众的心理感受是「字挤在一起」，
/// 而手工一句句去拖，几百句就是几百次拖拽。本模块与整轨变换共用同一套「先算计划、
/// 再落库」的形状：界面拿 [`plan_min_gaps`] 显示「将改动 N 句」，确认后才写回。
///
/// 取 40ms 而不是 0：人眼对「两行字同时在场」的阈值远高于一帧，40ms 在 24~60fps 下
/// 都足够让上一行先消失，又不至于为了凑空隙把短句压到不可读。
pub const DEFAULT_MIN_GAP_SECS: f64 = 0.04;

/// 一次「最小句间间隔」调整的计划（只算不改，见模块头）。
#[derive(Debug, Clone, PartialEq)]
pub struct GapPlan {
    /// 与输入等长的新 `(start, end)` 序列
    pub times: Vec<(f64, f64)>,
    /// 有多少条的**起点**真的变了
    pub changed: usize,
    /// 有多少条被**缩短**了（为了给后一句让出空隙）
    pub shortened: usize,
    /// 调整后是否仍存在间隔不足的相邻对（被 [`MIN_EDIT_DUR`] 挡住而无法完全满足）
    pub unresolved: bool,
}

/// 规划一次「保证相邻字幕之间至少有 `min_gap` 秒空隙」的调整。**不修改输入**。
///
/// # 算法
///
/// 按 start 升序逐对处理（输入乱序时先排序，与 [`resolve_overlaps`] 同口径）：
/// 若 `next.start < prev.end + min_gap`，优先把 `next.start` 往后抬到
/// `prev.end + min_gap`；抬完若 `next` 自身短于 [`MIN_EDIT_DUR`]，则改为**收
/// `prev.end`**，让 `next` 保住原有终点——「优先压前一条」与 [`resolve_overlaps`]
/// 的取舍一致，两处调用方看到的规则是一样的。
///
/// 末句之后没有「下一条」，因此末句永远不被本函数改动。
///
/// # 为什么只挪起点、不擅自加长
///
/// 加长末句去凑空隙会改动**用户已经确认过的**结束时间；而本函数的目标只是
/// 「消除相贴」，起点后移已经达成。真正需要重新铺满时间轴时应改用
/// [`plan_retime`] 的 [`TimeShift::FitToDuration`]。
pub fn plan_min_gaps(segments: &[Segment], min_gap: f64) -> GapPlan {
    // NaN 与负值一律回落到默认间隔：负间隔意味着「允许重叠」，与本函数的目的
    // 相反；NaN 更会把整条轨写成 NaN。0 是**合法值**（只要不重叠即可，
    // 不额外要空隙），因此只有非有限或负数才回落。
    let gap = if min_gap.is_finite() && min_gap >= 0.0 {
        min_gap
    } else {
        DEFAULT_MIN_GAP_SECS
    };
    let base: Vec<(f64, f64)> = segments.iter().map(|s| (s.start, s.end)).collect();
    if base.len() < 2 {
        return GapPlan {
            times: base,
            changed: 0,
            shortened: 0,
            unresolved: false,
        };
    }

    // 稳定排序：起点相等时保持原顺序，NaN 用 Equal 兜底（与 resolve_overlaps 同口径）。
    let mut order: Vec<usize> = (0..base.len()).collect();
    order.sort_by(|&a, &b| base[a].0.partial_cmp(&base[b].0).unwrap_or(Ordering::Equal));

    let mut out = base.clone();
    let mut changed = 0usize;
    let mut shortened = 0usize;
    for pair in order.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        // 倒挂或零长的片段自己就是坏的，本函数不负责修（那是 resolve_overlaps 的活）。
        if out[prev].1 <= out[prev].0 {
            continue;
        }
        let want = out[prev].1 + gap;
        if out[next].0 >= want {
            continue;
        }
        if out[next].1 - want >= MIN_EDIT_DUR {
            out[next].0 = want;
            changed += 1;
        } else {
            // 后一句本来就短：改为收前一句的 end，让它保住终点。
            let shrunk = out[next].1 - MIN_EDIT_DUR;
            // 收短也有下界：前一句被收到 `MIN_EDIT_DUR` 以下就不再收——否则一条
            // 本该几秒的句子会被压成零点几秒，下次载入直接落进鬼影判据被删掉。
            if shrunk - out[prev].0 >= MIN_EDIT_DUR - OVERLAP_TOL && shrunk < out[prev].1 {
                out[prev].1 = shrunk;
                shortened += 1;
            }
            // 收不动（前一句也短得没法再收）就承认这一对没解决，如实上报。
        }
    }

    let unresolved = has_short_gaps(&out, gap);
    GapPlan {
        times: out,
        changed,
        shortened,
        unresolved,
    }
}

/// 排序后是否仍存在间隔不足 `gap` 的相邻对（用 `OVERLAP_TOL` 容忍浮点毛刺）。
fn has_short_gaps(times: &[(f64, f64)], gap: f64) -> bool {
    if times.len() < 2 {
        return false;
    }
    let mut sorted: Vec<(f64, f64)> = times.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    sorted
        .windows(2)
        .any(|pair| pair[1].0 < pair[0].1 + gap - OVERLAP_TOL)
}
/// 便于界面显示的一句话摘要，例如
/// "平移 +12.0s，涉及 190 句" / "缩放 1500.0s → 1320.0s" / "已重排 42 句"。
///
/// 只按 `shift` 生成，**不含句数**——句数是界面拿到 [`RetimePlan::changed`] 后自己
/// 追加的。这里不读时钟、不看区域设置，保证同样的输入永远给同样的文案。
pub fn describe(shift: TimeShift) -> String {
    match shift {
        TimeShift::ShiftAll { delta } => format!("平移 {delta:+.1}s"),
        TimeShift::ShiftAfter { from_secs, delta } => {
            format!("从 {from_secs:.1}s 起平移 {delta:+.1}s")
        }
        TimeShift::Scale {
            source_span,
            target_span,
        } => format!("缩放 {source_span:.1}s → {target_span:.1}s"),
        TimeShift::FitToDuration { target_secs } => format!("铺满 {target_secs:.1}s"),
    }
}

/// 「是否需要把时间轴铺满」：当整轨已有时长与 `target_secs` 相差超过 10% 时返回
/// `Some(建议的 FitToDuration)`，否则 `None`。界面据此提示用户（TXT 导入后必用）。
pub fn suggest_fit(segments: &[Segment], target_secs: f64) -> Option<TimeShift> {
    // 目标时长非正或非有限时，「相差百分比」没有意义，直接不提示。
    if target_secs <= 0.0 || !target_secs.is_finite() {
        return None;
    }
    if segments.is_empty() {
        return None;
    }
    // 整轨已有时长按「最晚终点 - 最早起点」算；只看 max(end) 会让起点不在 0 的
    // 轨道（例如从 5s 开始的工程）被高估一截，本来该提示的反而落在 10% 带内。
    let min_start = segments
        .iter()
        .map(|s| s.start)
        .fold(f64::INFINITY, f64::min);
    let max_end = segments
        .iter()
        .map(|s| s.end)
        .fold(f64::NEG_INFINITY, f64::max);
    let current = max_end - min_start;
    // 无有效长度（零长、倒挂、NaN）的合成时间轴本来就该铺满，直接建议。
    if current <= 0.0 || current.is_nan() {
        return Some(TimeShift::FitToDuration { target_secs });
    }
    let diff_ratio = (current - target_secs).abs() / target_secs;
    if diff_ratio > 0.10 {
        Some(TimeShift::FitToDuration { target_secs })
    } else {
        None
    }
}

/// 全部平移 `delta`；负位移把起点压到 0 以下时，终点同步右移同样的量以保时长。
fn shift_all(base: &[(f64, f64)], delta: f64) -> Vec<(f64, f64)> {
    if !delta.is_finite() {
        // NaN/inf 位移会把整条轨写成 NaN——宁可不动，也不产出看不见的片段。
        return base.to_vec();
    }
    base.iter()
        .map(|&(start, end)| {
            finite_or_original(clamp_negative(start + delta, end + delta), (start, end))
        })
        .collect()
}

/// 只平移 `start >= from_secs` 的片段；之前的片段原样保留（「开头插了一段」）。
fn shift_after(base: &[(f64, f64)], from_secs: f64, delta: f64) -> Vec<(f64, f64)> {
    if !delta.is_finite() || !from_secs.is_finite() {
        return base.to_vec();
    }
    base.iter()
        .map(|&(start, end)| {
            if start >= from_secs {
                finite_or_original(clamp_negative(start + delta, end + delta), (start, end))
            } else {
                (start, end)
            }
        })
        .collect()
}

/// 变换结果里若出现非有限值（输入自带 inf/NaN，或加减把有限值推到了 inf），
/// 回退到原区间：非有限时间会让片段从时间轴上彻底消失，还会顺着 DB 写进去捞不回来。
fn finite_or_original(new: (f64, f64), original: (f64, f64)) -> (f64, f64) {
    if new.0.is_finite() && new.1.is_finite() {
        new
    } else {
        original
    }
}

/// 把 `(start, end)` 的起点夹到 0：起点被抬多少，终点就跟着抬多少（时长不变）。
fn clamp_negative(start: f64, end: f64) -> (f64, f64) {
    if start < 0.0 {
        // end - start = end + |start|，正好补上起点被抬的那一段。
        (0.0, end - start)
    } else {
        (start, end)
    }
}

/// 线性缩放：`ratio = target_span / source_span`。
fn scale_times(base: &[(f64, f64)], source_span: f64, target_span: f64) -> Vec<(f64, f64)> {
    // 守卫：任一端点非有限或非正时原样返回。若放任 ratio 变成 NaN/inf，
    // 输出时间会全是 NaN——片段从时间轴消失，并顺着 DB 写进去再也找不回。
    if source_span <= 0.0
        || !source_span.is_finite()
        || target_span <= 0.0
        || !target_span.is_finite()
    {
        return base.to_vec();
    }
    let ratio = target_span / source_span;
    // 跨度本身都有限且为正，比值仍可能溢出成 inf（如 1e-300 → 1e300）：一旦如此，
    // 输出会全是 inf/NaN，片段从时间轴消失——比照守卫原样返回。
    if !ratio.is_finite() {
        return base.to_vec();
    }
    base.iter()
        .map(|&(start, end)| finite_or_original((start * ratio, end * ratio), (start, end)))
        .collect()
}

/// 按各段时长占比把整轨重新铺满 `target_secs`，末条终点精确落在目标值上。
fn fit_to_duration(base: &[(f64, f64)], target_secs: f64) -> Vec<(f64, f64)> {
    if target_secs <= 0.0 || !target_secs.is_finite() {
        return base.to_vec();
    }
    // 用「每条正时长之和」作分母；倒挂区间按 0 计，免得负数把总量拉小。
    let total: f64 = base
        .iter()
        .map(|&(start, end)| (end - start).max(0.0))
        .sum();
    if total <= 0.0 || !total.is_finite() {
        // 没有任何可分配的正时长（空轨或全倒挂）：原样返回，不做除法以免 0/0。
        return base.to_vec();
    }
    let ratio = target_secs / total;
    // 分母是极小正数（次正规）时比值会溢出成 inf，累积求和随即把整条轨写成 inf。
    if !ratio.is_finite() {
        return base.to_vec();
    }
    let mut out = Vec::with_capacity(base.len());
    // 累积求和而非「各自乘完再各自定位」：起点就是上一条的终点，逐条累加不会
    // 留下空隙或重叠，末条也只需一次钉死就能与目标严丝合缝。
    let mut cursor = 0.0_f64;
    for &(start, end) in base {
        let dur = (end - start).max(0.0) * ratio;
        out.push((cursor, cursor + dur));
        cursor += dur;
    }
    if out
        .iter()
        .any(|&(start, end)| !start.is_finite() || !end.is_finite())
    {
        return base.to_vec();
    }
    if let Some(last) = out.last_mut() {
        // 浮点累积仍有 1 ulp 级残差；末尾必须严格等于目标值，否则导出后末句与
        // 画面差几毫秒，逐帧对不上。但钉死不能把末条钉成倒挂/零长：累积残差恰好
        // 让 `last.0` 越过目标值时，仍要保住 `MIN_EDIT_DUR` 的最短时长。
        last.1 = target_secs.max(last.0 + MIN_EDIT_DUR);
    }
    out
}

/// 把「原区间 → 新区间」压成一份带统计的报告。
fn finish(base: &[(f64, f64)], times: Vec<(f64, f64)>) -> RetimePlan {
    let changed = base
        .iter()
        .zip(&times)
        .filter(|(old, new)| (new.0 - old.0).abs() > CHANGED_TOL)
        .count();
    RetimePlan {
        overlaps: has_overlaps(&times),
        degenerate: has_degenerate(&times),
        times,
        changed,
    }
}

/// 排序后相邻两条是否有交叠（用 `start < prev.end - 1e-9` 判定，容忍浮点毛刺）。
fn has_overlaps(times: &[(f64, f64)]) -> bool {
    if times.len() < 2 {
        return false;
    }
    let mut sorted: Vec<(f64, f64)> = times.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    sorted
        .windows(2)
        .any(|pair| pair[1].0 < pair[0].1 - OVERLAP_TOL)
}

/// 是否存在时长不足 [`MIN_EDIT_DUR`] 的片段（含零长与倒挂）。
fn has_degenerate(times: &[(f64, f64)]) -> bool {
    times.iter().any(|&(start, end)| {
        // 非有限时间本身就是坏值（NaN 的比较恒为 false，不显式判就会漏报），
        // 与「时长不足」一起算作退化，界面才会如实警告。
        !start.is_finite() || !end.is_finite() || end - start < MIN_EDIT_DUR - OVERLAP_TOL
    })
}

/// 阅读速度上限的**默认值**：汉字类字幕的「舒适上限」9 字符/秒。
///
/// 与统计面板同源（`subtitle::stats` 的舒适阈值：表意文字 9、拉丁文字 17），
/// `default_matches_stats_thresholds` 这条单测把两个数字钉在一起——界面文案里的
/// 「超过 9 字/秒」必须与判定用的数字是同一个，否则提示和判定会当场互相打脸。
pub const DEFAULT_MAX_CPS: f64 = 9.0;

/// 阅读速度超限的字幕下标（`cps = 可见字符数 / 时长`，严格大于上限即入列）。
///
/// - **可见字符数**走与统计面板同一个口径（[`stats::visible_char_count`]：去空白、含标点）。
///   两处各数一遍，就会出现「统计卡说 3 句过快、复核清单说 0 句」的自相矛盾。
/// - `max_cps` 非有限或 `<= 0` 时按**字符集分档**回落（汉字类 9、拉丁类 17），
///   与 [`plan_min_gaps`] 对非法 `min_gap` 的处理同一思路：宁可回落到有依据的默认值，
///   也不要拿 `NaN` 去比较——NaN 的比较恒为 false，会把整份清单**静默清空**。
/// - **零长 / 倒挂**（`end <= start`）但文本非空的句子也入列：它不是「读得快」而是
///   「根本读不到」，正是最该进复核清单的一类。统计面板跳过它是不想让 `inf`/`NaN`
///   污染平均值，两个口径的目的不同，这里如实报出来。
/// - 文本为空（去空白后无字符）的句子没有阅读速度可言，不入列。
pub fn cps_violations(segments: &[Segment], max_cps: f64) -> Vec<usize> {
    let explicit = max_cps.is_finite() && max_cps > 0.0;
    let mut out = Vec::new();
    for seg in segments {
        let text = seg.display_text();
        let chars = stats::visible_char_count(text);
        if chars == 0 {
            continue;
        }
        let limit = if explicit {
            max_cps
        } else {
            stats::cps_thresholds(text).0
        };
        let duration = seg.duration();
        if duration <= 0.0 || chars as f64 / duration > limit {
            out.push(seg.index);
        }
    }
    out
}

/// 阅读速度 / 最短时长的**修正计划**：只改时间，绝不改文本。
///
/// 为什么不是直接返回一串新 `(start, end)`：界面要能说清「哪几句被拉长、哪几句实在
/// 拉不动（后一句贴得太近）」，用户才决定是先压后一句、还是把这句台词改短。只给一串
/// 数字，用户看到的是「时间变了但不知道为什么」，也就无从下手。
#[derive(Debug, Clone, PartialEq)]
pub struct CpsFixPlan {
    /// 建议区间，与输入**等长同序**（没被改动的原样带回）。
    pub times: Vec<(f64, f64)>,
    /// 时长真的被延长的句子下标（按 `Segment::index` 升序）。
    pub extended: Vec<usize>,
    /// 需要延长、但没达到目标时长的句子下标（升序）。
    ///
    /// 可能是 [`Self::extended`] 的子集（延了一部分但顶到了下一句），也可能**完全不在**
    /// [`Self::extended`] 里（下一句已经贴上来，一点空间都没有）。空表 = 全都够长了。
    pub unresolved: Vec<usize>,
}

impl CpsFixPlan {
    /// 是否有任何一句的时间需要落盘（界面据此决定「应用」按钮是否可用）。
    pub fn has_changes(&self) -> bool {
        !self.extended.is_empty()
    }
}

/// 规划阅读速度修正：把**时长不够**的句子往后延长到「读得完」所需的最短时间。
///
/// 规则与边界：
/// - 目标时长 `target = max(min_dur, 可见字符数 / 上限)`；上限同样在 `max_cps` 非法时
///   按字符集分档回落（见 [`cps_violations`]）。
/// - `min_dur` 是「一条字幕至少显示多久」的产品下限（短句保底）；非有限或负数回落到
///   [`MIN_EDIT_DUR`]，`0.0` 是**合法值**（表示只要够读完就行，不额外保底）。
/// - **只延长、不缩短**：终点取 `max(原终点, 目标终点)`，因此已经读得完的句子一个字节
///   都不动——「一键修正」把用户手工调好的时间也改一遍，是这类按钮最容易挨骂的地方。
/// - **不与后一句重叠**：终点最多顶到「按起点排序后紧邻的下一句的起点」。顶不住就延长
///   到能到的位置，并把该句记进 [`CpsFixPlan::unresolved`]——如实说「这里差 0.8 秒」，
///   比悄悄给一个仍然读不完的时长有用。
/// - 起点一律不动：动起点会把后续字幕整段推走，那是整轨平移（[`plan_retime`]）的活；
///   两件事混在一起，用户就再也说不清是哪一步改了哪一句。
/// - 起点 / 终点非有限的句子（坏数据）不参与规划，原样带回：给一个 NaN 起点算出的
///   「新终点」仍然是 NaN，改它只会让问题更难查。
pub fn plan_cps_fix(segments: &[Segment], max_cps: f64, min_dur: f64) -> CpsFixPlan {
    let explicit = max_cps.is_finite() && max_cps > 0.0;
    let floor = if min_dur.is_finite() && min_dur >= 0.0 {
        min_dur
    } else {
        MIN_EDIT_DUR
    };

    let mut times: Vec<(f64, f64)> = segments.iter().map(|s| (s.start, s.end)).collect();
    // 按起点排序取「紧邻的下一句」：输入可能乱序（外部导入 / 手工编辑），只按数组顺序
    // 取邻居会凭空给出一堆「空间不够」——用户看到的是一堆假的 unresolved。
    let mut order: Vec<usize> = (0..segments.len()).collect();
    order.sort_by(|&a, &b| {
        times[a]
            .0
            .partial_cmp(&times[b].0)
            .unwrap_or(Ordering::Equal)
    });

    let mut extended = Vec::new();
    let mut unresolved = Vec::new();
    for (rank, &idx) in order.iter().enumerate() {
        let seg = &segments[idx];
        let text = seg.display_text();
        let chars = stats::visible_char_count(text);
        // 空文本没有可读内容：拉长一条空白字幕只会挡住画面。
        if chars == 0 {
            continue;
        }
        let (start, end) = times[idx];
        if !start.is_finite() || !end.is_finite() {
            continue;
        }
        let limit = if explicit {
            max_cps
        } else {
            stats::cps_thresholds(text).0
        };
        let target_end = start + (chars as f64 / limit).max(floor);
        // 倒挂句按零长算：它确实该被拉长，起点不动、终点照样是新的。
        let current_end = end.max(start);
        if target_end <= current_end + CHANGED_TOL {
            continue;
        }
        let ceiling = order
            .get(rank + 1)
            .map(|&next| times[next].0)
            .filter(|next_start| next_start.is_finite())
            .unwrap_or(f64::INFINITY);
        let new_end = target_end.min(ceiling);
        if new_end > current_end + CHANGED_TOL {
            times[idx].1 = new_end;
            extended.push(seg.index);
        }
        if new_end + CHANGED_TOL < target_end {
            unresolved.push(seg.index);
        }
    }
    CpsFixPlan {
        times,
        extended,
        unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条只关心时间的片段；文本内容与本模块无关。
    fn seg(index: usize, start: f64, end: f64) -> Segment {
        Segment::new(index, start, end, "文本")
    }

    /// 浮点近似比较：本模块的期望值都算得出来，只差末位舍入。
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// 正位移：每条整体后移，起点与终点同步平移，时长不变。
    #[test]
    fn shift_all_positive_moves_every_cue() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 2.0, 3.5)];
        let plan = plan_retime(&segs, TimeShift::ShiftAll { delta: 12.0 });
        assert_eq!(plan.times, vec![(12.0, 13.0), (14.0, 15.5)]);
        assert_eq!(plan.changed, 2, "两条起点都变了");
        assert!(!plan.overlaps);
        assert!(!plan.degenerate);
    }

    /// 负位移把首条起点压到 0 以下时：起点夹到 0，终点同步右移，时长不能缩水。
    /// 防的是「-1.0 平移后 (0.2,3.2) 变成 (0.0,2.2)，第一条凭空短 1 秒」。
    #[test]
    fn shift_all_negative_clamps_start_and_keeps_duration() {
        let segs = [seg(1, 0.2, 3.2)];
        let plan = plan_retime(&segs, TimeShift::ShiftAll { delta: -1.0 });
        assert!(close(plan.times[0].0, 0.0), "起点夹到 0");
        assert!(close(plan.times[0].1, 3.0), "终点随起点右移同样的量");
        assert!(
            close(plan.times[0].1 - plan.times[0].0, 3.0),
            "时长仍是 3 秒"
        );
        assert_eq!(plan.changed, 1);
    }

    /// 定点后移：`from_secs` 之前的句子一个字节都不动，之后的才平移。
    /// 若误做成「整轨平移」，插片头之后前面所有句子都会错位。
    #[test]
    fn shift_after_leaves_earlier_cues_untouched() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 10.0, 11.0), seg(3, 30.0, 31.0)];
        let plan = plan_retime(
            &segs,
            TimeShift::ShiftAfter {
                from_secs: 30.0,
                delta: 5.0,
            },
        );
        assert_eq!(plan.times[0], (0.0, 1.0), "片头插入点之前的句子不动");
        assert_eq!(plan.times[1], (10.0, 11.0), "刚好在边界前的也不动");
        assert_eq!(plan.times[2], (35.0, 36.0), "边界上的这一条整体后移");
        assert_eq!(plan.changed, 1);
    }

    /// 缩放把首条起点与末条终点精确映射到目标跨度上（25 分钟轨复用到 22 分钟）。
    #[test]
    fn scale_maps_endpoints_and_last_end_to_target_span() {
        let segs = [seg(1, 0.0, 500.0), seg(2, 500.0, 1500.0)];
        let plan = plan_retime(
            &segs,
            TimeShift::Scale {
                source_span: 1500.0,
                target_span: 1320.0,
            },
        );
        assert!(close(plan.times[0].0, 0.0), "起点 0 映射后仍是 0");
        assert!(close(plan.times[0].1, 440.0), "500 * 1320/1500 = 440");
        assert!(close(plan.times[1].1, 1320.0), "末条终点落到 target_span");
        // 只有第二条的起点动了（第一条起点本就是 0）。
        assert_eq!(plan.changed, 1);
    }

    /// 非法跨度（0 / 负数 / NaN / inf）：原样返回、changed = 0，输出里绝不能有 NaN。
    /// 防的是「一个 NaN 起点让片段从时间轴消失，还顺着 DB 写进去捞不回来」。
    #[test]
    fn scale_with_invalid_span_returns_input_without_nan() {
        let segs = [seg(1, 1.0, 2.0), seg(2, 3.0, 4.0)];
        let bad = [
            TimeShift::Scale {
                source_span: 0.0,
                target_span: 100.0,
            },
            TimeShift::Scale {
                source_span: -5.0,
                target_span: 100.0,
            },
            TimeShift::Scale {
                source_span: 100.0,
                target_span: 0.0,
            },
            TimeShift::Scale {
                source_span: f64::NAN,
                target_span: 100.0,
            },
            TimeShift::Scale {
                source_span: 100.0,
                target_span: f64::INFINITY,
            },
        ];
        for shift in bad {
            let plan = plan_retime(&segs, shift);
            assert_eq!(
                plan.times,
                vec![(1.0, 2.0), (3.0, 4.0)],
                "{shift:?} 原样返回"
            );
            assert_eq!(plan.changed, 0, "{shift:?} 没有起点变化");
            assert!(
                plan.times
                    .iter()
                    .all(|&(s, e)| s.is_finite() && e.is_finite()),
                "{shift:?} 输出必须全部有限"
            );
        }
    }

    /// 铺满：末条终点精确落在 target_secs，且各条时长保持原有比例。
    #[test]
    fn fit_to_duration_ends_exactly_at_target_and_preserves_share() {
        // 原时长 1 : 2 : 1，总 4 秒；铺到 8 秒后应是 2 : 4 : 2。
        let segs = [seg(1, 0.0, 1.0), seg(2, 5.0, 7.0), seg(3, 9.0, 10.0)];
        let plan = plan_retime(&segs, TimeShift::FitToDuration { target_secs: 8.0 });
        assert!(close(plan.times[2].1, 8.0), "末条终点严格等于目标");
        assert!(close(plan.times[0].1 - plan.times[0].0, 2.0));
        assert!(close(plan.times[1].1 - plan.times[1].0, 4.0));
        assert!(close(plan.times[2].1 - plan.times[2].0, 2.0));
        let d0 = plan.times[0].1 - plan.times[0].0;
        let d1 = plan.times[1].1 - plan.times[1].0;
        assert!(close(d1 / d0, 2.0), "第二条仍是第一条的两倍长");
    }

    /// 铺满后时间轴必须单调：每条的起点不早于上一条的终点（不留重叠）。
    #[test]
    fn fit_to_duration_is_monotonic() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 1.0, 1.5), seg(3, 3.0, 6.0)];
        let plan = plan_retime(&segs, TimeShift::FitToDuration { target_secs: 30.0 });
        for pair in plan.times.windows(2) {
            assert!(pair[1].0 >= pair[0].1 - 1e-9, "起点不得早于上一条终点");
        }
        assert!(!plan.overlaps);
        assert!(!plan.degenerate);
    }

    /// 非正目标时长：铺满没有意义，原样返回。
    #[test]
    fn fit_to_duration_with_non_positive_target_is_unchanged() {
        let segs = [seg(1, 0.0, 1.0)];
        let plan = plan_retime(&segs, TimeShift::FitToDuration { target_secs: 0.0 });
        assert_eq!(plan.times, vec![(0.0, 1.0)]);
        assert_eq!(plan.changed, 0);
    }

    /// changed 只认真实位移：1e-7 的抖动在 1e-6 容差内不算改动，1e-5 才算。
    #[test]
    fn changed_counts_only_genuine_start_moves() {
        let segs = [seg(1, 0.0, 1.0)];
        let tiny = plan_retime(&segs, TimeShift::ShiftAll { delta: 1e-7 });
        assert_eq!(tiny.changed, 0, "小于容差的位移不算改");
        let real = plan_retime(&segs, TimeShift::ShiftAll { delta: 1e-5 });
        assert_eq!(real.changed, 1, "超过容差的位移才算改");
    }

    /// 重叠检测：把后一条往前推、与第一条交叉时必须报 overlaps。
    #[test]
    fn overlap_detection_flags_crossed_cues() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 2.0, 3.0)];
        let plan = plan_retime(
            &segs,
            TimeShift::ShiftAfter {
                from_secs: 2.0,
                delta: -1.5,
            },
        );
        assert_eq!(plan.times, vec![(0.0, 1.0), (0.5, 1.5)]);
        assert!(plan.overlaps, "第二条起点压进第一条区间");
    }

    /// 零长检测：大幅缩小比例会把片段压到 MIN_EDIT_DUR 以下，必须报 degenerate。
    #[test]
    fn degenerate_flag_flags_too_short_cues() {
        let segs = [seg(1, 0.0, 1.0)];
        let plan = plan_retime(
            &segs,
            TimeShift::Scale {
                source_span: 100.0,
                target_span: 1.0,
            },
        );
        assert!(close(plan.times[0].1 - plan.times[0].0, 0.01));
        assert!(plan.degenerate, "0.01s 短于 MIN_EDIT_DUR");
        assert!(!plan.overlaps);
    }

    /// resolve_overlaps 的四条不变量（升序、无重叠、每条 >= MIN_EDIT_DUR、start >= 0）
    /// 必须在最恶劣的输入上成立：完全重叠、乱序、end < start、负起点。
    #[test]
    fn resolve_overlaps_invariants_on_adversarial_input() {
        let messy = [
            (10.0, 10.0), // 零长
            (0.0, 5.0),   // 与下一条完全重叠
            (0.0, 5.0),   // 完全重叠的另一条
            (3.0, 1.0),   // end < start 的倒挂区间
            (-4.0, -1.0), // 负起点、负终点
            (2.0, 20.0),  // 跨过一大片的超长条
        ];
        let out = resolve_overlaps(&messy);
        assert_eq!(out.len(), messy.len(), "只重排不改条数");
        for pair in out.windows(2) {
            assert!(pair[0].0 <= pair[1].0 + 1e-9, "起点必须升序");
        }
        for (idx, &(start, end)) in out.iter().enumerate() {
            assert!(start >= 0.0, "第 {idx} 条起点不得为负");
            assert!(
                end - start >= MIN_EDIT_DUR - 1e-9,
                "第 {idx} 条时长不足下限"
            );
            if idx > 0 {
                assert!(start >= out[idx - 1].1 - 1e-9, "第 {idx} 条与前一条重叠");
            }
        }
    }

    /// 空输入不 panic，直接给空序列。
    #[test]
    fn resolve_overlaps_of_empty_is_empty() {
        assert!(resolve_overlaps(&[]).is_empty());
    }

    /// describe 是纯函数：同样的输入永远给同样的中文摘要（界面直接显示这句）。
    #[test]
    fn describe_strings_are_pinned() {
        assert_eq!(describe(TimeShift::ShiftAll { delta: 12.0 }), "平移 +12.0s");
        assert_eq!(
            describe(TimeShift::ShiftAfter {
                from_secs: 30.0,
                delta: 5.0,
            }),
            "从 30.0s 起平移 +5.0s"
        );
        assert_eq!(
            describe(TimeShift::Scale {
                source_span: 1500.0,
                target_span: 1320.0,
            }),
            "缩放 1500.0s → 1320.0s"
        );
        assert_eq!(
            describe(TimeShift::FitToDuration {
                target_secs: 1320.0
            }),
            "铺满 1320.0s"
        );
        assert_eq!(describe(TimeShift::ShiftAll { delta: -3.5 }), "平移 -3.5s");
    }

    /// suggest_fit：10% 带内不提示、带外提示铺满；target <= 0 或空轨一律 None。
    #[test]
    fn suggest_fit_band_and_edge_cases() {
        let segs = [seg(1, 0.0, 100.0)];
        let near = [seg(1, 0.0, 105.0)];
        assert_eq!(suggest_fit(&near, 100.0), None, "差 5%，带内");
        let edge = [seg(1, 0.0, 110.0)];
        assert_eq!(suggest_fit(&edge, 100.0), None, "恰好 10% 不算「超过」");
        let far = [seg(1, 0.0, 120.0)];
        assert_eq!(
            suggest_fit(&far, 100.0),
            Some(TimeShift::FitToDuration { target_secs: 100.0 }),
            "差 20%，带外"
        );
        let short = [seg(1, 0.0, 80.0)];
        assert_eq!(
            suggest_fit(&short, 100.0),
            Some(TimeShift::FitToDuration { target_secs: 100.0 }),
            "太短同样提示"
        );
        assert_eq!(suggest_fit(&segs, 0.0), None, "非正目标不提示");
        assert_eq!(suggest_fit(&segs, -1.0), None);
        assert_eq!(suggest_fit(&[], 100.0), None, "空轨不提示");
    }

    /// 空输入不 panic：times 空、changed 0、两个标志都 false。
    #[test]
    fn plan_retime_of_empty_track_is_inert() {
        let shifts = [
            TimeShift::ShiftAll { delta: 12.0 },
            TimeShift::ShiftAfter {
                from_secs: 5.0,
                delta: 1.0,
            },
            TimeShift::Scale {
                source_span: 10.0,
                target_span: 20.0,
            },
            TimeShift::FitToDuration { target_secs: 20.0 },
        ];
        for shift in shifts {
            let plan = plan_retime(&[], shift);
            assert!(plan.times.is_empty());
            assert_eq!(plan.changed, 0);
            assert!(!plan.overlaps);
            assert!(!plan.degenerate);
        }
    }
    // ── plan_min_gaps ──

    /// 相贴的两句被推开：后一句起点后移，前一句终点不动。
    #[test]
    fn min_gaps_pushes_next_start_forward() {
        let segs = [seg(1, 0.0, 2.0), seg(2, 2.0, 3.0)];
        let plan = plan_min_gaps(&segs, 0.5);
        assert_eq!(plan.times, vec![(0.0, 2.0), (2.5, 3.0)]);
        assert_eq!(plan.changed, 1, "只有后一句起点变了");
        assert_eq!(plan.shortened, 0, "前一句不该被收短");
        assert!(!plan.unresolved, "2.5 > 2.0 + 0.5 - tol");
    }

    /// 空隙本来就够：一条都不改，`changed` 为 0。
    #[test]
    fn min_gaps_is_noop_when_gaps_already_satisfied() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 2.0, 3.0)];
        let plan = plan_min_gaps(&segs, 0.04);
        assert_eq!(plan.times, vec![(0.0, 1.0), (2.0, 3.0)]);
        assert_eq!(plan.changed, 0);
        assert_eq!(plan.shortened, 0);
        assert!(!plan.unresolved);
    }

    /// 后一句太短、抬起点会压过 `MIN_EDIT_DUR` 时改为收前一句的 end，
    /// 让后一句保住原有终点（与 resolve_overlaps 同一取舍）。
    #[test]
    fn min_gaps_shortens_previous_when_next_is_too_short() {
        let segs = [seg(1, 0.0, 2.0), seg(2, 2.0, 2.05)];
        let plan = plan_min_gaps(&segs, 0.04);
        assert_eq!(plan.times[0].0, 0.0, "前一句起点不动");
        assert_eq!(plan.times[1], (2.0, 2.05), "后一句必须原样保住");
        assert_eq!(plan.shortened, 1, "前一句被收短");
        assert_eq!(plan.changed, 0, "没有起点被抬");
        assert!(!plan.unresolved);
    }

    /// 收不动时就如实上报：不能假装已经解决了。
    #[test]
    fn min_gaps_reports_unresolved_pairs() {
        // 两条都极短且互相压住：抬后一句会短于下限，收前一句也没空间。
        let segs = [seg(1, 0.0, 0.1), seg(2, 0.0, 0.1)];
        let plan = plan_min_gaps(&segs, 0.5);
        assert!(plan.unresolved, "无法满足时必须上报");
        assert_eq!(plan.changed, 0);
        assert_eq!(plan.shortened, 0);
    }

    /// 末句之后没有「下一条」，因此末句的起点永不被本函数改动。
    #[test]
    fn min_gaps_never_moves_the_last_cue() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 5.0, 6.0)];
        let plan = plan_min_gaps(&segs, 0.04);
        assert_eq!(plan.times[1], (5.0, 6.0), "末句原样");
        assert_eq!(plan.times[0], (0.0, 1.0));
    }

    /// 乱序输入：按 start 排序后处理，输出仍与输入等长、同序存放。
    #[test]
    fn min_gaps_sorts_before_planning() {
        let segs = [seg(1, 10.0, 11.0), seg(2, 0.0, 1.0), seg(3, 1.0, 2.0)];
        let plan = plan_min_gaps(&segs, 0.5);
        assert_eq!(plan.times.len(), 3);
        // 原顺序的第 2 条（0~1）与第 3 条（1~2）之间被插入 0.5s 空隙
        assert_eq!(plan.times[1], (0.0, 1.0));
        assert_eq!(plan.times[2], (1.5, 2.0));
        assert_eq!(plan.changed, 1);
    }

    /// 非法间隔值必须安全回落，绝不能把 NaN 写进时间轴。
    #[test]
    fn min_gaps_rejects_nan_and_negative_targets() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 1.0, 2.0)];
        for bad in [f64::NAN, -1.0, 0.0, f64::INFINITY] {
            let plan = plan_min_gaps(&segs, bad);
            assert_eq!(plan.times.len(), 2);
            for &(start, end) in &plan.times {
                assert!(start.is_finite() && end.is_finite(), "{bad} 产出非有限时间");
            }
        }
        // 负值/0/NaN 一律按默认间隔处理：0 表示「不许负空隙」，与 0.04 不同。
        let zero = plan_min_gaps(&segs, 0.0);
        assert_eq!(
            zero.times,
            vec![(0.0, 1.0), (1.0, 2.0)],
            "0 不应推开任何东西"
        );
    }

    /// 空轨与单条轨都是恒等变换。
    #[test]
    fn min_gaps_of_short_tracks_is_inert() {
        assert!(plan_min_gaps(&[], 0.5).times.is_empty());
        let one = [seg(1, 0.0, 1.0)];
        let plan = plan_min_gaps(&one, 0.5);
        assert_eq!(plan.times, vec![(0.0, 1.0)]);
        assert_eq!(plan.changed, 0);
    }

    /// 不变式：结果中不存在「间隔不足 `gap` 且两侧都够长」的相邻对——
    /// 也就是所有能解决的都被解决了（`unresolved` 为 false 时一个不剩）。
    #[test]
    fn min_gaps_resolves_everything_resolvable() {
        // 全部句子都长于 `MIN_EDIT_DUR`（0.25s），因此每一对都有解。
        // （若混入 0.2s 的鬼影碎片，它自己就低于下限，本函数修不了，
        // 那是 `optimize_segments` 的职责。）
        let segs = [
            seg(1, 0.0, 2.0),
            seg(2, 2.0, 4.0),
            seg(3, 4.0, 5.0),
            seg(4, 5.0, 7.0),
            seg(5, 7.0, 9.0),
        ];
        let gap = 0.3;
        let plan = plan_min_gaps(&segs, gap);
        assert!(!plan.unresolved, "这些句子都够长，应能全部解决");
        for pair in plan.times.windows(2) {
            assert!(
                pair[1].0 >= pair[0].1 + gap - 1e-9,
                "相邻空隙不足: {:?}",
                pair
            );
        }
        // 每条都不该被压到 MIN_EDIT_DUR 以下
        for &(start, end) in &plan.times {
            assert!(
                end - start >= MIN_EDIT_DUR - 1e-9,
                "片段被压过短: {start}..{end}"
            );
        }
    }

    /// 收短前一句也有下界：不能为凑空隙把一条几秒的句子压成零点几秒
    /// （那样下次载入会被鬼影判据删掉）。无解时如实上报 `unresolved`。
    #[test]
    fn min_gaps_does_not_crush_a_long_previous_cue() {
        let segs = [seg(1, 5.0, 12.0), seg(2, 5.0, 5.3)];
        let plan = plan_min_gaps(&segs, 0.04);
        assert_eq!(plan.times[0], (5.0, 12.0), "7 秒的句子不能被压成 0.05 秒");
        assert_eq!(plan.shortened, 0, "没有哪一条该被收短");
        assert!(plan.unresolved, "无解时必须如实上报");
    }

    /// 缩放比值溢出（有限输入、有限跨度）时不能产出 inf/NaN，应原样返回。
    #[test]
    fn scale_ratio_overflow_is_inert() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 5.0, 7.0)];
        let plan = plan_retime(
            &segs,
            TimeShift::Scale {
                source_span: 1e-300,
                target_span: 1e300,
            },
        );
        assert_eq!(plan.times, vec![(0.0, 1.0), (5.0, 7.0)], "溢出时应原样返回");
        assert_eq!(plan.changed, 0);
        assert!(!plan.degenerate, "回退后的时间必须仍是合法的");
    }

    /// 铺满的分母是次正规极小值时比值会溢出成 inf；此时必须原样返回，
    /// 绝不让整条轨变成 inf。
    #[test]
    fn fit_with_subnormal_total_is_inert() {
        let base = [seg(1, 0.0, 5e-324)];
        let plan = plan_retime(&base, TimeShift::FitToDuration { target_secs: 1e300 });
        assert_eq!(plan.times, vec![(0.0, 5e-324)], "溢出时应原样返回");
        assert!(plan
            .times
            .iter()
            .all(|&(s, e)| s.is_finite() && e.is_finite()));
    }

    /// 铺满后末条终点等于目标值，且永远不短于 `MIN_EDIT_DUR`（不能倒挂）。
    #[test]
    fn fit_to_duration_never_ends_before_it_starts() {
        let segs = [seg(1, 0.0, 1.0), seg(2, 1.0, 2.0)];
        let plan = plan_retime(&segs, TimeShift::FitToDuration { target_secs: 10.0 });
        let last = *plan.times.last().unwrap();
        assert!((last.1 - 10.0).abs() < 1e-9, "末条终点钉在目标值");
        assert!(last.1 - last.0 >= MIN_EDIT_DUR, "末条不得倒挂: {last:?}");
    }

    /// 有限位移撞上 f64 极值会溢出成 inf：该条必须回退原区间，而不是写进 inf。
    #[test]
    fn shift_overflow_falls_back_to_original_input() {
        let big = [seg(1, 1.0, f64::MAX)];
        let plan = plan_retime(&big, TimeShift::ShiftAll { delta: f64::MAX });
        assert_eq!(plan.times[0], (1.0, f64::MAX), "溢出成 inf 时回退原区间");

        let tiny = [seg(1, -1e308, -1e308 + 1.0)];
        let plan = plan_retime(
            &tiny,
            TimeShift::ShiftAfter {
                from_secs: -1e308,
                delta: -1e308,
            },
        );
        assert!(
            plan.times[0].0.is_finite() && plan.times[0].1.is_finite(),
            "下溢成 -inf/NaN 时必须回退: {:?}",
            plan.times[0]
        );
    }

    /// 输入自带 NaN / inf 时，`degenerate` 必须如实置位（NaN 比较恒 false，
    /// 不显式判定就会静默漏报，界面看不到任何警告）。
    #[test]
    fn non_finite_input_is_reported_degenerate() {
        let nan_start = [seg(1, f64::NAN, 1.0)];
        let plan = plan_retime(&nan_start, TimeShift::ShiftAll { delta: 0.5 });
        assert!(plan.degenerate, "NaN 起点必须被标记为退化");

        let inf_end = [seg(1, 0.0, f64::INFINITY)];
        let plan = plan_retime(&inf_end, TimeShift::ShiftAll { delta: 1.0 });
        assert!(plan.degenerate, "inf 终点必须被标记为退化");
    }
}

#[cfg(test)]
mod cps_tests {
    // 刻意不用 `use super::*`：只依赖本模块的公开接口与统计模块的公开口径，
    // 实现细节改名不会让测试「跟着一起改」，也就不会掩盖真正的行为变化。
    use super::{cps_violations, plan_cps_fix, DEFAULT_MAX_CPS};
    use crate::subtitle::segment::Segment;
    use crate::subtitle::stats;

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// 默认上限必须与统计面板的舒适阈值是同一个数字：界面文案说「超过 9 字/秒」，
    /// 判定就得按 9 判，两处各写一份迟早会打架。
    #[test]
    fn default_matches_stats_thresholds() {
        assert!(close(DEFAULT_MAX_CPS, stats::cps_thresholds("中文").0));
        assert_eq!(stats::cps_thresholds("中文").0, 9.0);
        assert_eq!(stats::cps_thresholds("latin").0, 17.0);
    }

    /// 超限的进清单、够读的不进；边界（恰好等于上限）算合格。
    #[test]
    fn cps_violations_flags_only_over_limit() {
        let slow = seg(1, 0.0, 1.0, "你好世"); // 3 字 / 1 秒 = 3 CPS
        let fast = seg(2, 1.0, 1.2, "你好世界"); // 4 字 / 0.2 秒 = 20 CPS
        let exact = seg(3, 2.0, 2.0 + 0.5, "你好"); // 2 字 / 0.5 秒 = 4 CPS
        assert_eq!(
            cps_violations(&[slow, fast, exact], DEFAULT_MAX_CPS),
            vec![2]
        );
        assert_eq!(
            cps_violations(&[seg(9, 0.0, 1.0, "九天玄女")], 4.0),
            Vec::<usize>::new(),
            "4 字 / 1 秒 = 4 CPS，恰好等于上限不算超限"
        );
    }

    /// 零长句算超限（读不到），空文本不算（没有阅读速度可言），倒挂同理。
    #[test]
    fn cps_violations_treats_zero_duration_as_violation_and_skips_empty_text() {
        let zero = seg(1, 1.0, 1.0, "你好");
        let empty = seg(2, 2.0, 2.0, "   ");
        let inverted = seg(3, 3.2, 3.0, "倒挂句");
        assert_eq!(
            cps_violations(&[zero, empty, inverted], DEFAULT_MAX_CPS),
            vec![1, 3]
        );
    }

    /// 上限非法（NaN / 0 / 负数）时按字符集分档回落：拉丁文字不会因为回落到汉字阈值
    /// 而被误报。
    #[test]
    fn cps_violations_falls_back_to_script_threshold_for_invalid_limit() {
        let latin = seg(1, 0.0, 0.3, "abcd"); // 4 字 / 0.3 秒 ≈ 13.3 CPS
        assert_eq!(
            cps_violations(std::slice::from_ref(&latin), f64::NAN),
            Vec::<usize>::new(),
            "拉丁档上限 17 → 13.3 不算超限"
        );
        assert_eq!(
            cps_violations(std::slice::from_ref(&latin), 0.0),
            Vec::<usize>::new(),
            "0 视为「用默认」，不是「上限为 0」"
        );
        assert_eq!(cps_violations(&[latin], 9.0), vec![1], "显式给 9 就按 9 判");
    }

    /// 有空间时把终点延到「读得完」的目标时长，起点与其它字段一律不动。
    #[test]
    fn plan_cps_fix_extends_end_to_reach_target() {
        let segs = [seg(1, 0.0, 0.2, "你好世界")]; // 目标 4 / 9 ≈ 0.4444 秒
        let plan = plan_cps_fix(&segs, DEFAULT_MAX_CPS, 0.0);
        assert_eq!(plan.extended, vec![1]);
        assert!(plan.unresolved.is_empty());
        assert!(plan.has_changes());
        assert!(close(plan.times[0].0, 0.0), "起点不动");
        assert!(close(plan.times[0].1, 4.0 / 9.0));
    }

    /// 后一句贴得太近时只延到它跟前，并如实报告 unresolved。
    #[test]
    fn plan_cps_fix_stops_at_next_start_and_reports_unresolved() {
        let segs = [seg(1, 0.0, 0.2, "你好世界"), seg(2, 0.3, 1.0, "再见")];
        let plan = plan_cps_fix(&segs, DEFAULT_MAX_CPS, 0.0);
        assert_eq!(plan.extended, vec![1]);
        assert_eq!(plan.unresolved, vec![1], "顶到了下一句，没能达标");
        assert!(close(plan.times[0].1, 0.3), "终点停在下一句起点");
        assert!(close(plan.times[1].1, 1.0), "下一句不动");
    }

    /// 只延长、不缩短；`min_dur` 是短句保底，非法时回落到最短编辑时长。
    #[test]
    fn plan_cps_fix_never_shortens_and_honours_min_dur() {
        // 短句保底真的生效：0.2 秒的句子被延到 1.0 秒。
        let short = [seg(1, 0.0, 0.2, "你")];
        let plan = plan_cps_fix(&short, DEFAULT_MAX_CPS, 1.0);
        assert_eq!(plan.extended, vec![1]);
        assert!(close(plan.times[0].1, 1.0), "延到保底 1 秒");

        // 已经够读的句子一个字节都不动（只延长、不缩短）。
        let calm = [seg(1, 0.0, 2.0, "你好世界")];
        let plan = plan_cps_fix(&calm, DEFAULT_MAX_CPS, 0.0);
        assert!(!plan.has_changes());
        assert!(close(plan.times[0].1, 2.0));

        // min_dur 非法（NaN / 负数）→ 回落到最短编辑时长，不做无限保底。
        let weird = [seg(1, 0.0, 0.2, "你")];
        let plan = plan_cps_fix(&weird, DEFAULT_MAX_CPS, f64::NAN);
        assert!(close(plan.times[0].1, 0.25), "0.25 是最短编辑时长");
    }

    /// 非有限时间的句子不参与规划，原样带回：给它算出一个 NaN 终点只会更难查。
    #[test]
    fn plan_cps_fix_leaves_non_finite_times_untouched() {
        let segs = [
            seg(1, f64::NAN, 1.0, "你好世界"),
            seg(2, 2.0, 2.1, "你好世界"),
        ];
        let plan = plan_cps_fix(&segs, DEFAULT_MAX_CPS, 0.0);
        assert!(plan.times[0].0.is_nan(), "坏数据原样带回");
        assert_eq!(plan.extended, vec![2]);
        assert!(plan.unresolved.is_empty());
        assert!(
            close(plan.times[1].1, 2.0 + 4.0 / 9.0),
            "正常句照常延长（目标时长从起点算）"
        );
    }
}
