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
//! - 默认「字面量」而非正则：`find` 里的 `\d`、`(` 一律按普通字符处理；只有显式
//!   打开 `ReplaceOptions::regex` 才交给 `regex` crate，此时 `replace` 里的
//!   `$1`/`${name}` 按捕获组回填；
//! - 正则失控保护：模式编译失败写成可读错误而不是 panic；单次替换总量封顶
//!   [`MAX_REPLACEMENTS`]，避免一条贪婪模式把整轨几千句一次换光；
//! - 保护译文：默认不动，必须显式勾选才改；
//! - 如实报告：字面量下 `find == replace` 不算改动（正则模式除外，回填 `$1` 时
//!   字符串相等也可能改变输出），避免「改了 N 句」的假象。

/// 一次批量替换的选项。
///
/// `Default` 与历史版本**逐字节一致**：字面量、不区分大小写、不动译文、不启用
/// 整词、范围=全部。新增的开关默认一律关闭，因此没有任何调用方需要为升级改动行为。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceOptions {
    /// 匹配时是否区分大小写
    pub case_sensitive: bool,
    /// 是否同时替换译文（`Segment::translation`）
    pub include_translation: bool,
    /// 是否把 `find` 当作正则（`regex` crate 语法，`replace` 里 `$1`/`${name}` 回填）
    pub regex: bool,
    /// 是否只匹配「整词」：正则模式加 `\b` 包裹，字面量模式按 Unicode 词边界判定
    pub whole_word: bool,
    /// 作用范围：全部句子，或仅选中那一句
    pub scope: ReplaceScope,
}

impl Default for ReplaceOptions {
    /// 默认：不区分大小写、**不**动译文、字面量、不整词、范围全部。
    ///
    /// 为什么默认不区分大小写：用户找「transformer」时并不想漏掉句首大写的
    /// 「Transformer」——那正是最常见的误漏。
    /// 为什么默认不动译文：机翻结果被一次「全部替换」悄悄改掉，用户很难发现，
    /// 而译文往往是他已经逐句校对过的成果。要改必须是显式勾选。
    /// 为什么默认字面量：历史行为就是字面量，「正则」是高级开关不能默认接管，
    /// 否则用户输入的 `(`、`*` 会被当元字符，轻则零命中、重则整轨乱改。
    fn default() -> Self {
        ReplaceOptions {
            case_sensitive: false,
            include_translation: false,
            regex: false,
            whole_word: false,
            scope: ReplaceScope::All,
        }
    }
}

/// 批量替换的作用范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplaceScope {
    /// 处理全部片段
    #[default]
    All,
    /// 只处理 `Segment::index` 等于该值的片段（界面上的「仅当前句」）。
    ///
    /// 携带的是 `Segment::index`（`AppState::selected_segment_index` 存的就是它），
    /// 而非切片里的位置下标——导入/合并之后切片顺序与 `index` 不保证一致。
    SelectedSentence(usize),
}

/// 单次调用允许的最大替换次数（主文本 + 译文合计）。
///
/// 为什么是 1000：本仓库不引新依赖，因此不做正则真超时；替代手段是给**破坏面**
/// 封顶。一条写坏的模式（例如 `.*` 或会吞掉大半句子的贪婪模式）在几千句的字幕
/// 上一次点下去就能改完整轨，而正常校对里单词级替换极少超过几百处——成片字幕
/// 通常 ≤1500 句，出现最多的词也就几百次。1000 既不会误伤常规批量替换，又能把
/// 一次性失控的破坏面压到可人工复查的量级。达到上限后**停止替换**但继续统计
/// 命中，报告以 [`ReplaceReport::truncated`] 说明，用户再点一次即可处理余下部分。
pub const MAX_REPLACEMENTS: usize = 1000;

/// 替换结果：匹配了多少、实际改了多少、动过哪几句。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceReport {
    /// 被修改的句序号（`Segment::index`，升序去重）
    pub changed: Vec<usize>,
    /// 主文本里**实际替换**的出现次数
    pub text_hits: usize,
    /// 译文里**实际替换**的出现次数
    pub translation_hits: usize,
    /// 匹配到的总处数（含因 [`MAX_REPLACEMENTS`] 未替换的部分）
    pub matched_hits: usize,
    /// 是否因命中上限而停止替换（`matched_hits > total_hits()` 时为真）
    pub truncated: bool,
    /// 正则模式下的编译失败原因；仅在 `regex == true` 且模式非法时非空
    pub regex_error: Option<String>,
}

