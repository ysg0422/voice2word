//! 字幕文本修复层：把「转写完还得手工返一遍」的两类机械劳动做成可预览、可撤销的
//! 一键动作。
//!
//! # 为什么要有这一层
//!
//! 转写页的质检卡片已经能把问题**找出来**（低置信、术语违规、未翻译、空/超短句），
//! 也能一键定位到那一句；但定位之后，用户还是得逐句手改。一支一小时的素材里，
//! 语气词（「嗯」「呃」）、口吃式重复（「我我我」）、听障辅助标记（「[掌声]」）、
//! 多余空格与相邻完全重复的句子加起来常常几百处。这些改动全都满足同一条：
//! 「改了不会更糟、不改一定有人问」，适合交给规则；真正需要人判断的是语义，
//! 不是空格。
//!
//! # 两条自我约束
//!
//! - **只改能解释清楚的**：语气词只删句首、标点之后独立成词的（句尾的「啊」
//!   是语气助词，删了会改语气）；重复只折叠 3 个以上的同字连写、2 字以上的
//!   单元紧挨重复，且在常用叠词表与拟声词表之外的（「谢谢」「刚刚」「哈哈哈」
//!   是正常表达）；听障标记只删已知的非台词描述词，括起来的数字、片名一律保留。
//! - **可预览、可撤销**：[`plan`] 与 [`apply`] 共用同一套变换，界面上显示的
//!   处数就是点下去真正会改的处数；应用前由调用方压一层撤销点，一次 Ctrl+Z
//!   全额退回。
//!
//! # 与相邻模块的分工
//!
//! - [`crate::subtitle::edit`]：用户显式给出的查找/替换串（想改什么由人决定）；
//! - 本模块：内置判据的机械修复（想不想改由人决定，改哪里由规则决定）；
//! - [`crate::subtitle::qc`]：只读的复核表导出。
//!
//! 纯函数、不碰 IO、不碰界面状态：所以每条规则都能脱离 GPUI 直接断言。

use crate::subtitle::Segment;

/// 一条修复规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupRule {
    /// 语气词 / 填充词：识别到的一次停顿，不是台词。
    Filler,
    /// 口吃式重复：同一个字或同一个短单元连着说两遍以上。
    Stutter,
    /// 听障辅助标记：`[掌声]`、`（音乐）`、`♪`、`>>` 这类非台词内容。
    Sdh,
    /// 空白与标点规整：多余空格、汉字之间被塞进的空格、重复逗号。
    Spacing,
}

impl CleanupRule {
    /// 展示与执行顺序：先削标记（标记里可能还包着语气词），再删语气词，再折叠
    /// 重复，最后规整空白标点——空格规整必须放最后，因为前几步的删除会留下
    /// 成对空格的「空洞」，只有最后统一收口才不会留下「好 ，」这种半成品。
    pub const ALL: [CleanupRule; 4] = [
        CleanupRule::Sdh,
        CleanupRule::Filler,
        CleanupRule::Stutter,
        CleanupRule::Spacing,
    ];

    /// 界面上的短名（勾选项与统计共用一份文案）。
    pub fn label(self) -> &'static str {
        match self {
            CleanupRule::Filler => "语气词",
            CleanupRule::Stutter => "重复字词",
            CleanupRule::Sdh => "听障标记",
            CleanupRule::Spacing => "空白标点",
        }
    }

    /// 悬停/说明用的补充文案：让用户知道勾上之后会发生什么。
    pub fn detail(self) -> &'static str {
        match self {
            CleanupRule::Filler => "删句首独立成词的「嗯 / 呃 / 唉」等停顿词",
            CleanupRule::Stutter => "「我我我」「这个这个」折回一份",
            CleanupRule::Sdh => "删「[掌声]」「（音乐）」这类非台词标记",
            CleanupRule::Spacing => "合并多余空格、去掉汉字之间被塞进的空格",
        }
    }

    /// 是否也作用于译文。
    ///
    /// 语气词与口吃是**识别**产物，只对原文成立；译文是模型写出来的句子，删词
    /// 会直接改语义（译者写的「啊」可能是真的感叹）。只有两条**纯格式**规则
    /// （标记与空白标点）对译文也安全。
    pub fn applies_to_translation(self) -> bool {
        matches!(self, CleanupRule::Sdh | CleanupRule::Spacing)
    }
}

/// 规则集合（界面上的勾选状态）。四个 bool 而不是 `Vec`：规则是固定枚举，
/// 用集合类型装会让「哪几条存在」变成运行时才知道的事，界面与测试都要多写一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupRules {
    pub sdh: bool,
    pub filler: bool,
    pub stutter: bool,
    pub spacing: bool,
}

impl Default for CleanupRules {
    fn default() -> Self {
        Self::recommended()
    }
}

impl CleanupRules {
    /// 一条都不勾：`plan` 必然为空（界面据此直接禁用按钮）。
    pub const NONE: Self = Self {
        sdh: false,
        filler: false,
        stutter: false,
        spacing: false,
    };

