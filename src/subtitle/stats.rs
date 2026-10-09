//! 字幕统计层：把整批 [`Segment`] 与媒体总时长压成一份可直接展示的报表。
//!
//! 编辑器只给「逐句行」和「全局样式面板」两种视角：前者一次只看一句，后者只管字体
//! 描边这类样式。**没有任何地方能让用户一眼看到刚产出的这条字幕整体长什么样**——
//! 有多长、里面有多少是静音、每句的阅读速度是多少、有没有哪一行长到会溢出、总共
//! 多少字（翻译成本估算、TTML/Netflix 交付上限都要看这个数）。今天只能一行行肉眼
//! 去数。本模块就是这块缺失的**纯统计层**：吃 `&[Segment]` + 时长，吐一份报表，
//! 不碰 IO、不碰 UI，方便被任意面板或导出流程复用。
//!
//! 关键约定（为什么这样算，见各条目注释）：
//!
//! - **字符数按 `chars()` 去空白计**，不按字节——按字节会让中文行看起来比实际长
//!   约 3 倍，而空格根本不是交付出去的字符。
//! - **CPS = 字符数 / 句时长**，时长非正的句子（零长、倒挂）直接跳过，绝不让
//!   `inf`/`NaN` 混进统计——一个 `NaN` 会顺着平均值污染整块面板，显示成「NaN」
//!   像程序崩了一样。
//! - **阅读速度分中/拉丁两套阈值**，因为一个汉字的信息量远大于一个字母，同一根线
//!   卡下去必然把中文全判「过快」或把英文全判「舒适」，两头都不准。

use crate::subtitle::Segment;

/// 一条字幕的阅读速度分档（按字符/秒 CPS 与行字长）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingSpeed {
    /// 观众读得完，正常语速。
    Comfortable,
    /// 偏快，还在可接受区间。
    Fast,
    /// 过快，广播平台通常据此拒绝烧录字幕。
    TooFast,
}

impl ReadingSpeed {
    /// 中文/日文等表意文字的舒适上限约 9 CPS，过快阈值 12 CPS。
    /// 拉丁文字舒适约 17 CPS，过快 25 CPS。为什么按字符集分档：CPS 对中文字符
    /// 与对英文字母根本不是一个量纲——一个汉字的信息量远大于一个字母，用同一个
    /// 阈值会把中文全判成「过快」或把英文全判成「舒适」，两头都不准。
    ///
    /// 空文本（去空白后无字符）或非正时长没有速度可言，返回 `None`。
    pub fn classify(text: &str, duration_secs: f64) -> Option<Self> {
        // 显式挡掉 NaN 与 <= 0：NaN 做除数会让 CPS 与后续平均值全变成 NaN。
        if duration_secs <= 0.0 || duration_secs.is_nan() {
            return None;
        }
        let (total, cjk) = count_visible(text);
        if total == 0 {
            return None;
        }
        let cps = total as f64 / duration_secs;
        // 用整数比较表达「CJK 占比 >= 30%」，避开浮点边界与 clippy::float_cmp。
        let (comfortable, fast) = if cjk * 10 >= total * 3 {
            (9.0, 12.0)
        } else {
            (17.0, 25.0)
        };
        Some(if cps <= comfortable {
            ReadingSpeed::Comfortable
        } else if cps <= fast {
            ReadingSpeed::Fast
        } else {
            ReadingSpeed::TooFast
        })
    }

    /// 界面直接展示的中文文案。
    pub fn label(self) -> &'static str {
        match self {
            ReadingSpeed::Comfortable => "舒适",
            ReadingSpeed::Fast => "偏快",
            ReadingSpeed::TooFast => "过快",
        }
    }
}

/// 整批字幕的统计。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubtitleStats {
    /// 片段总数。
    pub segment_count: usize,
    /// 带译文的句数
    pub translated_count: usize,
    /// 原文总字符数（去掉空白后）
    pub total_chars: usize,
    /// 单句最长字符数（去掉空白后）
    pub max_chars: usize,
    /// 最长那句的 `Segment::index`（空表为 None）
    pub longest_index: Option<usize>,
    /// 全部片段时长之和（秒）——注意是各段 `end - start` 之和，段间空白不计入
    pub spoken_secs: f64,
    /// 媒体总时长（秒），取自入参
    pub media_secs: f64,
    /// 语速最快的 Cps 值与该句 `Segment::index`
    pub max_cps: f64,
    /// 语速最快那句的 `Segment::index`。
    pub max_cps_index: Option<usize>,
    /// 被判为「过快」的句数
    pub too_fast_count: usize,
    /// 认为过长的句数（按 `max_chars_per_line` 的 2 倍算）
    pub long_line_count: usize,
}

