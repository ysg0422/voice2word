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
/// 4. 广播级极短句平滑：短句 (< 0.8s) 在间隙允许范围内适当延展，避免 0.1s~0.4s 闪烁过快导致人眼无法阅读
/// 5. 重新编排连续序号 (1, 2, 3...)
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

    // 4. 极短句平滑延展 (最低停留 0.8 秒，若有空隙则延长显示，防止字闪)
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

    // 5. 重新编排序号
    for (idx, seg) in filtered.iter_mut().enumerate() {
        seg.index = idx + 1;
    }

    *segments = filtered;
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
}