    /// 默认勾选四条。
    ///
    /// 为什么不保守地默认全关：规则本身的边界（见模块注释）已经把误伤压到
    /// 「抠掉一个停顿词、一个叠字」的量级，而漏做的代价是用户拿几百处去手工
    /// 搜索替换。真正有争议的素材（访谈原文、方言记录）用户会自己关掉——所以
    /// 关键不是默认值保守，而是**每一项都能单独关**，且改动可撤销。
    pub const fn recommended() -> Self {
        Self {
            sdh: true,
            filler: true,
            stutter: true,
            spacing: true,
        }
    }

    pub fn enabled(self, rule: CleanupRule) -> bool {
        match rule {
            CleanupRule::Filler => self.filler,
            CleanupRule::Stutter => self.stutter,
            CleanupRule::Sdh => self.sdh,
            CleanupRule::Spacing => self.spacing,
        }
    }

    pub fn set(&mut self, rule: CleanupRule, on: bool) {
        match rule {
            CleanupRule::Filler => self.filler = on,
            CleanupRule::Stutter => self.stutter = on,
            CleanupRule::Sdh => self.sdh = on,
            CleanupRule::Spacing => self.spacing = on,
        }
    }

    pub fn toggle(&mut self, rule: CleanupRule) {
        let on = !self.enabled(rule);
        self.set(rule, on);
    }

    /// 是否至少勾了一条（全关时界面不显示「可修 N 处」）。
    pub fn any(self) -> bool {
        CleanupRule::ALL.iter().any(|r| self.enabled(*r))
    }

    /// 勾了几条，用于「3/4 条规则」这类文案。
    pub fn enabled_count(self) -> usize {
        CleanupRule::ALL
            .iter()
            .filter(|r| self.enabled(**r))
            .count()
    }
}

/// 命中的位置：原文还是译文。
///
/// 分开记是因为两者待遇不同（见 [`CleanupRule::applies_to_translation`]），
/// 用户看到「译文 2 处」时才知道改的是哪一边——只报一个总数会把译文改动
/// 混进原文里，事后核对翻译记忆库时对不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupTarget {
    Source,
    Translation,
}

impl CleanupTarget {
    pub fn label(self) -> &'static str {
        match self {
            CleanupTarget::Source => "原文",
            CleanupTarget::Translation => "译文",
        }
    }
}

/// 一条改动。`before -> after` 记的是**这一步之后**的结果，因此同一条字幕被多条
/// 规则先后改动时会留下多条记录，每条都能单独解释（「这条为什么变了」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupHit {
    pub rule: CleanupRule,
    pub target: CleanupTarget,
    /// 片段序号（[`Segment::index`]），供界面点一下跳过去看。
    pub index: usize,
    pub before: String,
    pub after: String,
}

impl CleanupHit {
    /// 这一步是否把这一句（或这一边）清空了。整句只剩「嗯。」这类停顿词时会被
    /// 删成空串；字幕条本身不该由本模块删除（删条会改时间轴与序号，属于另一类
    /// 动作），所以计数露出来让用户自己决定取舍。
    pub fn emptied(&self) -> bool {
        self.after.trim().is_empty() && !self.before.trim().is_empty()
    }
}

/// 一次修复计划。`plan` 算出来给界面看，`apply` 按同一套规则落地。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupPlan {
    pub hits: Vec<CleanupHit>,
}

impl CleanupPlan {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hits.len()
    }

    /// 某条规则命中几处（勾选项上显示的数字）。
    pub fn count_of(&self, rule: CleanupRule) -> usize {
        self.hits.iter().filter(|h| h.rule == rule).count()
    }

    /// 会被清空的处数（整句只剩停顿词/标记）。
    pub fn emptied_count(&self) -> usize {
        self.hits.iter().filter(|h| h.emptied()).count()
    }

    /// 受影响的**句子**数（同一句被两条规则改动只算一句）。
    pub fn affected_segments(&self) -> usize {
        let mut seen: Vec<usize> = self.hits.iter().map(|h| h.index).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// 第一条改动，界面拿它显示「例：嗯，我来了 → 我来了」。
    pub fn first(&self) -> Option<&CleanupHit> {
        self.hits.first()
    }

    /// 汇总一行：`语气词 3 · 空白标点 12`。零处的规则不出现——列一堆「0」会让
    /// 用户以为那些规则做了检查却没结果，而不是「这一项本来就没有问题」。
    pub fn summary(&self) -> String {
        let parts: Vec<String> = CleanupRule::ALL
            .iter()
            .filter_map(|rule| {
                let n = self.count_of(*rule);
                (n > 0).then(|| format!("{} {n}", rule.label()))
            })
            .collect();
        parts.join(" · ")
    }
}

/// 单条文本的修复结果（单测与外部预览用；界面走 [`plan`] 拿分规则明细）。
pub fn clean_text(text: &str, rules: CleanupRules) -> String {
    let mut cur = text.to_string();
    for rule in CleanupRule::ALL {
        if rules.enabled(rule) {
            cur = transform(&cur, rule);
        }
    }
    cur
}

/// 只算不改。界面每帧用它显示「可修 N 处」，与 [`apply`] 共用同一套变换，
/// 所以按钮上的数字就是真正会改的处数，不存在「界面估算、落地再算一遍」。
pub fn plan(segments: &[Segment], rules: CleanupRules) -> CleanupPlan {
    let mut out = CleanupPlan::default();
    for seg in segments {
        let (src, trans) = plan_segment(seg, rules);
        if let Some(p) = src {
            out.hits.extend(p.hits);
        }
        if let Some(p) = trans {
            out.hits.extend(p.hits);
        }
    }
    out
}