impl SubtitleStats {
    /// 有效说话时长占比 = spoken_secs / media_secs（media_secs <= 0 时返回 0.0）。
    pub fn spoken_ratio(&self) -> f64 {
        // `> 0.0` 同时挡掉 0、负数与 NaN，避免除出 `inf`/`NaN`。
        if self.media_secs > 0.0 {
            self.spoken_secs / self.media_secs
        } else {
            0.0
        }
    }

    /// 平均语速（CPS，按有效说话时长算；无说话时长返回 0.0）。
    pub fn average_cps(&self) -> f64 {
        if self.spoken_secs > 0.0 {
            self.total_chars as f64 / self.spoken_secs
        } else {
            0.0
        }
    }
}

/// 计算统计。`max_chars_per_line` 来自字幕样式配置，用于判「过长句」。
pub fn compute(segments: &[Segment], media_secs: f64, max_chars_per_line: usize) -> SubtitleStats {
    let mut stats = SubtitleStats {
        segment_count: segments.len(),
        media_secs,
        ..Default::default()
    };
    // 为什么乘 2：一条字幕行允许折成两行显示，单行上限的两倍才是「这句会溢出」的
    // 近似门槛。`max_chars_per_line == 0` 表示样式没给上限，此时一律不计，免得把
    // 整篇都标成长句。
    let long_threshold = max_chars_per_line.saturating_mul(2);

    for seg in segments {
        let text = seg.display_text();
        // 按 `chars()` 去空白计，不按字节：字节数会让中文行看起来比实际长约 3 倍，
        // 而空格不是交付出去的字符，算进去会虚增字长与 CPS。
        let (chars, _cjk) = count_visible(text);
        stats.total_chars += chars;

        if seg.has_translation() {
            stats.translated_count += 1;
        }

        if chars > stats.max_chars {
            stats.max_chars = chars;
            stats.longest_index = Some(seg.index);
        }

        let duration = seg.end - seg.start;
        // 时长非正（零长或倒挂）的句子跳过 CPS：除以 0 会得到 `inf`/`NaN`，
        // 它会顺着平均值污染整份统计并显示成「NaN」，看着像崩了。
        if duration > 0.0 {
            stats.spoken_secs += duration;
            let cps = chars as f64 / duration;
            if cps > stats.max_cps {
                stats.max_cps = cps;
                stats.max_cps_index = Some(seg.index);
            }
        }

        if matches!(
            ReadingSpeed::classify(text, duration),
            Some(ReadingSpeed::TooFast)
        ) {
            stats.too_fast_count += 1;
        }

        if max_chars_per_line > 0 && chars > long_threshold {
            stats.long_line_count += 1;
        }
    }

    stats
}

/// 统计非空白字符总数，以及其中属于 CJK 的字符数。
///
/// 返回二元组而不是只返回总数，是因为 [`ReadingSpeed::classify`] 还要据 CJK 占比
/// 决定用哪套阈值；一次遍历同时拿到两个数，省去重复扫描文本。
fn count_visible(text: &str) -> (usize, usize) {
    let mut total = 0;
    let mut cjk = 0;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        total += 1;
        if is_cjk(c) {
            cjk += 1;
        }
    }
    (total, cjk)
}