impl ReplaceReport {
    /// 主文本与译文**实际替换**次数之和。
    pub fn total_hits(&self) -> usize {
        self.text_hits + self.translation_hits
    }

    /// 匹配到但未替换的处数（只可能因为命中上限）。
    pub fn skipped_hits(&self) -> usize {
        self.matched_hits.saturating_sub(self.total_hits())
    }

    /// 是否一处都没改（以命中次数为准；无命中时 `changed` 必为空）。
    ///
    /// 注意：正则编译失败同样是「没改」，但那种情况要读 [`Self::regex_error`]
    /// 才能给用户正确提示，调用方应先查错误再查本方法。
    pub fn is_empty(&self) -> bool {
        self.total_hits() == 0
    }
}

/// 在 `segments` 上原地做批量查找/替换，返回报告。
///
/// - 默认**字面量**替换，不是正则：`find` 里的 `\d`、`(` 等都不会被解释。只有
///   `options.regex == true` 才走 `regex` crate，此时 `replace` 里的
///   `$1`/`${name}` 按捕获组回填。
/// - `find` 为空 → 直接返回空报告（绝不把「替换空串」变成「在每个字符间插东西」；
///   正则模式下 `""` 同样在每个位置命中，一律挡在编译之前）。
/// - 字面量下 `find == replace` → 视为无操作，不计入报告（否则用户会看到
///   「改了 200 句」而实际内容一字未变，然后怀疑程序坏了）。正则模式不套这条：
///   `replace` 里可能有 `$1`，字符串相等并不代表输出不变。
/// - 大小写不敏感时，替换**保留原文的大小写形态**是不可行的（没有变形信息），
///   因此统一写入 `replace` 原样；这条要在文档里明说。
/// - 主文本用 `Segment::display_text()` 的落点写回**同一个字段**：若 `polished`
///   非空（用户看到的、引擎产出的都是它），就改 `polished`；否则改 `text`。
///   绝不能既改 `text` 又改 `polished`——那样导出会取 `polished`、界面某处取
///   `text`，同一句话出现两个版本。
/// - 正则编译失败**不 panic**：原因写进 [`ReplaceReport::regex_error`] 后原样返回，
///   界面据此提示用户改模式。
/// - 单次替换总量封顶 [`MAX_REPLACEMENTS`]：到顶后停止替换但继续统计命中，报告以
///   [`ReplaceReport::truncated`] 说明，防止一条贪婪模式一次把整轨改光。
pub fn replace_all(
    segments: &mut [crate::subtitle::Segment],
    find: &str,
    replace: &str,
    options: &ReplaceOptions,
) -> ReplaceReport {
    let mut report = ReplaceReport::default();

    // 空查找串是硬禁区：字面量下允许它就会在每个字符边界都算一次命中，「全部替换」
    // 变成往每个字符之间插一段文本；正则模式下 `""` 编译出的模式同样在每个位置
    // 命中。一律当无操作返回，且早于正则编译。
    if find.is_empty() {
        return report;
    }
    // 字面量下 find == replace 时内容一字未变，但按字面统计会得出「改了 N 处」。
    // 用户看到「已替换 200 处」而屏幕没变，第一反应是程序坏了。宁可不报。
    // 正则模式跳过这条：`replace` 可能含 `$1`，字符串相等不代表结果不变。
    if !options.regex && find == replace {
        return report;
    }

    // 正则模式只编译一次，逐句复用。编译失败就把原因交给调用方，绝不 panic——
    // 一条写错的模式应当得到一条可读提示，而不是把整个应用打崩。
    // `whole_word` 用 `\b(?:...)` 包裹：非捕获组避免把用户的 `$1` 编号整体挪位。
    let regex = if options.regex {
        let pattern = if options.whole_word {
            format!(r"\b(?:{find})\b")
        } else {
            find.to_string()
        };
        match regex::RegexBuilder::new(&pattern)
            .case_insensitive(!options.case_sensitive)
            .build()
        {
            Ok(compiled) => Some(compiled),
            Err(err) => {
                report.regex_error = Some(err.to_string());
                return report;
            }
        }
    } else {
        None
    };

    // 范围收口：`SelectedSentence` 携带 `Segment::index`。`All` 时不设限，遍历与
    // 历史版本完全一致。
    let scope_index = match options.scope {
        ReplaceScope::All => None,
        ReplaceScope::SelectedSentence(index) => Some(index),
    };

    // 本次调用的剩余替换名额，主文本与译文共用；见 [`MAX_REPLACEMENTS`]。
    let mut budget = MAX_REPLACEMENTS;

    for seg in segments.iter_mut() {
        if let Some(index) = scope_index {
            if seg.index != index {
                continue;
            }
        }
        let mut changed = false;

        // 主文本落点：与 `Segment::display_text()` 完全一致——`polished` 非空白时
        // 界面和导出取的都是它，替换也必须写回它；否则同一句会出现「导出是新版、
        // 界面某处还是旧版」的两份内容。反之只改 `text`，绝不同时改两个字段。
        let target = if seg.polished.trim().is_empty() {
            &mut seg.text
        } else {
            &mut seg.polished
        };
        let outcome = replace_one(target, find, replace, options, regex.as_ref(), &mut budget);
        report.matched_hits += outcome.matched;
        if outcome.replaced > 0 {
            *target = outcome.text;
            report.text_hits += outcome.replaced;
            changed = true;
        }

        // 译文默认不动：机翻结果被一次「全部替换」悄悄改掉很难被发现，而译文通常
        // 是用户逐句校对过的成果。必须显式勾选才动，且单独计数，好让界面能提示
        // 「其中 N 处在译文里」。
        if options.include_translation {
            if let Some(translation) = seg.translation.as_deref() {
                let outcome = replace_one(
                    translation,
                    find,
                    replace,
                    options,
                    regex.as_ref(),
                    &mut budget,
                );
                report.matched_hits += outcome.matched;
                if outcome.replaced > 0 {
                    seg.translation = Some(outcome.text);
                    report.translation_hits += outcome.replaced;
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
    report.truncated = report.skipped_hits() > 0;

    report
}

/// 单段文本的替换结果。
struct SegmentReplacement {
    /// 替换后的新文本（`replaced == 0` 时为空串，调用方不使用）
    text: String,
    /// 匹配到的处数（含因名额不足而未替换的）
    matched: usize,
    /// 实际替换的处数
    replaced: usize,
}

/// 在单段文本上做一次替换（字面量或正则），按 `budget` 限制实际替换数量。
///
/// `budget` 是本次调用剩余的替换名额：命中数照实统计，但实际替换不超过它。
/// `regex` 由调用方按整批编译一次后传入（`None` 表示字面量模式）——正则绝不在这里
/// 逐句重编译，否则一条复杂模式会在大字幕上编译上千次。本函数不碰全局状态、不 panic。
fn replace_one(
    text: &str,
    find: &str,
    replace: &str,
    options: &ReplaceOptions,
    regex: Option<&regex::Regex>,
    budget: &mut usize,
) -> SegmentReplacement {
    if let Some(re) = regex {
        // 正则模式：`find_iter` 统计命中，`replacen` 按名额真正替换。
        // `replacen` 的替换串是 `&str`，`$1`/`${name}` 会被展开成捕获组内容。
        let matched = re.find_iter(text).count();
        let replaced = matched.min(*budget);
        *budget -= replaced;
        if replaced == 0 {
            return SegmentReplacement {
                text: String::new(),
                matched,
                replaced,
            };
        }
        return SegmentReplacement {
            text: re.replacen(text, replaced, replace).into_owned(),
            matched,
            replaced,
        };
    }

    // 字面量模式：先找出所有区间，再按名额单趟拼接。
    let hits = find_hits(text, find, options.case_sensitive, options.whole_word);
    let matched = hits.len();
    let replaced = matched.min(*budget);
    *budget -= replaced;
    if replaced == 0 {
        return SegmentReplacement {
            text: String::new(),
            matched,
            replaced,
        };
    }

    // 单趟构造：把每处命中之间的原文与替换文本依次推入新 `String`。绝不能用
    // `str::replace` 再对结果循环——那会重新扫描刚插入的内容（find="a"
    // replace="aa" 会无限增长）。
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for &(start, end) in &hits[..replaced] {
        out.push_str(&text[cursor..start]);
        out.push_str(replace);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    SegmentReplacement {
        text: out,
        matched,
        replaced,
    }
}

/// 找出 `needle` 在 `haystack` 里所有**不重叠**的字面量匹配，返回
/// `(起始字节下标, 结束字节下标)` 列表；`whole_word` 为真时只保留整词命中。
///
/// 为什么返回区间而不是只返回起点：大小写不敏感时，命中片段的字节长度未必等于
/// `needle.len()`（`'İ'` 占两个字节，而小写化后的 `"i\u{307}"` 是三个字节），只给
/// 起点就无法安全切出原文，替换会切在字符中间 panic。
fn find_hits(
    haystack: &str,
    needle: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Vec<(usize, usize)> {
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
    } else {
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
    }

    if whole_word {
        hits.retain(|&(start, end)| is_whole_word(haystack, start, end));
    }
    hits
}

/// 「整词」判定：命中片段的**左右两侧**都不是词字符才算整词。
///
/// 词字符口径与 `regex` 的 `\w`（Unicode）对齐：`char::is_alphanumeric() || '_'`。
/// 关键含义：**汉字算词字符**（`char::is_alphanumeric()` 对 CJK 返回 true），因此
/// - `你好` 在 `你好，世界` 里：左边是行首、右边是逗号（非词字符）→ 整词命中；
/// - `你好` 在 `你好世界` 里：右边紧跟 `世`（词字符）→ 不算整词，不命中。
///
/// 也就是说中文不会按字拆词，`whole_word` 只在命中片段恰好被标点/空格/行首行尾
/// 包住时才生效——这与正则模式下 `\b你好\b` 的行为一致（`regex` 同样把汉字视为
/// 词字符）。
fn is_whole_word(text: &str, start: usize, end: usize) -> bool {
    let left_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| !is_word_char(c));
    let right_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
    left_ok && right_ok
}

/// 词字符判定，与 `regex` 的 Unicode `\w` 对齐（字母/数字/下划线，含 CJK 汉字）。
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::{replace_all, ReplaceOptions, ReplaceReport, ReplaceScope, MAX_REPLACEMENTS};
    use crate::subtitle::Segment;

    /// 构造一个只有主文本的片段，省掉每个测试里的样板字段。
    fn seg(index: usize, text: &str) -> Segment {
        Segment::new(index, 0.0, 1.0, text)
    }

    /// 默认值本身就是产品决策，单独钉住：不区分大小写、不动译文，且新开关
    /// （正则 / 整词 / 范围）一律关闭——升级不得改变任何调用方的默认行为。
    #[test]
    fn replace_options_default_protects_translation() {
        let d = ReplaceOptions::default();
        assert!(!d.case_sensitive, "默认必须不区分大小写");
        assert!(!d.include_translation, "默认必须不动译文");
        assert!(!d.regex, "默认必须是字面量，不能悄悄开启正则");
        assert!(!d.whole_word, "默认不启用整词");
        assert_eq!(d.scope, ReplaceScope::All, "默认作用于全部句子");
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
                ..ReplaceOptions::default()
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
                ..ReplaceOptions::default()
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

    /// 默认（`regex == false`）仍是字面量：`\d`、`(` 按普通字符处理，绝不被当元
    /// 字符。这是升级后仍要守住的老契约——正则只在显式打开开关后才生效。
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
                ..ReplaceOptions::default()
            },
        );
        assert_eq!(segs[0].text, "伊斯坦布尔 你好");
        assert_eq!(r.text_hits, 1);
    }

    // ── 正则模式 ──────────────────────────────────────────────────────────

    /// 打开 `regex` 后 `find` 才按模式解释：`\d+` 命中连续数字，而默认字面量模式下
    /// 它只会匹配到字面的“反斜杠 + d + 加号”。
    #[test]
    fn regex_enabled_uses_pattern_semantics() {
        let opts = ReplaceOptions {
            regex: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "abc 123 def 45")];
        let r = replace_all(&mut segs, r"\d+", "#", &opts);

        assert_eq!(segs[0].text, "abc # def #");
        assert_eq!(r.text_hits, 2);
        assert_eq!(r.matched_hits, 2);
        assert!(r.regex_error.is_none());
    }

    /// 捕获组回填：`$2`/`$1` 按编号取值，交换“词-词”的左右两边。
    #[test]
    fn regex_capture_group_backreference_fills_dollar_one() {
        let opts = ReplaceOptions {
            regex: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "foo-bar baz-qux")];
        let r = replace_all(&mut segs, r"(\w+)-(\w+)", "$2-$1", &opts);

        assert_eq!(segs[0].text, "bar-foo qux-baz");
        assert_eq!(r.text_hits, 2);
    }

    /// 正则模式同样尊重大小写开关：默认不敏感能命中句首大写，显式敏感则不能。
    #[test]
    fn regex_respects_case_sensitive_flag() {
        let insensitive = ReplaceOptions {
            regex: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "Cat cat")];
        let r = replace_all(&mut segs, "cat", "dog", &insensitive);
        assert_eq!(segs[0].text, "dog dog");
        assert_eq!(r.text_hits, 2);

        let sensitive = ReplaceOptions {
            regex: true,
            case_sensitive: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "Cat cat")];
        let r = replace_all(&mut segs, "cat", "dog", &sensitive);
        assert_eq!(segs[0].text, "Cat dog");
        assert_eq!(r.text_hits, 1);
    }

    /// 整词 + 正则：模式被 `\b(?:...)\b` 包裹，嵌入在更长单词里的片段不命中。
    #[test]
    fn whole_word_regex_wraps_with_word_boundary() {
        let opts = ReplaceOptions {
            regex: true,
            whole_word: true,
            case_sensitive: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "cat concatenate cat.")];
        let r = replace_all(&mut segs, "cat", "dog", &opts);

        assert_eq!(segs[0].text, "dog concatenate dog.");
        assert_eq!(r.text_hits, 2, "concatenate 里的 cat 不是整词");
    }

    // ── 整词（字面量模式）────────────────────────────────────────────────

    /// 字面量 + 整词：命中片段两侧不能紧邻词字符；标点/空格/行首行尾都算边界。
    #[test]
    fn whole_word_literal_skips_embedded_ascii() {
        let opts = ReplaceOptions {
            whole_word: true,
            case_sensitive: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "cat concatenate a-cat cat")];
        let r = replace_all(&mut segs, "cat", "dog", &opts);

        // "concatenate" 里的 cat 左右都是字母 → 不整词；"a-cat" 的 cat 左侧是 '-'
        // （非词字符）、右侧是行尾 → 整词，命中。
        assert_eq!(segs[0].text, "dog concatenate a-dog dog");
        assert_eq!(r.text_hits, 3);
    }

    /// 中文词边界口径：汉字算词字符（与 `regex` 的 `\w` 一致），因此 `你好` 只在被
    /// 标点/空格/行首行尾包住时才算整词，`你好世界` 里的 `你好` 不会被命中。
    #[test]
    fn whole_word_literal_cjk_uses_unicode_word_chars() {
        let opts = ReplaceOptions {
            whole_word: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "你好世界，你好")];
        let r = replace_all(&mut segs, "你好", "HI", &opts);

        assert_eq!(segs[0].text, "你好世界，HI");
        assert_eq!(
            r.text_hits, 1,
            "`你好世界` 里的 `你好` 右侧是汉字，不算整词"
        );
        assert_eq!(r.matched_hits, 1);
    }

    // ── 范围 ──────────────────────────────────────────────────────────────

    /// 范围=仅当前句：只改 `Segment::index` 命中的那一句，其余不动。
    #[test]
    fn scope_selected_sentence_only_touches_that_index() {
        let opts = ReplaceOptions {
            scope: ReplaceScope::SelectedSentence(2),
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(1, "cat"), seg(2, "cat cat"), seg(3, "cat")];
        let r = replace_all(&mut segs, "cat", "dog", &opts);

        assert_eq!(segs[0].text, "cat", "非选中句不动");
        assert_eq!(segs[1].text, "dog dog");
        assert_eq!(segs[2].text, "cat", "非选中句不动");
        assert_eq!(r.text_hits, 2);
        assert_eq!(r.changed, vec![2]);

        // 选中的 index 不存在于切片里 → 什么都不改（不 panic、不误伤全部）。
        let opts = ReplaceOptions {
            scope: ReplaceScope::SelectedSentence(99),
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(1, "cat"), seg(2, "cat")];
        let r = replace_all(&mut segs, "cat", "dog", &opts);
        assert!(r.is_empty());
        assert_eq!(segs[0].text, "cat");
        assert_eq!(segs[1].text, "cat");
    }

    // ── 安全网 ────────────────────────────────────────────────────────────

    /// 正则编译失败不 panic：报告里给出原因，内容一字不改。
    #[test]
    fn regex_compile_error_is_reported_without_panic() {
        let opts = ReplaceOptions {
            regex: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "a(b(c")];
        let r = replace_all(&mut segs, "(", "[", &opts);

        assert_eq!(segs[0].text, "a(b(c", "非法模式绝不能改动内容");
        assert!(r.is_empty());
        assert!(r.changed.is_empty());
        assert!(
            r.regex_error.is_some(),
            "编译失败必须把原因写进报告以便界面提示"
        );
    }

    /// 命中上限：一次最多替换 [`MAX_REPLACEMENTS`] 处；超出部分不替换但在报告里
    /// 如实标出 `truncated`，命中总数仍然统计到位。
    #[test]
    fn replacement_cap_truncates_and_reports_truncation() {
        let total = MAX_REPLACEMENTS + 250;
        let mut segs = vec![seg(0, &"a".repeat(total))];
        let r = replace_all(&mut segs, "a", "b", &ReplaceOptions::default());

        assert_eq!(r.text_hits, MAX_REPLACEMENTS, "实际替换数封顶");
        assert_eq!(r.matched_hits, total, "命中总数照实统计");
        assert!(r.truncated, "必须标记被上限截断");
        assert_eq!(r.skipped_hits(), 250, "未替换的差额可算出");
        assert_eq!(
            segs[0].text.chars().filter(|&c| c == 'b').count(),
            MAX_REPLACEMENTS
        );
        assert_eq!(segs[0].text.chars().filter(|&c| c == 'a').count(), 250);
    }

    /// 空 pattern 在正则模式下同样安全：不编译、不插入、零命中（绝不“每个位置都命中”）。
    #[test]
    fn empty_pattern_is_safe_in_regex_mode() {
        let opts = ReplaceOptions {
            regex: true,
            ..ReplaceOptions::default()
        };
        let mut segs = vec![seg(0, "abc")];
        let r = replace_all(&mut segs, "", "X", &opts);

        assert_eq!(segs[0].text, "abc");
        assert!(r.is_empty());
        assert!(r.regex_error.is_none(), "空串不是编译错误，是直接无操作");
    }

    /// 大小写不敏感的 Unicode 口径（取舍写清）：
    /// - 逐字符 `to_lowercase()` 折叠，`ß` 与 `ẞ`/原形可互相命中；
    /// - **不做**完整折叠，`ß` 与 `ss` 不互相命中（`straße` 命不中 `STRASSE`）。
    ///   这是刻意的保守口径：宁可漏过 `ß`↔`ss` 这一类异体，也不引入会改变命中
    ///   片段长度的复杂折叠。正则模式的 `case_insensitive` 同样是简单折叠。
    #[test]
    fn case_insensitive_german_sharp_s_folds_conservatively() {
        let mut segs = vec![seg(0, "STRAßE straße STRASSE")];
        let r = replace_all(&mut segs, "straße", "weg", &ReplaceOptions::default());

        assert_eq!(segs[0].text, "weg weg STRASSE");
        assert_eq!(r.text_hits, 2, "ß 与 ss 不做完整折叠，STRASSE 不命中");
    }
}