/// 落地修改。调用方负责压撤销点、落库与刷新界面——本函数只改文本。
pub fn apply(segments: &mut [Segment], rules: CleanupRules) -> CleanupPlan {
    let mut out = CleanupPlan::default();
    for seg in segments.iter_mut() {
        let (src, trans) = plan_segment(seg, rules);
        if let Some(p) = src {
            // 写回 display_text 真正读的那个字段：润色文本非空时界面与导出都看
            // `polished`，改 `text` 等于改了个看不见的地方。
            if seg.polished.trim().is_empty() {
                seg.text = p.final_text.clone();
            } else {
                seg.polished = p.final_text.clone();
            }
            // 词级时间戳是按旧文本切出来的，文本一变就整体错位；清空让逐词高亮
            // 回落到整句高亮，而不是显示一段对不上号的假高亮（与拆分/合并同一
            // 处理方式：宁可没有，不要错的）。
            seg.words.clear();
            out.hits.extend(p.hits);
        }
        if let Some(p) = trans {
            seg.translation = Some(p.final_text.clone());
            out.hits.extend(p.hits);
        }
    }
    out
}

/// 一句（或一边）的试算结果。
struct TargetPlan {
    final_text: String,
    hits: Vec<CleanupHit>,
}

fn plan_segment(seg: &Segment, rules: CleanupRules) -> (Option<TargetPlan>, Option<TargetPlan>) {
    let src = plan_text(seg.index, seg.display_text(), rules, CleanupTarget::Source);
    let trans = seg
        .translation
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .and_then(|t| plan_text(seg.index, t, rules, CleanupTarget::Translation));
    (src, trans)
}

/// 按 [`CleanupRule::ALL`] 的顺序逐步施加，逐步记录命中；没有变化的步骤不记录
/// （有些规则先跑一遍把文本改成了另一条规则也能命中的样子，例如抹掉标记后
/// 留下「嗯，」——命中清单如实反映每一步，界面才能解释「为什么改了两次」）。
fn plan_text(
    index: usize,
    text: &str,
    rules: CleanupRules,
    target: CleanupTarget,
) -> Option<TargetPlan> {
    let mut cur = text.to_string();
    let mut hits: Vec<CleanupHit> = Vec::new();
    for rule in CleanupRule::ALL {
        if !rules.enabled(rule) {
            continue;
        }
        if target == CleanupTarget::Translation && !rule.applies_to_translation() {
            continue;
        }
        let next = transform(&cur, rule);
        if next != cur {
            hits.push(CleanupHit {
                rule,
                target,
                index,
                before: cur.clone(),
                after: next.clone(),
            });
            cur = next;
        }
    }
    if hits.is_empty() {
        return None;
    }
    Some(TargetPlan {
        final_text: cur,
        hits,
    })
}

fn transform(text: &str, rule: CleanupRule) -> String {
    match rule {
        CleanupRule::Filler => strip_fillers(text),
        CleanupRule::Stutter => fold_stutter(text),
        CleanupRule::Sdh => strip_sdh(text),
        CleanupRule::Spacing => normalize_spacing(text),
    }
}

/// 非台词描述的判定词。只列这些：括号里出现别的词（「（2020）」「（片名）」）
/// 一律原样保留——把数字或作品名当成标记删掉，用户会以为字幕丢字，而丢字的
/// 字幕比多一个「[掌声]」严重得多。
const SDH_KEYWORDS: [&str; 26] = [
    "掌声", "鼓掌", "笑声", "大笑", "哭声", "欢呼", "歌声", "音乐", "鼓声", "咳嗽", "叹气", "喘气",
    "脚步", "敲门", "铃声", "电话", "噪音", "杂音", "静音", "沉默", "laughter", "applause",
    "music", "silence", "sighs", "coughs",
];

/// 句首/标点之后独立成词的停顿词。长词排在前面由匹配逻辑按长度择优，不受顺序影响。
const FILLERS: [&str; 24] = [
    "嗯嗯", "呃呃", "啊啊", "哦哦", "emmm", "emm", "umm", "uhm", "hmm", "erm", "uh", "um", "嗯",
    "呃", "唔", "啊", "唉", "哦", "噢", "诶", "欸", "哎", "呀", "嘛",
];

/// 正常叠词：这些字/词重复是表达方式，不是口吃。漏进白名单的代价是「该折的没折」，
/// 错删的代价是「台词被改坏」，所以宁可白名单长一点。
const LEGIT_REDUPLICATION: [&str; 20] = [
    "谢谢", "刚刚", "常常", "天天", "看看", "试试", "慢慢", "好好", "快快", "多多", "轻轻", "静静",
    "明明", "白白", "大大", "小小", "高高", "早早", "各个", "人人",
];

/// 正常连写的拟声词/笑声：把「哈哈哈」折成「哈」是明显的改坏。
const LEGIT_RUN_CHARS: [char; 14] = [
    '哈', '嘿', '嘻', '呵', '呜', '哇', '砰', '咚', '滴', '嗒', '嗡', '呼', '唰', '嘟',
];

