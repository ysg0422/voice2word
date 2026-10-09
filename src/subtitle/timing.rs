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

use std::cmp::Ordering;

use crate::subtitle::segment::{Segment, MIN_EDIT_DUR};

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
        .map(|&(start, end)| clamp_negative(start + delta, end + delta))
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
                clamp_negative(start + delta, end + delta)
            } else {
                (start, end)
            }
        })
        .collect()
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
    base.iter()
        .map(|&(start, end)| (start * ratio, end * ratio))
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
    let mut out = Vec::with_capacity(base.len());
    // 累积求和而非「各自乘完再各自定位」：起点就是上一条的终点，逐条累加不会
    // 留下空隙或重叠，末条也只需一次钉死就能与目标严丝合缝。
    let mut cursor = 0.0_f64;
    for &(start, end) in base {
        let dur = (end - start).max(0.0) * ratio;
        out.push((cursor, cursor + dur));
        cursor += dur;
    }
    if let Some(last) = out.last_mut() {
        // 浮点累积仍有 1 ulp 级残差；末尾必须严格等于目标值，否则导出后末句与
        // 画面差几毫秒，逐帧对不上。
        last.1 = target_secs;
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
    times
        .iter()
        .any(|&(start, end)| end - start < MIN_EDIT_DUR - OVERLAP_TOL)
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
}
