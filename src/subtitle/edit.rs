//! 字幕编辑：批量查找/替换的纯文本变换层。
//!
//! 编辑字幕是校对流程的核心动作，但此前每一次文本操作都写死在 `ui/editor.rs`
//! 的内联闭包里——既没法单测，又在每个调用点各抄一份，改一处漏一处。本模块把
//! 这些操作收成纯函数：输入是片段切片、查找/替换串与选项，输出是一份「改了多少、
//! 动了哪几句」的报告；不碰 UI、不读全局状态，因此可以脱离 GPUI 直接断言。
//!
//! 真正容易出错的地方都集中在这里并被测试钉死：
//! - 大小写敏感/不敏感。不敏感时**不能**对整串 `to_lowercase()` 再映射下标：
//!   Unicode 小写化会改变字节长度（`'İ'` U+0130 小写化后是 2 个字符），映射错位
//!   会切在字符中间直接 panic；
//! - 「字面量」而非正则：`find` 里的 `\d`、`(` 一律按普通字符处理；
//! - 保护译文：默认不动，必须显式勾选才改；
//! - 如实报告：`find == replace` 不算改动，避免「改了 N 句」的假象。

/// 一次批量替换的选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceOptions {
    /// 匹配时是否区分大小写
    pub case_sensitive: bool,
    /// 是否同时替换译文（`Segment::translation`）
    pub include_translation: bool,
}

impl Default for ReplaceOptions {
    /// 默认：不区分大小写、**不**动译文。
    ///
    /// 为什么默认不区分大小写：用户找「transformer」时并不想漏掉句首大写的
    /// 「Transformer」——那正是最常见的误漏。
    /// 为什么默认不动译文：机翻结果被一次「全部替换」悄悄改掉，用户很难发现，
    /// 而译文往往是他已经逐句校对过的成果。要改必须是显式勾选。
    fn default() -> Self {
        ReplaceOptions {
            case_sensitive: false,
            include_translation: false,
        }
    }
}

/// 替换结果：改了多少处、动了哪几句。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceReport {
    /// 被修改的句序号（`Segment::index`，升序去重）
    pub changed: Vec<usize>,
    /// 主文本里替换掉的出现次数
    pub text_hits: usize,
    /// 译文里替换掉的出现次数
    pub translation_hits: usize,
}

impl ReplaceReport {
    /// 主文本与译文命中次数之和。
    pub fn total_hits(&self) -> usize {
        self.text_hits + self.translation_hits
    }

    /// 是否一处都没改（以命中次数为准；无命中时 `changed` 必为空）。
    pub fn is_empty(&self) -> bool {
        self.total_hits() == 0
    }
}