/// 单字连写到几次才认定为口吃。
///
/// 取 3：中文里两个同字连写太常见（「刚刚」「看看」），而三个以上同字连写基本
/// 只出现在识别到的口吃或抖动里。
const MIN_STUTTER_RUN: usize = 3;

/// 折叠「相邻重复句」时要求的最短可见字数。
///
/// 取 2：单个字（「对。」「好。」）完全可能就是用户自己的两次回答，删掉一句就
/// 少一层意思；两个以上字完全相同且紧挨着，才是模型在静音边界附近重复出句。
const MIN_DUP_CHARS: usize = 2;

/// 折叠「相邻重复句」时允许的最大间隔（秒）。隔得远的两句多半是副歌、口头禅
/// 这类真的又说了同样的话，不能当重复删掉。
const MAX_DUP_GAP: f64 = 2.0;

/// 删听障辅助标记：先去掉整句级的符号噪声，再删「已知描述词」的括号对。
fn strip_sdh(text: &str) -> String {
    let stage: String = text.chars().filter(|c| !matches!(c, '♪' | '♫')).collect();
    let stage = stage.replace(">>>", "").replace(">>", "");

    let chars: Vec<char> = stage.chars().collect();
    let mut out = String::with_capacity(stage.len());
    let mut i = 0;
    while i < chars.len() {
        let close = match chars[i] {
            '[' => Some(']'),
            '【' => Some('】'),
            '（' => Some('）'),
            '(' => Some(')'),
            _ => None,
        };
        if let Some(close_ch) = close {
            // 找不到配对的右括号就当普通字符处理：宁可留一个孤立的「[」，也不能
            // 把后面整段正文一起吞掉。
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == close_ch) {
                let end = i + 1 + rel;
                let inner: String = chars[i + 1..end].iter().collect();
                if is_sdh_marker(&inner) {
                    i = end + 1;
                    // 顺手吃掉标记后紧跟的一个空格，否则会留下「 我们走吧」开头的空格
                    if chars.get(i) == Some(&' ') {
                        i += 1;
                    }
                    continue;
                }
                for x in &chars[i..=end] {
                    out.push(*x);
                }
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_sdh_marker(inner: &str) -> bool {
    let t = inner.trim().to_lowercase();
    // 空括号「（）」「[]」本身不含信息，留着只是噪声
    if t.is_empty() {
        return true;
    }
    SDH_KEYWORDS.iter().any(|k| t.contains(k))
}

/// 删句首、标点之后的停顿词（连同它后面的停顿标点与空格）。
fn strip_fillers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    // 「当前位置是否处于句首或标点之后」：只有这种位置的停顿词才是语气词。
    let mut at_boundary = true;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            // 空白原样保留（由 Spacing 规则统一收口），也不改变边界性：
            // 「嗯， 我来了」这种空格错位同样该把「嗯」删掉。
            out.push(c);
            i += 1;
            continue;
        }
        if at_boundary {
            let rest: String = chars[i..].iter().collect();
            if let Some(filler) = match_filler(&rest) {
                i += filler.chars().count();
                // 连带的停顿标点一并删：只删字会留下一个孤零零的逗号（「嗯，我来了」
                // → 「，我来了」），那比不改还难看。
                if let Some(&p) = chars.get(i) {
                    if is_pause_punct(p) {
                        i += 1;
                        while matches!(chars.get(i), Some(q) if q.is_whitespace()) {
                            i += 1;
                        }
                    }
                }
                // 连续停顿词（「嗯嗯，那个」）继续按边界处理
                at_boundary = true;
                continue;
            }
        }
        out.push(c);
        at_boundary = is_boundary_after(c);
        i += 1;
    }
    out.into_iter().collect()
}

/// 在 `rest` 的开头匹配停顿词。
///
/// 拉丁词必须**自成词**：`umbrella` 不能被削成 `brella`。中文没有词边界符号，
/// 只靠「句首/标点之后」这一条约束即可。
fn match_filler(rest: &str) -> Option<&'static str> {
    let mut best: Option<&'static str> = None;
    for filler in FILLERS {
        if rest.len() < filler.len() || !rest.is_char_boundary(filler.len()) {
            continue;
        }
        if !rest[..filler.len()].eq_ignore_ascii_case(filler) {
            continue;
        }
        if filler.is_ascii() {
            let after = rest[filler.len()..].chars().next();
            if matches!(after, Some(c) if c.is_alphanumeric() || c == '\'') {
                continue;
            }
        }
        let longer = match best {
            Some(b) => filler.len() > b.len(),
            None => true,
        };
        if longer {
            best = Some(filler);
        }
    }
    best
}

fn is_pause_punct(c: char) -> bool {
    matches!(
        c,
        '，' | ',' | '、' | '。' | '.' | '！' | '!' | '？' | '?' | '…'
    )
}

fn is_boundary_after(c: char) -> bool {
    is_pause_punct(c)
        || matches!(
            c,
            '；' | ';' | '：' | ':' | '—' | '～' | '~' | '\n' | '\r' | '\t' | ' '
        )
}

