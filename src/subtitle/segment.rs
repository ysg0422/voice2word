//! 字幕片段数据结构

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub index: usize,
    /// 起始时间 (秒)
    pub start: f64,
    /// 结束时间 (秒)
    pub end: f64,
    /// 原始转写识别文本 (原声识别内容，直接声音是啥就是啥)
    pub text: String,
    /// 翻译字幕 (其他语言对中文的翻译，无翻译时为 None)
    #[serde(default)]
    pub translation: Option<String>,
    /// LLM 润色文本 (保留字段兼容)
    #[serde(default)]
    pub polished: String,
    /// 语种 (zh, en 等)
    pub language: Option<String>,
    /// 模型对该片段的平均对数置信度 (avg_logprob，越小越不可靠；旧记录/非 Whisper 引擎为 None)
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    /// 仅音频原文字幕
    RawOnly,
    /// 仅翻译字幕 (若无翻译回退原文)
    TranslationOnly,
    /// 双语对照字幕 (第一行中文翻译，第二行音频原文)
    Bilingual,
}

impl Segment {
    pub fn new(index: usize, start: f64, end: f64, text: impl Into<String>) -> Self {
        Self {
            index,
            start,
            end,
            text: text.into(),
            translation: None,
            polished: String::new(),
            language: None,
            confidence: None,
        }
    }

    /// 显示文本：原汁原味的声音识别文本 (直接声音是啥就是啥)
    pub fn display_text(&self) -> &str {
        &self.text
    }

    /// 根据导出模式格式化字幕文本
    pub fn export_text(&self, mode: ExportMode) -> String {
        match mode {
            ExportMode::RawOnly => self.text.clone(),
            ExportMode::TranslationOnly => {
                self.translation.clone().unwrap_or_else(|| self.text.clone())
            }
            ExportMode::Bilingual => {
                if let Some(ref trans) = self.translation {
                    format!("{}\n{}", trans, self.text)
                } else {
                    self.text.clone()
                }
            }
        }
    }

    /// 片段时长 (秒)
    pub fn duration(&self) -> f64 {
        (self.end - self.start).max(0.0)
    }
}

/// 智能优化字幕时间轴与消除鬼影/闪烁：
/// 1. 按起始时间升序排序
/// 2. 剔除无效片段 (duration <= 0.05s) 以及模型幻读重叠鬼影 (< 0.25s 且与后句同时间启动)
/// 3. 消除时间戳倒退与重叠冲突（前句尾部不超出后句头部）
/// 4. 长句自动拆分：超过 6 秒 / 60 字的大句按标点就近均分，时间按字数比例分配
/// 5. 广播级极短句平滑：短句 (< 0.8s) 在间隙允许范围内适当延展，避免 0.1s~0.4s 闪烁过快导致人眼无法阅读
/// 6. 重新编排连续序号 (1, 2, 3...)
pub fn optimize_segments(segments: &mut Vec<Segment>) {
    if segments.is_empty() {
        return;
    }

    // 1. 排序
    segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));

    // 2. 剔除无效时长与同时间戳鬼影片段
    let mut filtered = Vec::with_capacity(segments.len());
    let mut i = 0;
    while i < segments.len() {
        let curr = &segments[i];
        let dur = curr.end - curr.start;
        if dur <= 0.05 || curr.display_text().trim().is_empty() {
            i += 1;
            continue;
        }

        // 检查是否为同时间段重叠的极短鬼影碎片 (例如 100ms 的 "对吧？" 与紧随其后同一毫秒启动的整句)
        if i + 1 < segments.len() {
            let next = &segments[i + 1];
            if dur < 0.25 && (next.start - curr.start).abs() < 0.15 {
                // 属于模型分词切片边界重复，跳过鬼影
                i += 1;
                continue;
            }
        }

        filtered.push(curr.clone());
        i += 1;
    }

    // 3. 消除时间重叠
    for j in 0..filtered.len().saturating_sub(1) {
        if filtered[j].end > filtered[j + 1].start {
            if filtered[j + 1].start > filtered[j].start {
                filtered[j].end = (filtered[j].start + 0.1).max(filtered[j + 1].start);
            } else {
                filtered[j + 1].start = filtered[j].end;
            }
        }
    }

    // 4. 长句自动拆分：转写初始阶段就把 8 秒级大句按标点切短，句长可控且画面字幕不超屏
    let mut filtered = split_long_segments(filtered);

    // 5. 极短句平滑延展 (最低停留 0.8 秒，若有空隙则延长显示，防止字闪)
    const MIN_READABLE_DUR: f64 = 0.8;
    let n = filtered.len();
    for j in 0..n {
        let dur = filtered[j].end - filtered[j].start;
        if dur < MIN_READABLE_DUR {
            let next_start = if j + 1 < n {
                filtered[j + 1].start
            } else {
                filtered[j].start + 2.0
            };
            if next_start > filtered[j].end {
                // 留出 40ms 呼吸空隙给下一句，或延满 0.8s
                let max_target = (next_start - 0.04).max(filtered[j].end);
                let desired = filtered[j].start + MIN_READABLE_DUR;
                filtered[j].end = desired.min(max_target);
            }
        }
    }

    // 6. 重新编排序号
    for (idx, seg) in filtered.iter_mut().enumerate() {
        seg.index = idx + 1;
    }

    *segments = filtered;
}