/// 在 `segments` 上原地做批量字面量替换，返回报告。
///
/// - **字面量**替换，不是正则：`find` 里的 `\d`、`(` 等都不会被解释。
///   界面要加正则必须另外设计（还要防 ReDoS），不在这一版。
/// - `find` 为空 → 直接返回空报告（绝不把「替换空串」变成「在每个字符间插东西」）。
/// - `find == replace` → 视为无操作，不计入报告（否则用户会看到「改了 200 句」
///   而实际内容一字未变，然后怀疑程序坏了）。
/// - 大小写不敏感时，替换**保留原文的大小写形态**是不可行的（字面量替换没有
///   变形信息），因此统一写入 `replace` 原样；这条要在文档里明说。
/// - 主文本用 `Segment::display_text()` 的落点写回**同一个字段**：若 `polished`
///   非空（用户看到的、引擎产出的都是它），就改 `polished`；否则改 `text`。
///   绝不能既改 `text` 又改 `polished`——那样导出会取 `polished`、界面某处取
///   `text`，同一句话出现两个版本。
pub fn replace_all(
    segments: &mut [crate::subtitle::Segment],
    find: &str,
    replace: &str,
    options: &ReplaceOptions,
) -> ReplaceReport {
    let mut report = ReplaceReport::default();

    // 空查找串是硬禁区：允许它就会在每个字符边界都算一次命中，「全部替换」变成
    // 往每个字符之间插一段文本。直接当无操作返回。
    if find.is_empty() {
        return report;
    }
    // find == replace 时内容一字未变，但按字面统计会得出「改了 N 处」。用户看到
    // 「已替换 200 处」而屏幕没变，第一反应是程序坏了。宁可不报。
    if find == replace {
        return report;
    }

    for seg in segments.iter_mut() {
        let mut changed = false;

        // 主文本落点：与 `Segment::display_text()` 完全一致——`polished` 非空白时
        // 界面和导出取的都是它，替换也必须写回它；否则同一句会出现「导出是新版、
        // 界面某处还是旧版」的两份内容。反之只改 `text`，绝不同时改两个字段。
        if seg.polished.trim().is_empty() {
            let (new_text, hits) = replace_in(&seg.text, find, replace, options.case_sensitive);
            if hits > 0 {
                seg.text = new_text;
                report.text_hits += hits;
                changed = true;
            }
        } else {
            let (new_polished, hits) =
                replace_in(&seg.polished, find, replace, options.case_sensitive);
            if hits > 0 {
                seg.polished = new_polished;
                report.text_hits += hits;
                changed = true;
            }
        }

        // 译文默认不动：机翻结果被一次「全部替换」悄悄改掉很难被发现，而译文通常
        // 是用户逐句校对过的成果。必须显式勾选才动，且单独计数，好让界面能提示
        // 「其中 N 处在译文里」。
        if options.include_translation {
            if let Some(translation) = seg.translation.as_deref() {
                let (new_translation, hits) =
                    replace_in(translation, find, replace, options.case_sensitive);
                if hits > 0 {
                    seg.translation = Some(new_translation);
                    report.translation_hits += hits;
                    changed = true;
                }
            }
        }

        // 同一句命中多少次都只记一次序号：`changed` 是「动了哪几句」，不是命中表。
        if changed {
            report.changed.push(seg.index);
        }
    }

    // 文档承诺 `changed` 是「升序去重」：切片顺序不保证按 `index` 排（导入/合并后
    // 可能乱序），异常数据里也可能出现重复 `index`。统一收口，调用方拿去高亮/跳转
    // 才不会来回跳。
    report.changed.sort_unstable();
    report.changed.dedup();

    report
}