/// 折叠口吃式重复：先处理中文的同字连写与单元重复，再处理拉丁单词的紧挨重复。
fn fold_stutter(text: &str) -> String {
    let cjk_folded = collapse_cjk_repeats(text);
    collapse_latin_word_repeats(&cjk_folded)
}

fn collapse_cjk_repeats(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut run = 1;
        while i + run < chars.len() && chars[i + run] == c {
            run += 1;
        }
        // 「谢谢谢谢」这种正常叠词写成四遍连写时，整串的字都一样——但它是叠词而不是
        // 口吃。先看开头两字是不是常用叠词，是就整串放过，交给下面的单元重复判断
        // （那里会因为命中白名单而同样不折）。
        let pair: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        if run >= MIN_STUTTER_RUN
            && is_cjk(c)
            && !LEGIT_RUN_CHARS.contains(&c)
            && !is_legit_reduplication(&pair)
        {
            out.push(c);
            i += run;
            continue;
        }

        // 2~4 字的单元紧挨重复：「这个这个」「然后然后」。单个字的单元不走这条
        // （两个同字连写是正常叠词），所以长度从 2 起。
        let mut collapsed = false;
        for unit_len in (2..=4usize).rev() {
            if i + 2 * unit_len > chars.len() {
                continue;
            }
            let unit: String = chars[i..i + unit_len].iter().collect();
            let next: String = chars[i + unit_len..i + 2 * unit_len].iter().collect();
            if unit != next || !unit.chars().any(is_cjk) || is_legit_reduplication(&unit) {
                continue;
            }
            // 连着说三遍以上（「这个这个这个」）一并吃掉
            let mut j = i + unit_len;
            while j + unit_len <= chars.len()
                && chars[j..j + unit_len].iter().collect::<String>() == unit
            {
                j += unit_len;
            }
            out.extend(chars[i..i + unit_len].iter());
            i = j;
            collapsed = true;
            break;
        }
        if collapsed {
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

fn is_legit_reduplication(unit: &str) -> bool {
    LEGIT_REDUPLICATION.contains(&unit)
}

/// 拉丁单词紧挨重复：「the the」→「the」。
///
/// 按「非词段 + 词」交替扫描，非词段（空格与标点）原样带出——本规则只删重复词，
/// 不顺手改排版（空白规整是另一条规则的事，用户可以单独关掉）。
fn collapse_latin_word_repeats(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<String> = None;
    let mut rest = text;
    while !rest.is_empty() {
        let sep_len = match rest.char_indices().find(|(_, c)| is_word_char(*c)) {
            Some((i, _)) => i,
            None => rest.len(),
        };
        out.push_str(&rest[..sep_len]);
        rest = &rest[sep_len..];
        if rest.is_empty() {
            break;
        }
        let word_len = match rest.char_indices().find(|(_, c)| !is_word_char(*c)) {
            Some((i, _)) => i,
            None => rest.len(),
        };
        let word = &rest[..word_len];
        rest = &rest[word_len..];

        let key = word.to_lowercase();
        // 只折拉丁词：中文没有词间分隔，`is_word_char` 会把整段中文当成一个词，
        // 若在这里参与判重，等于把「好的，，好的」错折成「好的，」。中文的单元
        // 重复由 `collapse_cjk_repeats` 负责（那里才分得清叠词与口吃）。
        let duplicate = is_latin_word(word)
            && key.chars().count() >= MIN_DUP_CHARS
            && prev.as_deref() == Some(key.as_str());
        if duplicate {
            // 连它前面的那一段分隔一起收掉，否则「the  the」会留下一个双空格
            while matches!(out.chars().last(), Some(c) if c.is_whitespace()) {
                out.pop();
            }
        } else {
            out.push_str(word);
            prev = is_latin_word(word).then_some(key);
        }
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\''
}

/// 是否拉丁词（含 ASCII 字母且不含汉字）：只对这些词做「紧挨重复」折叠。
fn is_latin_word(word: &str) -> bool {
    word.chars().any(|c| c.is_ascii_alphabetic()) && !word.chars().any(is_cjk)
}

/// 空白与标点规整：空白收敛成一个半角空格并去首尾；标点前不留空格、开括号后不留
/// 空格、汉字之间不留空格；重复的逗号/顿号收敛成一个。
///
/// 为什么不去动「中文与英文之间的空格」：那是**排版风格**，有人要有人不要，属于
/// 用户选择；而「你 好」「好 ，」是识别噪声，任何风格下都不成立。
fn normalize_spacing(text: &str) -> String {
    let mut squeezed = String::with_capacity(text.len());
    let mut prev_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space && !squeezed.is_empty() {
                squeezed.push(' ');
            }
            prev_space = true;
        } else {
            squeezed.push(c);
            prev_space = false;
        }
    }

    let chars: Vec<char> = squeezed.trim().chars().collect();
    let mut out = String::with_capacity(squeezed.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' {
            let prev = out.chars().last();
            let next = chars.get(i + 1).copied();
            if should_drop_space(prev, next) {
                i += 1;
                continue;
            }
        }
        if matches!(c, '，' | ',' | '、')
            && matches!(out.chars().last(), Some(p) if matches!(p, '，' | ',' | '、'))
        {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.trim().to_string()
}

fn should_drop_space(prev: Option<char>, next: Option<char>) -> bool {
    let (Some(p), Some(n)) = (prev, next) else {
        // 首尾空格（`trim` 之外的兜底）
        return true;
    };
    if is_cjk_punct(n) || matches!(n, ',' | '.' | '!' | '?' | ';' | ':' | '%') {
        return true;
    }
    if matches!(p, '（' | '(' | '「' | '『' | '【' | '[') {
        return true;
    }
    // 中文标点后面紧跟汉字时的空格同样是噪声（「你好， 世界」）
    if is_cjk_punct(p) && is_cjk(n) {
        return true;
    }
    is_cjk(p) && is_cjk(n)
}

fn is_cjk_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。'
            | '、'
            | '！'
            | '？'
            | '；'
            | '：'
            | '）'
            | '】'
            | '」'
            | '』'
            | '…'
            | '—'
            | '～'
    )
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF
    )
}