/// 是否属于「表意文字」量纲：平假名/片假名、CJK 统一表意、扩展 A、谚文。
///
/// 这四段覆盖中日韩最常用的字符；用 30% 占比而不是「含一个就算」是为了处理双语行
/// 与产品名混排——比如 "hello 中文测试" 里中文占 44%，按中文阈值判才合理，而
/// "hello world 中" 里中文只占 9%，按拉丁阈值判才不会把英文长句误判成过快。
fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x3040..=0x30FF).contains(&cp) // 平假名 / 片假名
        || (0x4E00..=0x9FFF).contains(&cp) // CJK 统一表意
        || (0x3400..=0x4DBF).contains(&cp) // CJK 扩展 A
        || (0xAC00..=0xD7AF).contains(&cp) // 谚文
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 浮点断言统一走这个容差，避免 `clippy::float_cmp`。
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// 空输入：全部取默认值，说话占比为 0 而不是 NaN。
    #[test]
    fn empty_input_yields_defaults() {
        let s = compute(&[], 60.0, 20);
        assert_eq!(s.segment_count, 0);
        assert_eq!(s.translated_count, 0);
        assert_eq!(s.total_chars, 0);
        assert_eq!(s.max_chars, 0);
        assert_eq!(s.too_fast_count, 0);
        assert_eq!(s.long_line_count, 0);
        assert!(close(s.spoken_secs, 0.0));
        assert!(close(s.media_secs, 60.0));
        assert!(close(s.max_cps, 0.0));
        assert_eq!(s.longest_index, None);
        assert_eq!(s.max_cps_index, None);
        assert!(close(s.spoken_ratio(), 0.0));
        assert!(close(s.average_cps(), 0.0));
    }

    /// 基础计数：句数、带译文句数、总字符、最长句及其 index。
    #[test]
    fn basic_counts_and_longest_index() {
        let mut a = Segment::new(1, 0.0, 2.0, "你好世界");
        a.translation = Some("Hello world".to_string());
        let b = Segment::new(2, 3.0, 4.5, "Hello, world!");
        let c = Segment::new(3, 5.0, 6.0, "中");

        let s = compute(&[a, b, c], 10.0, 20);
        assert_eq!(s.segment_count, 3);
        assert_eq!(s.translated_count, 1);
        assert_eq!(s.total_chars, 4 + 12 + 1);
        assert_eq!(s.max_chars, 12);
        assert_eq!(s.longest_index, Some(2));
        assert!(close(s.media_secs, 10.0));
    }

    /// 说话时长只累加各段 `end - start`，段间空白不计入。
    #[test]
    fn spoken_secs_ignores_gaps_between_segments() {
        let a = Segment::new(1, 0.0, 1.0, "甲");
        let b = Segment::new(2, 3.0, 4.0, "乙");
        let s = compute(&[a, b], 10.0, 20);
        assert!(close(s.spoken_secs, 2.0), "中间 2 秒空白不该计入");
        assert!(close(s.spoken_ratio(), 0.2));
    }

    /// 零长 / 倒挂片段不进 CPS，且不会让任何统计量变成 NaN / inf。
    #[test]
    fn non_positive_duration_segment_is_finite_and_excluded_from_cps() {
        let zero = Segment::new(1, 5.0, 5.0, "零长句");
        let inverted = Segment::new(2, 2.0, 1.0, "倒挂句");
        let normal = Segment::new(3, 0.0, 1.0, "正常");

        let s = compute(&[zero, inverted, normal], 5.0, 20);
        assert!(s.spoken_secs.is_finite());
        assert!(s.max_cps.is_finite());
        assert!(s.average_cps().is_finite());
        assert!(close(s.spoken_secs, 1.0), "只有正常句贡献时长");
        assert_eq!(s.max_cps_index, Some(3), "最快句必须来自正时长片段");
        assert!(close(s.max_cps, 2.0), "正常句 2 字 / 1 秒 = 2 CPS");
    }

    /// 同一 CPS 下，中文走 CJK 阈值、拉丁文字走拉丁阈值，结论不同。
    #[test]
    fn cjk_and_latin_use_their_own_thresholds() {
        // 4 汉字 / 0.2 秒 = 20 CPS > 12，中文判「过快」。
        assert_eq!(
            ReadingSpeed::classify("你好世界", 0.2),
            Some(ReadingSpeed::TooFast)
        );
        // 同样 20 CPS 的拉丁文字落在 17..=25 区间，只判「偏快」。
        assert_eq!(
            ReadingSpeed::classify("abcd", 0.2),
            Some(ReadingSpeed::Fast)
        );
        // 舒适档也各按各的阈值。
        assert_eq!(
            ReadingSpeed::classify("你好", 1.0),
            Some(ReadingSpeed::Comfortable)
        );
        assert_eq!(
            ReadingSpeed::classify("hello", 1.0),
            Some(ReadingSpeed::Comfortable)
        );
    }

    /// 空文本或非正时长没有阅读速度可言，返回 None。
    #[test]
    fn classify_is_none_for_empty_text_or_non_positive_duration() {
        assert_eq!(ReadingSpeed::classify("", 1.0), None);
        assert_eq!(ReadingSpeed::classify("   ", 1.0), None, "纯空白不算字符");
        assert_eq!(ReadingSpeed::classify("你好", 0.0), None);
        assert_eq!(ReadingSpeed::classify("你好", -0.5), None);
        assert_eq!(ReadingSpeed::classify("你好", f64::NAN), None);
    }

    /// 混排文本：CJK 占比 >= 30% 走中文阈值，低于则走拉丁阈值。
    #[test]
    fn mixed_line_switches_to_cjk_thresholds_at_thirty_percent() {
        // 5 拉丁 + 4 汉字 = 9 字符，CJK 占 44%，按中文阈值：9 / 0.5 = 18 CPS > 12。
        assert_eq!(
            ReadingSpeed::classify("hello 中文测试", 0.5),
            Some(ReadingSpeed::TooFast)
        );
        // 10 拉丁 + 1 汉字 = 11 字符，CJK 占 9%，按拉丁阈值：11 / 0.55 = 20 CPS。
        assert_eq!(
            ReadingSpeed::classify("hello world 中", 0.55),
            Some(ReadingSpeed::Fast)
        );
    }

    /// 行宽 20 时，只有超过 2 倍（>40 字）的句子才判「过长」。
    #[test]
    fn long_line_count_uses_double_the_configured_width() {
        let ok = Segment::new(1, 0.0, 1.0, "a".repeat(40));
        let long = Segment::new(2, 1.0, 2.0, "b".repeat(41));
        let s = compute(&[ok, long], 5.0, 20);
        assert_eq!(s.long_line_count, 1, "40 字不算，41 字算");
    }

    /// `max_chars_per_line = 0` 视为「不判过长」，再长的行也不计。
    #[test]
    fn zero_max_chars_per_line_counts_no_long_lines() {
        let long = Segment::new(1, 0.0, 1.0, "x".repeat(100));
        let s = compute(&[long], 5.0, 0);
        assert_eq!(s.long_line_count, 0);
    }

    /// 平均语速按有效说话时长算，不受媒体总时长（含空白）稀释。
    #[test]
    fn average_cps_uses_spoken_time_not_media_time() {
        // 2 句各 5 字、各 1 秒 → 说话 2 秒、10 字，平均 5 CPS；媒体 100 秒不稀释。
        let a = Segment::new(1, 0.0, 1.0, "一二三四五");
        let b = Segment::new(2, 2.0, 3.0, "六七八九十");
        let s = compute(&[a, b], 100.0, 20);
        assert!(close(s.average_cps(), 5.0));
        assert!(close(s.spoken_ratio(), 0.02));
    }

    /// `media_secs` 为 0 时说话占比返回 0，而不是除零得到 NaN / inf。
    #[test]
    fn spoken_ratio_is_zero_when_media_is_zero() {
        let a = Segment::new(1, 0.0, 1.0, "你好");
        let s = compute(&[a], 0.0, 20);
        assert!(close(s.spoken_ratio(), 0.0));
        assert!(s.spoken_ratio().is_finite());
    }

    /// `too_fast_count` 只数被判「过快」的句，偏快与舒适都不算。
    #[test]
    fn too_fast_count_counts_only_too_fast_segments() {
        let cjk_too_fast = Segment::new(1, 0.0, 0.2, "你好世界"); // 20 CPS → 过快
        let cjk_calm = Segment::new(2, 1.0, 2.0, "你好世界"); // 4 CPS → 舒适
        let latin_fast = Segment::new(3, 3.0, 3.2, "abcd"); // 20 CPS → 偏快
        let s = compute(&[cjk_too_fast, cjk_calm, latin_fast], 10.0, 20);
        assert_eq!(s.too_fast_count, 1);
    }

    /// 三档各自有稳定的中文文案，供界面直接展示。
    #[test]
    fn reading_speed_labels_are_stable() {
        assert_eq!(ReadingSpeed::Comfortable.label(), "舒适");
        assert_eq!(ReadingSpeed::Fast.label(), "偏快");
        assert_eq!(ReadingSpeed::TooFast.label(), "过快");
    }
}