/// 在单段文本上做一次字面量替换，返回（新文本，命中次数）。
fn replace_in(text: &str, find: &str, replace: &str, case_sensitive: bool) -> (String, usize) {
    let ranges = find_hits(text, find, case_sensitive);
    if ranges.is_empty() {
        return (text.to_string(), 0);
    }

    // 单趟构造：把每处命中之间的原文与替换文本依次推入新 `String`。绝不能用
    // `str::replace` 再对结果循环——那会重新扫描刚插入的内容（find="a"
    // replace="aa" 会无限增长）。
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for &(start, end) in &ranges {
        out.push_str(&text[cursor..start]);
        out.push_str(replace);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    (out, ranges.len())
}

/// 找出 `needle` 在 `haystack` 里所有**不重叠**的字面量匹配，返回
/// `(起始字节下标, 结束字节下标)` 列表。
///
/// 为什么返回区间而不是只返回起点：大小写不敏感时，命中片段的字节长度未必等于
/// `needle.len()`（`'İ'` 占两个字节，而小写化后的 `"i\u{307}"` 是三个字节），只给
/// 起点就无法安全切出原文，替换会切在字符中间 panic。
fn find_hits(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    if needle.is_empty() || haystack.is_empty() {
        return hits;
    }

    if case_sensitive {
        // 区分大小写时直接按字节找：`match_indices` 是字面量匹配，`\d`、`(` 等
        // 都不会被当成正则元字符，而且天然不重叠。
        hits.extend(
            haystack
                .match_indices(needle)
                .map(|(start, m)| (start, start + m.len())),
        );
        return hits;
    }

    // 大小写不敏感：逐字符比较。绝不能对整串 `to_lowercase()` 再映射回下标——
    // Unicode 小写化会改变字节长度（`'İ'` U+0130 → "i" + U+0307 两个字符），
    // 映射回去的下标会落到字符中间，随后的切片直接 panic。中文本身没有大小写，
    // 但夹在中间的土耳其语/德语字符会踩中这条。这里只把小写化用在**比较**上：
    // 以 `char_indices()` 给出的字符边界为候选起点，逐字符比较小写化结果，
    // 命中的起点天然是合法的字节下标。
    let hay_chars: Vec<(usize, char)> = haystack.char_indices().collect();
    let needle_lower: Vec<Vec<char>> = needle
        .chars()
        .map(|c| c.to_lowercase().collect::<Vec<char>>())
        .collect();
    let needle_len = needle_lower.len();

    let mut pos = 0usize;
    while pos + needle_len <= hay_chars.len() {
        let matched = hay_chars[pos..pos + needle_len]
            .iter()
            .zip(&needle_lower)
            .all(|((_, hc), nl)| hc.to_lowercase().eq(nl.iter().copied()));
        if matched {
            let start = hay_chars[pos].0;
            let end = hay_chars
                .get(pos + needle_len)
                .map_or(haystack.len(), |(b, _)| *b);
            hits.push((start, end));
            // 不重叠：整段跳过，与区分大小写分支的 `match_indices` 语义保持一致，
            // 否则 "aaa" 找 "aa" 在两分支下会得出不同结果。
            pos += needle_len;
        } else {
            pos += 1;
        }
    }

    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::Segment;

    /// 构造一个只有主文本的片段，省掉每个测试里的样板字段。
    fn seg(index: usize, text: &str) -> Segment {
        Segment::new(index, 0.0, 1.0, text)
    }

    /// 默认值本身就是产品决策，单独钉住：不区分大小写、不动译文。
    #[test]
    fn replace_options_default_protects_translation() {
        let d = ReplaceOptions::default();
        assert!(!d.case_sensitive, "默认必须不区分大小写");
        assert!(!d.include_translation, "默认必须不动译文");
    }

    /// 最简单的字面量替换：命中一句改一句，报告计数与句序号正确。
    #[test]
    fn simple_literal_replace() {
        let mut segs = vec![seg(0, "hello world"), seg(1, "no match here")];
        let report = replace_all(&mut segs, "world", "rust", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "hello rust");
        assert_eq!(segs[1].text, "no match here");
        assert_eq!(report.text_hits, 1);
        assert_eq!(report.translation_hits, 0);
        assert_eq!(report.changed, vec![0]);
        assert_eq!(report.total_hits(), 1);
        assert!(!report.is_empty());
    }

    /// 默认不区分大小写能同时命中句首大写与句中全小写；显式区分大小写时只命中全小写。
    #[test]
    fn case_insensitive_matches_mixed_case_but_sensitive_does_not() {
        let original = "Transformer 很好用，transformer 也行";

        let mut insensitive = vec![seg(0, original)];
        let r = replace_all(
            &mut insensitive,
            "transformer",
            "模型",
            &ReplaceOptions::default(),
        );
        assert_eq!(insensitive[0].text, "模型 很好用，模型 也行");
        assert_eq!(r.text_hits, 2);

        let mut sensitive = vec![seg(0, original)];
        let r = replace_all(
            &mut sensitive,
            "transformer",
            "模型",
            &ReplaceOptions {
                case_sensitive: true,
                include_translation: false,
            },
        );
        assert_eq!(sensitive[0].text, "Transformer 很好用，模型 也行");
        assert_eq!(r.text_hits, 1, "区分大小写时句首大写不该被命中");
    }

    /// 替换结果不会被重新扫描：find="a" replace="aa" 覆盖 "aaa" 恰好 3 处，
    /// 结果长度 6，而不是越换越长。
    #[test]
    fn replacement_text_is_not_rescanned() {
        let mut segs = vec![seg(0, "aaa")];
        let r = replace_all(&mut segs, "a", "aa", &ReplaceOptions::default());

        assert_eq!(r.text_hits, 3);
        assert_eq!(segs[0].text, "aaaaaa");
        assert_eq!(segs[0].text.len(), 6, "绝不能把刚插入的 aa 再扫一遍");
    }

    /// 空查找串是无操作：不插入任何东西，报告为空。
    #[test]
    fn empty_find_is_noop() {
        let mut segs = vec![seg(0, "abc")];
        let r = replace_all(&mut segs, "", "X", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "abc");
        assert!(r.is_empty());
        assert!(r.changed.is_empty());
        assert_eq!(r.text_hits, 0);
    }

    /// find == replace 时内容一字未变，报告必须为空，不能谎报「改了 N 处」。
    #[test]
    fn identical_find_and_replace_reports_nothing() {
        let mut segs = vec![seg(0, "abc abc")];
        let r = replace_all(&mut segs, "abc", "abc", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "abc abc");
        assert!(r.is_empty());
        assert_eq!(r.text_hits, 0);
        assert_eq!(r.translation_hits, 0);
    }

    /// 译文默认不动；显式勾选后才替换，并且译文命中单独计数。
    #[test]
    fn translation_untouched_by_default_and_changed_when_opted_in() {
        let mut s = seg(0, "hello");
        s.translation = Some("你好 Hello".to_string());
        let mut segs = vec![s];
        let r = replace_all(&mut segs, "hello", "hi", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "hi");
        assert_eq!(
            segs[0].translation.as_deref(),
            Some("你好 Hello"),
            "默认绝不能动译文"
        );
        assert_eq!(r.text_hits, 1);
        assert_eq!(r.translation_hits, 0);

        let mut s = seg(0, "hello");
        s.translation = Some("你好 Hello".to_string());
        let mut segs = vec![s];
        let r = replace_all(
            &mut segs,
            "hello",
            "hi",
            &ReplaceOptions {
                case_sensitive: false,
                include_translation: true,
            },
        );

        assert_eq!(segs[0].text, "hi");
        assert_eq!(segs[0].translation.as_deref(), Some("你好 hi"));
        assert_eq!(r.text_hits, 1);
        assert_eq!(r.translation_hits, 1, "译文命中要单独计数");
        assert_eq!(r.total_hits(), 2);
    }

    /// `polished` 非空时只改 `polished`，`text` 一字不动；为空时才改 `text`。
    /// 这是「同一句话不能出现两个版本」的底线。
    #[test]
    fn write_back_targets_display_field_only() {
        let mut polished = seg(0, "raw text");
        polished.polished = "polished text".to_string();
        let mut segs = vec![polished];
        let r = replace_all(&mut segs, "text", "T", &ReplaceOptions::default());

        assert_eq!(segs[0].polished, "polished T", "有润色时写回 polished");
        assert_eq!(segs[0].text, "raw text", "text 绝不能被同时改动");
        assert_eq!(r.text_hits, 1);

        let mut raw = seg(1, "raw text");
        let mut segs = vec![raw.clone()];
        let r = replace_all(&mut segs, "text", "T", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "raw T", "无润色时写回 text");
        assert!(segs[0].polished.is_empty(), "polished 保持空");
        assert_eq!(r.text_hits, 1);

        // 纯空白 polished 视同「没有润色」，与 display_text() 的判定保持一致。
        raw.polished = "   ".to_string();
        let mut segs = vec![raw];
        let _ = replace_all(&mut segs, "text", "T", &ReplaceOptions::default());
        assert_eq!(segs[0].text, "raw T");
        assert_eq!(segs[0].polished, "   ", "空白 polished 不该被写入");
    }

    /// 同一句命中多处只记一次序号；多句时 `changed` 升序、去重。
    #[test]
    fn changed_is_ascending_and_deduped() {
        let mut segs = vec![
            seg(5, "cat cat cat"),
            seg(2, "cat"),
            seg(9, "no match"),
            seg(2, "cat again"),
        ];
        let r = replace_all(&mut segs, "cat", "dog", &ReplaceOptions::default());

        assert_eq!(r.text_hits, 5, "命中次数按出现次数累加");
        assert_eq!(r.changed, vec![2, 5], "升序去重，且 9 号无命中不入列");
    }

    /// 空片段列表：什么都不做，返回默认空报告，不 panic。
    #[test]
    fn empty_segments_slice_is_fine() {
        let mut segs: Vec<Segment> = Vec::new();
        let r = replace_all(&mut segs, "a", "b", &ReplaceOptions::default());

        assert!(r.is_empty());
        assert_eq!(r, ReplaceReport::default());
    }

    /// 字面量而非正则：`\d`、`(` 按普通字符处理，绝不会被当作元字符。
    #[test]
    fn find_is_literal_not_regex() {
        let mut segs = vec![seg(0, r"a\d(b"), seg(1, "123")];
        let r = replace_all(&mut segs, r"\d", "N", &ReplaceOptions::default());

        assert_eq!(segs[0].text, r"aN(b", "\\d 就是反斜杠加字母 d");
        assert_eq!(segs[1].text, "123", "正则语义下 \\d 会命中数字，这里不能");
        assert_eq!(r.text_hits, 1);

        let mut segs = vec![seg(0, "a(b(c")];
        let r = replace_all(&mut segs, "(", "[", &ReplaceOptions::default());
        assert_eq!(segs[0].text, "a[b[c", "括号是字面字符");
        assert_eq!(r.text_hits, 2);
    }

    /// 匹配不重叠："aaaa" 找 "aa" 恰好 2 处（与区分大小写分支的 match_indices 一致）。
    #[test]
    fn matches_do_not_overlap() {
        let mut segs = vec![seg(0, "aaaa")];
        let r = replace_all(&mut segs, "aa", "b", &ReplaceOptions::default());

        assert_eq!(r.text_hits, 2);
        assert_eq!(segs[0].text, "bb");
    }

    /// UTF-8 陷阱：`'İ'`(U+0130) 小写化后是两个字符，朴素地整串 `to_lowercase`
    /// 再把下标映射回去会落到字符中间而 panic。这里必须既不 panic 也不破坏原文。
    #[test]
    fn dotted_capital_i_does_not_panic_or_corrupt_utf8() {
        let original = "İstanbul 你好";
        let mut segs = vec![seg(0, original)];
        let r = replace_all(&mut segs, "istanbul", "X", &ReplaceOptions::default());

        // 逐字符比较下 'İ' 的小写化结果是 "i\u{307}"，与 'i' 不等，故不命中——
        // 这是刻意的保守行为：宁可漏过这一个异体字，也不做会错位的折叠。
        assert!(r.is_empty());
        assert_eq!(segs[0].text, original);
        assert!(
            segs[0].text.contains("你好"),
            "中文部分必须原样存活，不能被切坏"
        );

        // 大小写不敏感路径在中文里夹普通 ASCII 时必须正常命中。
        let mut segs = vec![seg(0, " 你好 WORLD 你好 ")];
        let r = replace_all(&mut segs, "world", "世界", &ReplaceOptions::default());
        assert_eq!(segs[0].text, " 你好 世界 你好 ");
        assert_eq!(r.text_hits, 1);

        // 区分大小写时走字节字面量匹配，含 'İ' 的原文可被精确替换且不越界。
        let mut segs = vec![seg(0, original)];
        let r = replace_all(
            &mut segs,
            "İstanbul",
            "伊斯坦布尔",
            &ReplaceOptions {
                case_sensitive: true,
                include_translation: false,
            },
        );
        assert_eq!(segs[0].text, "伊斯坦布尔 你好");
        assert_eq!(r.text_hits, 1);
    }
}