/// 一组相邻的重复句。
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// 组内句序号（升序，含被保留的第一句）。
    pub indices: Vec<usize>,
    /// 保留的那句文本（原文展示文本）。
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// 折叠结果：组数与被删掉的句数（两者都进审计与界面提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateFoldReport {
    pub groups: usize,
    pub removed: usize,
}

/// 找出所有相邻重复句组（只读，界面拿它显示「重复 N 组 / 可删 M 句」）。
pub fn duplicate_groups(segments: &[Segment]) -> Vec<DuplicateGroup> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        let mut j = i + 1;
        while j < segments.len() && is_duplicate_pair(&segments[j - 1], &segments[j]) {
            j += 1;
        }
        if j - i >= 2 {
            groups.push(DuplicateGroup {
                indices: segments[i..j].iter().map(|s| s.index).collect(),
                text: segments[i].display_text().trim().to_string(),
                start: segments[i].start,
                end: segments[j - 1].end,
            });
        }
        i = j;
    }
    groups
}

/// 折叠相邻重复句：每组保留第一句，结束时间延到组内最后一句的结束时间。
///
/// 为什么保留第一句而不是最长的那句：重复出句几乎都发生在静音边界附近，第一句
/// 才是模型在正确的上下文里解出来的那一次；而且保留第一句时，组内**后续**句子的
/// 删除不会影响它前面已经落定的时间轴。
///
/// 组内首句缺译文而后句有译文时，把译文献给保留的那一句：否则折叠会连带删掉
/// 唯一的一份译文，双语导出凭空少一行。
pub fn fold_duplicates(segments: &mut Vec<Segment>) -> DuplicateFoldReport {
    let groups = duplicate_groups(segments);
    if groups.is_empty() {
        return DuplicateFoldReport {
            groups: 0,
            removed: 0,
        };
    }
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    let mut removed = 0;
    for seg in segments.drain(..) {
        let duplicate = out.last().is_some_and(|prev| is_duplicate_pair(prev, &seg));
        if !duplicate {
            out.push(seg);
            continue;
        }
        let kept = out.last_mut().expect("上一句刚刚判定存在");
        kept.end = kept.end.max(seg.end);
        let kept_translation_missing = kept
            .translation
            .as_deref()
            .map(|t| t.trim().is_empty())
            .unwrap_or(true);
        if kept_translation_missing && seg.translation.is_some() {
            kept.translation = seg.translation;
            kept.translation_lang = seg.translation_lang;
        }
        removed += 1;
    }
    *segments = out;
    DuplicateFoldReport {
        groups: groups.len(),
        removed,
    }
}

/// 两句是否算「相邻重复」：文本（去空白、去尾标点、忽略大小写）完全相同、
/// 可见字数达标，且间隔在 [`MAX_DUP_GAP`] 以内。
fn is_duplicate_pair(prev: &Segment, next: &Segment) -> bool {
    let a = normalize_for_compare(prev.display_text());
    let b = normalize_for_compare(next.display_text());
    if a != b || a.chars().count() < MIN_DUP_CHARS {
        return false;
    }
    next.start - prev.end <= MAX_DUP_GAP
}