/// 长句自动拆分阈值：单条字幕最长显示 6 秒 / 60 字
const MAX_SEGMENT_DUR: f64 = 6.0;
const MAX_SEGMENT_CHARS: usize = 60;

/// 可作为长句切分点的中英文标点
fn is_split_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '！' | '？' | '；' | '、' | '：' | '…' | ',' | '.' | '!' | '?' | ';' | ':'
    )
}

/// 在 text 中寻找最接近 ratio 位置的标点切点（切点标点归前段）。
/// 切点强制落在 25%~75% 区间且两侧各保留至少 2 字符，避免切出头尾碎渣；无合理切点返回 None。
fn split_index_at_ratio(text: &str, ratio: f64) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n < 4 {
        return None;
    }
    let target = (n as f64 * ratio.clamp(0.0, 1.0)) as usize;
    let lo = ((n as f64) * 0.25) as usize;
    let hi = ((n as f64) * 0.75).ceil() as usize;
    let mut best: Option<(usize, i64)> = None;
    for (i, &c) in chars.iter().enumerate() {
        if !is_split_punct(c) {
            continue;
        }
        let left_len = i + 1;
        if left_len < 2 || n - left_len < 2 || left_len < lo || left_len > hi {
            continue;
        }
        let dist = (left_len as i64 - target as i64).abs();
        if best.map_or(true, |(_, best_dist)| dist < best_dist) {
            best = Some((left_len, dist));
        }
    }
    best.map(|(cut, _)| cut)
}

/// 递归拆分单条长句：按标点就近均分文本，时间按字数比例分配；
/// 翻译与润色文本按同比例就近标点跟随拆分。无合理标点切点时保留原句。
fn split_segment_recursive(seg: Segment, out: &mut Vec<Segment>) {
    let char_count = seg.text.chars().count();
    if seg.duration() <= MAX_SEGMENT_DUR && char_count <= MAX_SEGMENT_CHARS {
        out.push(seg);
        return;
    }

    let Some(cut) = split_index_at_ratio(&seg.text, 0.5) else {
        out.push(seg);
        return;
    };

    let total = char_count as f64;
    let ratio = cut as f64 / total;
    let split_t = seg.start + (seg.end - seg.start) * ratio;
    let chars: Vec<char> = seg.text.chars().collect();
    let left_text: String = chars[..cut].iter().collect();
    let right_text: String = chars[cut..].iter().collect();

    let (left_trans, right_trans) = match &seg.translation {
        Some(trans) if trans.chars().count() > 8 => match split_index_at_ratio(trans, ratio) {
            Some(tc) => {
                let t: Vec<char> = trans.chars().collect();
                (
                    Some(t[..tc].iter().collect()),
                    Some(t[tc..].iter().collect()),
                )
            }
            None => (Some(trans.clone()), None),
        },
        Some(trans) => (Some(trans.clone()), None),
        None => (None, None),
    };

    let (left_polished, right_polished) = if seg.polished.is_empty() {
        (String::new(), String::new())
    } else {
        match split_index_at_ratio(&seg.polished, ratio) {
            Some(pc) => {
                let p: Vec<char> = seg.polished.chars().collect();
                (p[..pc].iter().collect(), p[pc..].iter().collect())
            }
            None => (seg.polished.clone(), String::new()),
        }
    };

    let left = Segment {
        index: 0,
        start: seg.start,
        end: split_t,
        text: left_text,
        translation: left_trans,
        polished: left_polished,
        language: seg.language.clone(),
        confidence: seg.confidence,
    };
    let right = Segment {
        index: 0,
        start: split_t,
        end: seg.end,
        text: right_text,
        translation: right_trans,
        polished: right_polished,
        language: seg.language,
        confidence: seg.confidence,
    };
    split_segment_recursive(left, out);
    split_segment_recursive(right, out);
}