fn normalize_for_compare(text: &str) -> String {
    // 只留「字」：空白、标点、符号全部丢掉并忽略大小写。同一句话被重复出句时，
    // 两次的标点常常不一样（「你好，世界」/「你好世界」），按原始文本比对会漏掉
    // 真正的重复；反过来，只差标点的两句话本来就该算同一句。
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::segment::WordTiming;

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    fn all() -> CleanupRules {
        CleanupRules::recommended()
    }

    /// 语气词只在句首/标点之后删：句尾的「呀」「啊」是语气助词，删了会改语气。
    #[test]
    fn filler_only_removed_at_boundaries() {
        assert_eq!(clean_text("嗯，我来了。", all()), "我来了。");
        assert_eq!(clean_text("哦，这样啊", all()), "这样啊");
        assert_eq!(clean_text("真好呀", all()), "真好呀", "句尾语气助词不许动");
        assert_eq!(clean_text("他说，呃，明天见", all()), "他说，明天见");
    }

    /// 拉丁停顿词必须自成词：`umbrella` 不能被削成 `brella`。
    #[test]
    fn latin_filler_must_be_a_whole_word() {
        assert_eq!(clean_text("Umbrella is here", all()), "Umbrella is here");
        assert_eq!(clean_text("um, I think so", all()), "I think so");
        // 大小写不敏感，但只在词首位置成立
        assert_eq!(clean_text("Hmm, ok", all()), "ok");
    }

    /// 整句只剩停顿词时会被清空——由调用方决定是否删条，本层只如实报告。
    #[test]
    fn filler_can_empty_a_cue() {
        assert_eq!(clean_text("嗯。", all()), "");
        let segments = vec![seg(1, 0.0, 1.0, "嗯。")];
        let plan = plan(&segments, all());
        assert_eq!(plan.emptied_count(), 1);
    }

    /// 3 个以上同字连写、2 字以上单元紧挨重复都要折叠。
    #[test]
    fn cjk_run_and_unit_repeats_fold() {
        assert_eq!(clean_text("我我我来了", all()), "我来了");
        assert_eq!(clean_text("这个这个", all()), "这个");
        assert_eq!(clean_text("你你你好", all()), "你好");
    }

    /// 正常叠词与笑声不能被当成口吃改坏。
    #[test]
    fn legit_reduplication_and_laughter_are_kept() {
        assert_eq!(clean_text("谢谢谢谢", all()), "谢谢谢谢");
        assert_eq!(clean_text("刚刚好的", all()), "刚刚好的");
        assert_eq!(clean_text("哈哈哈，好", all()), "哈哈哈，好");
    }

    /// 拉丁单词紧挨重复折回一份，且不顺手改别处的排版。
    #[test]
    fn latin_word_repeats_fold() {
        assert_eq!(clean_text("the the cat", all()), "the cat");
        assert_eq!(clean_text("to to", all()), "to");
        assert_eq!(
            clean_text("the  the cat", all()),
            "the cat",
            "重复词前的双空格一起收掉"
        );
        // 只有 1 个字符的词不折：可能是真的重复（「a a」在白名单外也无从判断）
        assert_eq!(clean_text("a a", all()), "a a");
    }

    /// 听障标记：已知描述词的括号对整对删；其它括号（年份、片名）原样保留。
    #[test]
    fn sdh_removes_known_markers_and_keeps_other_brackets() {
        assert_eq!(clean_text("（掌声）你来了", all()), "你来了");
        assert_eq!(clean_text("[音乐] ♪ 我们走吧", all()), "我们走吧");
        assert_eq!(clean_text("（2020）那一年", all()), "（2020）那一年");
        assert_eq!(clean_text("他发出了 >> 的声音", all()), "他发出了的声音");
    }

    /// 括号没有配对时不许吞掉后面整段正文。
    #[test]
    fn unmatched_bracket_is_kept_verbatim() {
        assert_eq!(clean_text("[掌声", all()), "[掌声");
        assert_eq!(clean_text("（音乐", all()), "（音乐");
    }

    /// 空白规整只处理「任何风格下都不成立」的噪声，不动中英之间的排版空格。
    #[test]
    fn spacing_normalizes_cjk_noise_only() {
        assert_eq!(clean_text("你 好 世界 ， 好。", all()), "你好世界，好。");
        assert_eq!(
            clean_text("hello world", all()),
            "hello world",
            "拉丁空格是正常的"
        );
        assert_eq!(
            clean_text("见 Voice2Word 文档", all()),
            "见 Voice2Word 文档"
        );
        assert_eq!(clean_text("好的，，好的", all()), "好的，好的");
    }

    /// 全关时一个字都不改：用户关掉规则按钮后不该还有「顺手」的改动。
    #[test]
    fn disabled_rules_leave_text_untouched() {
        let raw = "嗯，你 好 世界";
        assert_eq!(clean_text(raw, CleanupRules::NONE), raw);
        let mut only_spacing = CleanupRules::NONE;
        only_spacing.set(CleanupRule::Spacing, true);
        assert_eq!(clean_text(raw, only_spacing), "嗯，你好世界");
    }

    /// 规则表的勾选状态可读可写（界面按它渲染，测试按它断言）。
    #[test]
    fn rules_toggle_and_count() {
        let mut rules = CleanupRules::NONE;
        assert!(!rules.any());
        assert_eq!(rules.enabled_count(), 0);
        rules.toggle(CleanupRule::Spacing);
        assert!(rules.enabled(CleanupRule::Spacing));
        assert_eq!(rules.enabled_count(), 1);
        rules.toggle(CleanupRule::Spacing);
        assert!(!rules.enabled(CleanupRule::Spacing));
        assert_eq!(CleanupRules::recommended().enabled_count(), 4);
    }

    /// 预览与落地必须完全一致：按钮上的数字与真正改出来的结果不能是两套。
    #[test]
    fn plan_and_apply_agree() {
        let mut segments = vec![
            seg(1, 0.0, 1.0, "嗯，我来了。"),
            seg(2, 1.0, 2.0, "这个这个"),
            seg(3, 2.0, 3.0, "（掌声）"),
            seg(4, 3.0, 4.0, "你 好"),
        ];
        let expected = plan(&segments, all());
        assert!(!expected.is_empty());
        let actual = apply(&mut segments, all());
        assert_eq!(expected, actual, "预览与落地的命中必须逐条相同");
        assert_eq!(segments[0].text, "我来了。");
        assert_eq!(segments[1].text, "这个");
        assert_eq!(segments[2].text, "", "整句只剩标记");
        assert_eq!(segments[3].text, "你好");
    }

    /// 译文只吃格式类规则：语气词/口吃是识别产物，动译文等于改语义。
    #[test]
    fn translation_only_gets_format_rules() {
        let mut one = seg(1, 0.0, 1.0, "嗯，你好");
        one.translation = Some("嗯， 你好".to_string());
        let report = apply(std::slice::from_mut(&mut one), all());
        assert_eq!(one.text, "你好");
        assert_eq!(one.translation.as_deref(), Some("嗯，你好"));
        assert_eq!(
            report.count_of(CleanupRule::Filler),
            1,
            "只有原文命中语气词"
        );
        assert_eq!(report.count_of(CleanupRule::Spacing), 1, "译文命中空白规整");
    }

    /// 润色文本存在时改动要落在 `polished` 上：界面与导出都读它，改 `text`
    /// 等于改了个看不见的地方。
    #[test]
    fn apply_writes_to_the_field_that_is_displayed() {
        let mut one = seg(1, 0.0, 1.0, "嗯，原声");
        one.polished = "嗯，润色".to_string();
        apply(std::slice::from_mut(&mut one), all());
        assert_eq!(one.polished, "润色");
        assert_eq!(one.text, "嗯，原声", "显示文本来自 polished，text 不该被动");
    }

    /// 文本一变，词级时间戳就不能再留着（否则逐词高亮会整体错位）。
    #[test]
    fn apply_clears_word_timings() {
        let mut one = seg(1, 0.0, 2.0, "嗯，我来了");
        one.words = vec![
            WordTiming {
                word: "嗯".to_string(),
                start: 0.0,
                end: 0.5,
            },
            WordTiming {
                word: "我".to_string(),
                start: 0.5,
                end: 1.0,
            },
        ];
        apply(std::slice::from_mut(&mut one), all());
        assert!(one.words.is_empty(), "文本已变，词级时间戳必须清空");
    }

    /// 汇总只列真正命中的规则：列一堆 0 会让用户以为「查过但没问题」。
    #[test]
    fn summary_lists_only_hit_rules() {
        let segments = vec![seg(1, 0.0, 1.0, "嗯，来了"), seg(2, 1.0, 2.0, "你 好")];
        let plan = plan(&segments, all());
        assert_eq!(plan.summary(), "语气词 1 · 空白标点 1");
        assert_eq!(plan.affected_segments(), 2);
        assert_eq!(plan.count_of(CleanupRule::Sdh), 0);
    }

    /// 相邻重复句：要求文本一致（忽略空白与尾标点）、字数达标、间隔够近。
    #[test]
    fn duplicate_groups_require_proximity_and_length() {
        let segments = vec![
            seg(1, 0.0, 1.0, "你好世界"),
            seg(2, 1.0, 2.0, "你好，世界。"),
            seg(3, 5.0, 6.0, "你好世界"),
            seg(4, 6.0, 7.0, "对。"),
            seg(5, 7.0, 8.0, "对。"),
        ];
        let groups = duplicate_groups(&segments);
        assert_eq!(
            groups.len(),
            1,
            "隔了 3 秒的重复句与单字重复都不算: {groups:?}"
        );
        assert_eq!(groups[0].indices, vec![1, 2]);
        assert_eq!(groups[0].end, 2.0);
    }

    /// 折叠：保留第一句、时间跨度延到组内末句、后面的句子消失。
    #[test]
    fn fold_duplicates_keeps_first_and_extends_end() {
        let mut segments = vec![
            seg(1, 0.0, 1.0, "你好世界"),
            seg(2, 1.0, 2.0, "你好世界"),
            seg(3, 2.5, 3.5, "别的"),
        ];
        let report = fold_duplicates(&mut segments);
        assert_eq!(
            report,
            DuplicateFoldReport {
                groups: 1,
                removed: 1
            }
        );
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].index, 1);
        assert_eq!(segments[0].text, "你好世界");
        assert_eq!(segments[0].end, 2.0, "保留句的结束时间要盖住整组");
        assert_eq!(segments[1].text, "别的");
    }

    /// 首句没译文、后句有译文时，折叠不能把唯一的一份译文丢掉。
    #[test]
    fn fold_duplicates_keeps_translation_from_later_cue() {
        let mut segments = vec![seg(1, 0.0, 1.0, "你好世界"), seg(2, 1.0, 2.0, "你好世界")];
        segments[1].translation = Some("Hello world".to_string());
        segments[1].translation_lang = Some("English".to_string());
        fold_duplicates(&mut segments);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].translation.as_deref(), Some("Hello world"));
        assert_eq!(segments[0].translation_lang.as_deref(), Some("English"));
    }

    /// 没有重复时原样返回：界面据此把按钮置灰，不做「删了 0 句」的假动作。
    #[test]
    fn fold_duplicates_is_a_noop_without_groups() {
        let mut segments = vec![seg(1, 0.0, 1.0, "甲"), seg(2, 1.0, 2.0, "乙")];
        let before = segments.clone();
        let report = fold_duplicates(&mut segments);
        assert_eq!(report.groups, 0);
        assert_eq!(report.removed, 0);
        assert_eq!(segments, before);
    }
}