/// 逐条长句拆分
fn split_long_segments(segments: Vec<Segment>) -> Vec<Segment> {
    let mut out = Vec::with_capacity(segments.len());
    for seg in segments {
        split_segment_recursive(seg, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_optimize_segments_removes_ghost_stub() {
        let mut segs = vec![
            Segment::new(1, 10.0, 12.0, "第一句话"),
            Segment::new(2, 12.0, 12.1, "对吧？"),
            Segment::new(3, 12.0, 15.0, "第二句话完整内容"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[0].text, "第一句话");
        assert_eq!(segs[1].index, 2);
        assert_eq!(segs[1].text, "第二句话完整内容");
    }

    #[test]
    fn test_optimize_segments_smooths_short_duration() {
        let mut segs = vec![
            Segment::new(1, 1.0, 1.3, "短句"),
            Segment::new(2, 5.0, 7.0, "后一句"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2);
        assert!(segs[0].end >= 1.8, "短句应延展至至少 0.8 秒");
    }

    #[test]
    fn test_long_segment_splits_at_punctuation() {
        // 模拟用户遇到的 8.25s 大句：应按标点就近均分为两段
        let mut segs = vec![Segment::new(
            1,
            24.070,
            32.320,
            "所以说我们这个专题就总结出来,就帮助大家,就遇到这种题目,咱们至少呢,能够有一个思路,对吧,至少能知道从哪个地方入手。",
        )];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2, "8.25s 大句应拆为两段");
        for seg in &segs {
            assert!(seg.duration() <= 6.0 + 1e-9, "拆分后单段不得超过 6 秒");
        }
        // 时间必须无缝衔接且单调
        assert!((segs[0].end - segs[1].start).abs() < 1e-9);
        assert_eq!(segs[0].index, 1);
        assert_eq!(segs[1].index, 2);
        // 文本拼接应还原原句（无丢字）
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("所以说我们这个专题就总结出来"));
        assert!(joined.contains("至少能知道从哪个地方入手"));
    }

    #[test]
    fn test_very_long_segment_splits_recursively() {
        let text = "第一点我们来看这个概念的定义,它在课本当中写得非常清楚,然后第二点我们来看它的几何意义,其实就是面积的表达,然后第三点我们来看例题,通过例题巩固一下,最后再总结一下易错点。";
        let mut segs = vec![Segment::new(1, 10.0, 30.0, text)];
        optimize_segments(&mut segs);
        assert!(segs.len() >= 3, "20s 大句应递归拆成至少三段");
        for seg in &segs {
            assert!(seg.duration() <= 6.0 + 1e-9);
        }
        // 全程时间单调不重叠
        for w in segs.windows(2) {
            assert!(w[0].end <= w[1].start + 1e-9);
        }
    }

    #[test]
    fn test_normal_segments_untouched_by_splitter() {
        let mut segs = vec![
            Segment::new(1, 1.0, 4.0, "这是一句正常长度的话,有标点也没关系。"),
            Segment::new(2, 4.0, 5.5, "短句也没事。"),
        ];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 2, "正常句长不应被拆分");
        assert_eq!(segs[0].text, "这是一句正常长度的话,有标点也没关系。");
    }

    #[test]
    fn test_no_punctuation_long_sentence_stays_intact() {
        let text = "这一整句话完全没有出现任何可以作为切分点的标点符号所以保留原样不强行切断";
        let mut segs = vec![Segment::new(1, 0.0, 8.0, text)];
        optimize_segments(&mut segs);
        assert_eq!(segs.len(), 1, "无标点可切时不强行拆分");
        assert_eq!(segs[0].text, text);
    }
}
