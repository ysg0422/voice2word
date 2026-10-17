//! 自动章节：把逐句字幕聚成「可直接发布的章节列表」。
//!
//! 章节（chapters）不是字幕：它的每一行是「时间 + 一句话标题」，面向 YouTube
//! 简介、播客 shownotes、视频导航，因此不能塞进 writer 的「段 → 行」管线。
//! 本模块只做**纯函数**（分组 / 提示词 / 解析 / 导出），不碰 IO、不调模型，
//! 全部可单测；真正的写盘在 [`crate::subtitle::writer::write_chapters_file`]。
//!
//! # 为什么标题要交给模型，而不是直接从原文截
//!
//! 兜底标题（每组首句）永远可用，但它只是「原话」，不是标题；模型能把它压成
//! 「性能优化：三个常见误区」这类可读标签。所以流程是：
//! [`plan_chapters`] 分组 → [`build_title_prompt`] 问模型 → [`parse_titles`]
//! 解析；一旦模型跑偏（行数不对 / 整段废话），解析返回 `None`，调用方**原样
//! 退回兜底标题**——标题生成是锦上添花，绝不能让一次导出的成败取决于模型。
//!
//! # 时间格式的两种形态
//!
//! `mm:ss`（不足 1 小时）与 `h:mm:ss`（满 1 小时）。YouTube 的章节解析器两种
//! 都认，但**首行必须恰好是 `00:00`**，否则整段章节列表不生效——这是平台规则，
//! 不是格式偏好，因此 [`to_youtube_chapters`] 会强制首章写成 `00:00`。

use crate::subtitle::segment::Segment;

/// 一章：视频里的一个导航点。
///
/// 只留「起点 + 标题」两个字段：章节列表本来就只有这两样，结束时间由下一章
/// 起点隐含表达（末章到片尾）。多存一个 `end_secs` 就多出一份真相，两处一旦
/// 不同步，导出与预览就会互相打脸。
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    /// 章节起点（秒）。负数 / NaN / 无穷在导出时一律按 0 处理。
    pub start_secs: f64,
    /// 章节标题。导出时 `trim`；空标题会得到「章节 N」占位（见 [`to_youtube_chapters`]）。
    pub title: String,
}

/// 兜底分组的默认目标时长：5 分钟。
///
/// 为什么是 300s：YouTube 章节要求每章至少 10s、且总章数 ≥ 3，而观众对一屏
/// 十几个章节的可读性有个上限；5 分钟既够一段话题讲完，又不会让 1 小时的片子
/// 只剩 5 个章节。
pub const DEFAULT_TARGET_SECS: f64 = 300.0;

/// 单章最短时长（秒）。
///
/// 不足 60s 的组并入邻组：几十秒的「章节」在播放器里连标题都来不及读完，
/// 却会把章节总表撑得很碎；这条规则也让「片尾只剩 20s」不会单独成章。
pub const MIN_CHAPTER_SECS: f64 = 60.0;

/// 组数上限。
///
/// 上限放在**分组之后**做合并（见 [`plan_chapters`]）：先按目标时长切、再按
/// 最短邻组合并，比「把目标时长调大重切一遍」更能保住原本对齐的内容边界。
pub const MAX_CHAPTERS: usize = 30;

/// 兜底标题的最大字数（按字符计，不是字节）。
const TITLE_MAX_CHARS: usize = 24;

/// 解析出的模型标题的长度上限。
///
/// 只是防跑飞（模型偶尔把一句解释写成标题），正常标题远不到这个长度。
const PARSED_TITLE_MAX_CHARS: usize = 200;

/// 提示词里每章正文摘录的字数上限。
///
/// 30 章 × 整章全文可能上万字，而标题只需要「这段在讲什么」的线索；截断后
/// 提示词规模可控，模型也更不容易在中途丢掉行。
const PROMPT_TEXT_MAX_CHARS: usize = 600;

// ─────────────────────────────────────────────────────────────
// 分组（无 LLM 的兜底，也是模型标题的输入）
// ─────────────────────────────────────────────────────────────

/// 无 LLM 的兜底分组：按累计时长切段，标题取该段第一句（截断 [`TITLE_MAX_CHARS`] 字）。
///
/// 规则：
/// 1. **空文本句直接忽略**——它们既不该成为章节起点，也不该贡献时长；
///    「有说话人没字」的段（识别失败 / 纯音乐）在章节里没有意义。
/// 2. 切点永远落在**某一句话的起点**：当这句的起点比本组起点晚 `target_secs`
///    或更多时就开新组。用起点而不是终点，是为了不因某句特别长而把切点推后，
///    章节时间戳也才能和字幕行一一对上。
/// 3. 不足 [`MIN_CHAPTER_SECS`] 的组并入邻组（优先并入上一组；首组没有上一组，
///    就并入下一组）。**合并后起点取被并入组的起点**，标题也取合并后组内的首句。
/// 4. 组数超过 [`MAX_CHAPTERS`] 时，反复把「合并后总时长最短」的相邻两组并起来。
///
/// `target_secs` 非有限或 ≤ 0 时回落 [`DEFAULT_TARGET_SECS`]；其余值按调用方
/// 给的来（小于 [`MIN_CHAPTER_SECS`] 也会被第 3 条自动抬到 60s 以上）。
pub fn plan_chapters(segments: &[Segment], target_secs: f64) -> Vec<Chapter> {
    let target = if target_secs.is_finite() && target_secs > 0.0 {
        target_secs
    } else {
        DEFAULT_TARGET_SECS
    };

    // 先过滤再做分组：过滤后所有条目都「有时间、有文本」，后续逻辑不必反复判空。
    let items: Vec<(f64, f64, &str)> = segments
        .iter()
        .filter_map(|seg| {
            if !seg.start.is_finite() {
                return None;
            }
            // 用 display_text（优先标点恢复 / 润色结果）而不是裸 text：章节标题
            // 要靠句号断句，没有标点的原文会让「第一句」退化成整段。
            let text = seg.display_text().trim();
            if text.is_empty() {
                return None;
            }
            let start = seg.start.max(0.0);
            // 时间倒挂 / 结束时间缺失时按「零时长」处理，而不是丢弃这句文本。
            let end = if seg.end.is_finite() {
                seg.end.max(start)
            } else {
                start
            };
            Some((start, end, text))
        })
        .collect();
    if items.is_empty() {
        return Vec::new();
    }

    let mut groups: Vec<Group> = Vec::new();
    let mut current = Group::from_item(&items[0]);
    for item in &items[1..] {
        if item.0 - current.start >= target {
            groups.push(current);
            current = Group::from_item(item);
        } else if item.1 > current.end {
            current.end = item.1;
        }
    }
    groups.push(current);

    // ① 不足最短时长的组并入邻组。每轮至少消掉一个组，必然终止。
    while groups.len() > 1 {
        match groups
            .iter()
            .position(|g| g.end - g.start < MIN_CHAPTER_SECS)
        {
            // 首组没有上一组：并入下一组，但起点与标题仍归首组（它才是这段的开头）。
            Some(0) => {
                let head = groups.remove(0);
                groups[0].start = head.start;
                groups[0].first = head.first;
            }
            Some(i) => {
                let tail = groups.remove(i);
                let prev = &mut groups[i - 1];
                if tail.end > prev.end {
                    prev.end = tail.end;
                }
            }
            None => break,
        }
    }

    // ② 组数上限：合并「总时长最短」的相邻两组，尽量少破坏内容边界。
    while groups.len() > MAX_CHAPTERS {
        let mut best = 0usize;
        let mut best_span = f64::INFINITY;
        for i in 0..groups.len() - 1 {
            let span = groups[i + 1].end - groups[i].start;
            if span < best_span {
                best_span = span;
                best = i;
            }
        }
        let tail = groups.remove(best + 1);
        if tail.end > groups[best].end {
            groups[best].end = tail.end;
        }
    }

    groups
        .into_iter()
        .map(|g| Chapter {
            start_secs: g.start,
            title: truncate_chars(first_sentence(&g.first), TITLE_MAX_CHARS),
        })
        .collect()
}

/// 分组过程中的中间态：起点、终点与该组**第一句**（标题来源）。
struct Group {
    start: f64,
    end: f64,
    first: String,
}

impl Group {
    fn from_item(item: &(f64, f64, &str)) -> Self {
        Self {
            start: item.0,
            end: item.1,
            first: item.2.to_string(),
        }
    }
}

/// 取第一句：在句末标点处切开，返回第一段非空白内容。
///
/// 句末标点里 `'.'` 只在**后面紧跟空白或行尾**时才算句末——否则 `3.5 小时`、
/// `v1.2 发布` 会被切成毫无意义的「3」，而这正是章节标题最常见的翻车点。
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let mut sentence_start = 0usize;
    for (i, ch) in text.char_indices() {
        let is_end = match ch {
            '。' | '！' | '？' | '!' | '?' | '；' | ';' | '…' | '\n' | '\r' => true,
            '.' => match text[i + 1..].chars().next() {
                None => true,
                Some(next) => next.is_whitespace(),
            },
            _ => false,
        };
        if is_end {
            let candidate = text[sentence_start..i].trim();
            if !candidate.is_empty() {
                return candidate;
            }
            sentence_start = i + ch.len_utf8();
        }
    }
    text[sentence_start..].trim()
}

/// 按字符（不是字节）截断，截断后去掉悬空的标点。
///
/// 中文字符是 3 字节，按字节截断会把标题劈成非法 UTF-8 的乱码；按 `chars()`
/// 取前 N 个才安全。截断落在「，」「、」上时看起来像没写完，顺手抹掉。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        while out.chars().next_back().is_some_and(is_trailing_separator) {
            out.pop();
        }
    }
    out.trim_end().to_string()
}

fn is_trailing_separator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '，' | ',' | '、' | '；' | ';' | '：' | ':')
}

// ─────────────────────────────────────────────────────────────
// 提示词
// ─────────────────────────────────────────────────────────────

/// 生成「要求模型给标题」的提示词。
///
/// 输入是已分组好的 `(起点秒, 终点秒, 该组正文)`。提示词里把**输出格式写死**
/// 成 [`parse_titles`] 能吃下的形态（`[mm:ss] 标题`、严格 N 行），这样「模型
/// 照做」与「解析器接受」是同一份契约，而不是两处各自猜。
pub fn build_title_prompt(groups: &[(f64, f64, String)]) -> String {
    let n = groups.len();
    let mut out = String::with_capacity(512 + groups.len() * 96);
    out.push_str("你在为一段已经转写完成的视频生成章节标题。下面按时间顺序列出了 ");
    out.push_str(&n.to_string());
    out.push_str(" 个章节，每行是「起始时间 -> 结束时间 正文摘录」。\n\n");
    out.push_str("请为每一章拟一个简短、具体、一眼能看出这段在讲什么的标题。要求：\n");
    out.push_str("1. 只输出 ");
    out.push_str(&n.to_string());
    out.push_str(" 行，一行一个章节，顺序与下面完全一致，不能多也不能少；\n");
    out.push_str("2. 每行写成 `[mm:ss] 标题`（满 1 小时用 `h:mm:ss`），时间照抄下面的起始时间；\n");
    out.push_str("3. 标题不超过 20 个字，不要序号、不要引号、不要句末标点、不要任何解释；\n");
    out.push_str("4. 不要输出表头、列表符号或 ``` 代码围栏；标题语言与正文语言保持一致。\n\n");
    out.push_str("章节列表：\n");
    for (start, end, text) in groups {
        out.push('[');
        out.push_str(&format_chapter_stamp(*start));
        out.push_str("] -> [");
        out.push_str(&format_chapter_stamp(*end));
        out.push_str("] ");
        out.push_str(&prompt_excerpt(text));
        out.push('\n');
    }
    out
}

/// 提示词里的正文摘录：压成一行（否则模型会把换行当成新的章节行）+ 截断。
fn prompt_excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = flat.chars().count() > PROMPT_TEXT_MAX_CHARS;
    let mut excerpt = truncate_chars(&flat, PROMPT_TEXT_MAX_CHARS);
    if truncated {
        excerpt.push('…');
    }
    excerpt
}

// ─────────────────────────────────────────────────────────────
// 解析模型输出
// ─────────────────────────────────────────────────────────────

/// 解析模型输出：每行 `时间<TAB或空格>标题`、`[mm:ss] 标题`，或只有 `标题`（按顺序对应）。
///
/// 容错清单（都是模型实际会写出来的东西）：
/// - 三反引号 / 三波浪线围栏行、空行、行首行尾空白；
/// - `[00:12]` / `[1:02:03]` 方括号时间前缀；
/// - 裸时间前缀（`00:12 标题`、`00:12\t标题`、`00:12 - 标题`）；
/// - 列表符号（`-` `*` `+` `•`）、数字序号（`1.` `2)` `3、`）、Markdown 粗体；
/// - 成对引号（`"…"`、`「…」`、`“…”`）；
/// - 模型开头的客套话「以下是章节标题：」（只在还没解析出标题时按表头跳过）。
///
/// 标题内部的空格原样保留（`结尾 Q&A` 不会被拆开）。时间前缀**不参与**匹配：
/// 章节按输入顺序一一对应，模型把时间写错/省略都不会让标题错位到别的章。
///
/// 行数必须**恰好**等于 `expected`，否则返回 `None` 由调用方回退兜底标题——
/// 「少一行」会让后面的标题整体前移，「多一行」会把说明文字当成标题，两种都
/// 比没有标题更难发现。
pub fn parse_titles(raw: &str, expected: usize) -> Option<Vec<String>> {
    // 预分配上限 64：`expected` 由调用方给，写错成很大的数时不该先吃掉一大块内存
    //（真实行数不可能超过返回的行数，超出部分由 `!= expected` 判失败）。
    let mut titles: Vec<String> = Vec::with_capacity(expected.min(64));
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_fence(trimmed) {
            continue;
        }
        // 表头只可能出现在第一个标题之前；带时间戳的行不可能是表头。
        if titles.is_empty()
            && (trimmed.ends_with('：') || trimmed.ends_with(':'))
            && strip_bracket_stamp(trimmed).is_none()
            && strip_bare_stamp(trimmed).is_none()
        {
            continue;
        }
        if let Some(title) = title_from_line(trimmed) {
            titles.push(title);
        }
    }
    if titles.len() != expected {
        return None;
    }
    Some(titles)
}

fn is_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

/// 单行 → 标题。返回 `None` 表示这一行没有可用标题（例如只有时间戳）。
fn title_from_line(line: &str) -> Option<String> {
    let mut rest = line;
    // 前缀可能叠加（`- [00:12] 标题`、`1. 00:12 标题`），循环剥到剥不动为止。
    // 每轮都严格变短，因此不会死循环。
    for _ in 0..6 {
        let next = strip_leading_noise(rest);
        if next.len() == rest.len() {
            break;
        }
        rest = next;
    }
    let title = finalize_title(rest);
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// 剥掉一层「行首噪声」：空白 + 列表/分隔符 + 数字序号 + 时间前缀。
fn strip_leading_noise(s: &str) -> &str {
    let mut t = s.trim_start();
    t = t.trim_start_matches(|c: char| {
        matches!(
            c,
            '-' | '–' | '—' | '*' | '+' | '•' | '·' | '|' | '：' | ':'
        )
    });
    t = t.trim_start();
    if let Some(after) = strip_number_marker(t) {
        t = after.trim_start();
    }
    if let Some(after) = strip_bracket_stamp(t) {
        t = after.trim_start();
    }
    if let Some(after) = strip_bare_stamp(t) {
        t = after.trim_start();
    }
    t
}

/// `[00:12]` / `[1:02:03]` 前缀；方括号里不是时间就**不当前缀**（保留原文本）。
fn strip_bracket_stamp(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    if !is_time_token(rest[..end].trim()) {
        return None;
    }
    Some(&rest[end + 1..])
}

/// 裸时间前缀；时间token 后面必须紧跟分隔符，否则它可能只是标题的一部分。
fn strip_bare_stamp(s: &str) -> Option<&str> {
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == ':'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if !is_time_token(&s[..end]) {
        return None;
    }
    let rest = &s[end..];
    let separated = match rest.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '-' | '–' | '—' | '|' | '·' | ')' | '：' | ':'),
    };
    if separated {
        Some(rest)
    } else {
        None
    }
}

/// `mm:ss` 或 `h:mm:ss`（分秒均补零；秒必须两位，避免把 `3:5` 这种正文当时间）。
fn is_time_token(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    match parts.as_slice() {
        [minutes, seconds] => is_digits(minutes, 1..=3) && is_digits(seconds, 2..=2),
        [hours, minutes, seconds] => {
            is_digits(hours, 1..=2) && is_digits(minutes, 2..=2) && is_digits(seconds, 2..=2)
        }
        _ => false,
    }
}

fn is_digits(s: &str, len_range: std::ops::RangeInclusive<usize>) -> bool {
    !s.is_empty() && len_range.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// `1.` / `2)` / `3、` 数字序号。
///
/// 点号后面必须紧跟空白或行尾：`1.5 倍增长` 里的 `1.` 是数字的一部分，不是序号。
/// 冒号刻意不在序号分隔符里——那会和 `1:23` 这种时间前缀打架。
fn strip_number_marker(s: &str) -> Option<&str> {
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 || end > 2 {
        return None;
    }
    let after_digits = &s[end..];
    let sep = after_digits.chars().next()?;
    if !matches!(sep, '.' | ')' | '、' | '．') {
        return None;
    }
    let rest = &after_digits[sep.len_utf8()..];
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_whitespace() => Some(rest),
        Some(_) => None,
    }
}

/// 收尾：去成对引号 / 粗体星号、按上限截断。
fn finalize_title(raw: &str) -> String {
    let mut t = raw.trim();
    for (open, close) in [
        ('"', '"'),
        ('\'', '\''),
        ('「', '」'),
        ('“', '”'),
        ('《', '》'),
    ] {
        if t.starts_with(open) && t.ends_with(close) && t.len() >= 2 {
            t = t[open.len_utf8()..t.len() - close.len_utf8()].trim();
            break;
        }
    }
    let t = t.trim_matches('*').trim();
    truncate_chars(t, PARSED_TITLE_MAX_CHARS)
}

// ─────────────────────────────────────────────────────────────
// 导出
// ─────────────────────────────────────────────────────────────

/// 章节时间戳：不足 1 小时写 `mm:ss`，满 1 小时写 `h:mm:ss`（小时不补零）。
///
/// 四舍五入到整秒：章节是给人点着跳转的，秒以下没有意义；把 `59.6` 进成 `01:00`
/// 也避免了「59:60」这种非法时间码。负数 / NaN / 无穷一律按 0 处理。
pub fn format_chapter_stamp(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds.round() as u64
    } else {
        0
    };
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, secs)
    } else {
        format!("{:02}:{:02}", minutes, secs)
    }
}

/// YouTube 章节文本：一行一章，`mm:ss 标题`。
///
/// **首行强制 `00:00`**：这是 YouTube 识别章节列表的硬性条件（首章不是 0:00
/// 时整段列表直接被当成普通文字）。其余章节按传入顺序输出，不做排序——调用方
/// 若给了乱序的章节，那是数据问题，静默重排只会让人更难发现。
///
/// 标题为空时补「章节 N」：只有时间戳的行在 YouTube 里不是章节，会凭空让首行
/// 失效，等于整份导出白做。
pub fn to_youtube_chapters(chapters: &[Chapter]) -> String {
    let mut out = String::new();
    for (i, chapter) in chapters.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let stamp = if i == 0 {
            "00:00".to_string()
        } else {
            format_chapter_stamp(chapter.start_secs)
        };
        out.push_str(&stamp);
        out.push(' ');
        out.push_str(&chapter_title(chapter, i));
    }
    out
}

/// 展示用标题：trim、把换行压成空格（一条标题必须占一行），空标题补占位。
fn chapter_title(chapter: &Chapter, index: usize) -> String {
    let title = chapter.title.trim().replace(['\r', '\n'], " ");
    if title.is_empty() {
        format!("章节 {}", index + 1)
    } else {
        title
    }
}

/// JSON：`[{"start": 0.0, "title": "..."}]`（pretty）。
///
/// `start` 保留毫秒精度并抹掉浮点噪声（`0.1 + 0.2` 这类值不该出现在交付物里）；
/// 负数 / 非有限值按 0。空输入输出 `[]`（而不是空串），下游 `serde_json`
/// 解析不需要特判空文档。
pub fn to_json(chapters: &[Chapter]) -> String {
    let items: Vec<serde_json::Value> = chapters
        .iter()
        .map(|chapter| {
            serde_json::json!({
                "start": sanitize_start(chapter.start_secs),
                "title": chapter.title.trim(),
            })
        })
        .collect();
    // `to_string_pretty` 只在值含 NaN / Infinity 时会失败，而它们已被
    // [`sanitize_start`] 挡掉；兜底分支实际不可达，但纯函数里不该留 `unwrap`。
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
}

fn sanitize_start(seconds: f64) -> f64 {
    if !seconds.is_finite() {
        return 0.0;
    }
    (seconds.max(0.0) * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(index: usize, start: f64, end: f64, text: impl Into<String>) -> Segment {
        Segment::new(index, start, end, text)
    }

    /// 边界：切点落在「起点达到 target」的那一句上；非法 target 回落默认 300s。
    #[test]
    fn plan_cuts_at_target_boundaries() {
        let segs: Vec<Segment> = (0..34)
            .map(|i| {
                let start = i as f64 * 30.0;
                segment(i, start, start + 30.0, format!("第 {i} 句。"))
            })
            .collect();

        let chapters = plan_chapters(&segs, 300.0);
        let starts: Vec<f64> = chapters.iter().map(|c| c.start_secs).collect();
        assert_eq!(starts, vec![0.0, 300.0, 600.0, 900.0], "{chapters:?}");
        assert!(chapters.iter().all(|c| !c.title.is_empty()));

        // 目标时长非法 → 与显式传 300 完全一致
        assert_eq!(plan_chapters(&segs, 0.0), chapters);
        assert_eq!(plan_chapters(&segs, f64::NAN), chapters);
    }

    /// 空句：不贡献起点、不贡献时长、不参与标题。
    #[test]
    fn plan_ignores_blank_text() {
        let segs = vec![
            segment(1, 0.0, 5.0, "   "),
            segment(2, 5.0, 40.0, ""),
            segment(3, 40.0, 100.0, "大家好。今天我们聊 Rust。"),
            segment(4, 100.0, 130.0, "第二部分。"),
        ];
        let chapters = plan_chapters(&segs, 300.0);
        assert_eq!(chapters.len(), 1, "空句不得自成章节: {chapters:?}");
        assert_eq!(chapters[0].start_secs, 40.0, "起点应取第一条有文本的句子");
        assert_eq!(chapters[0].title, "大家好");

        assert!(plan_chapters(&[], 300.0).is_empty());
        assert!(plan_chapters(&[segment(1, 0.0, 80.0, "   ")], 300.0).is_empty());
    }

    /// 最短时长：尾巴并入上一组；首组没有上一组时并入下一组。
    #[test]
    fn plan_merges_short_groups_into_previous() {
        let segs = vec![
            segment(1, 0.0, 150.0, "开场白。"),
            segment(2, 150.0, 300.0, "正题。"),
            segment(3, 300.0, 320.0, "收尾。"),
        ];
        let chapters = plan_chapters(&segs, 300.0);
        assert_eq!(chapters.len(), 1, "20s 的尾巴不该单独成章: {chapters:?}");
        assert_eq!(chapters[0].start_secs, 0.0);
        assert_eq!(chapters[0].title, "开场白");

        // 首组只有 30s：并入下一组，起点与标题仍归合并后的开头
        let segs = vec![
            segment(1, 0.0, 30.0, "短开场。"),
            segment(2, 30.0, 90.0, "正文。"),
        ];
        let chapters = plan_chapters(&segs, 10.0);
        assert_eq!(
            chapters.len(),
            1,
            "首组长不足 60s 应并入下一组: {chapters:?}"
        );
        assert_eq!(chapters[0].start_secs, 0.0);
        assert_eq!(chapters[0].title, "短开场");
    }

    /// 组数上限 30：合并后起点仍严格递增、无空标题。
    #[test]
    fn plan_caps_chapters_at_thirty() {
        let segs: Vec<Segment> = (0..200)
            .map(|i| {
                let start = i as f64 * 30.0;
                segment(i, start, start + 30.0, format!("第 {i} 句。"))
            })
            .collect();
        let chapters = plan_chapters(&segs, 60.0);
        assert_eq!(chapters.len(), MAX_CHAPTERS, "组数上限 30: {chapters:?}");
        assert_eq!(chapters[0].start_secs, 0.0);
        assert!(
            chapters
                .windows(2)
                .all(|w| w[0].start_secs < w[1].start_secs),
            "章节起点必须严格递增: {chapters:?}"
        );
        assert!(chapters.iter().all(|c| !c.title.is_empty()));
    }

    /// 兜底标题：取第一句、按字符截断 24 字；句末标点不进标题。
    #[test]
    fn plan_titles_take_first_sentence_capped_at_24_chars() {
        let long = "今天我们来非常详细地聊一聊如何在一个完全陌生的项目里快速定位性能瓶颈并把它修好。第二句不该进标题。";
        let chapters = plan_chapters(&[segment(1, 0.0, 90.0, long)], 300.0);
        assert_eq!(chapters.len(), 1);
        assert_eq!(
            chapters[0].title.chars().count(),
            24,
            "{}",
            chapters[0].title
        );
        assert_eq!(
            chapters[0].title,
            "今天我们来非常详细地聊一聊如何在一个完全陌生的项"
        );
        assert!(!chapters[0].title.contains("第二句"));

        // 首句够短：原样使用，`。` 被吃掉
        let short = plan_chapters(&[segment(1, 0.0, 90.0, "开场闲聊。后面还有。")], 300.0);
        assert_eq!(short[0].title, "开场闲聊");
    }

    /// 解析：围栏 / 空行 / 列表符号 / 编号 / 方括号时间 / 裸时间 / 引号 / 表头全都要能忍。
    #[test]
    fn parse_titles_tolerates_dirty_model_output() {
        let raw = "\
```text
以下是章节标题：
[00:00] 开场闲聊

- [00:12] 第一个话题：性能
  00:30\t结尾 Q&A
01:00 - 没有时间前缀的标题
  01:30  「深夜食堂」
2. 第二个话题
```";
        let titles = parse_titles(raw, 6).expect("脏格式应当能解析");
        assert_eq!(
            titles,
            vec![
                "开场闲聊",
                "第一个话题：性能",
                "结尾 Q&A",
                "没有时间前缀的标题",
                "深夜食堂",
                "第二个话题",
            ]
        );
    }

    /// 解析失败：行数不符 / 只有围栏 / 只有时间戳，都必须返回 None（交给兜底标题）。
    #[test]
    fn parse_titles_rejects_wrong_line_counts() {
        assert!(
            parse_titles("[00:00] 一\n[00:10] 二", 3).is_none(),
            "少一行"
        );
        assert!(
            parse_titles("[00:00] 一\n[00:10] 二\n[00:20] 三", 2).is_none(),
            "多一行"
        );
        assert!(
            parse_titles("完全没有时间也没关系", 2).is_none(),
            "只有一行却要两章"
        );
        assert!(
            parse_titles("```\n\n```", 1).is_none(),
            "只有围栏/空行时不得凑数"
        );
        assert_eq!(parse_titles("", 0), Some(Vec::new()));

        // 满 1 小时的时间前缀同样要认
        assert_eq!(
            parse_titles("[1:02:03] 一小时后的章节", 1),
            Some(vec!["一小时后的章节".to_string()])
        );
    }

    /// 时间戳：mm:ss / h:mm:ss 的分界在 1 小时；四舍五入会跨分钟、跨小时。
    #[test]
    fn stamps_use_mm_ss_under_one_hour_and_h_mm_ss_above() {
        let chapters = vec![
            Chapter {
                start_secs: 0.0,
                title: "开场".into(),
            },
            Chapter {
                start_secs: 61.4,
                title: "中段".into(),
            },
            Chapter {
                start_secs: 3599.6,
                title: "整一小时".into(),
            },
            Chapter {
                start_secs: 3661.0,
                title: "再往后".into(),
            },
        ];
        assert_eq!(
            to_youtube_chapters(&chapters),
            "00:00 开场\n01:01 中段\n1:00:00 整一小时\n1:01:01 再往后"
        );
        assert_eq!(format_chapter_stamp(59.6), "01:00", "进位要能跨进分钟");
        assert_eq!(format_chapter_stamp(3599.6), "1:00:00", "进位要能跨进小时");
        assert_eq!(format_chapter_stamp(3600.0), "1:00:00");
        assert_eq!(format_chapter_stamp(-5.0), "00:00");
        assert_eq!(format_chapter_stamp(f64::NAN), "00:00");
    }

    /// YouTube 规则：首章必须是 0:00；空标题不能产出「只有时间」的行。
    #[test]
    fn youtube_chapters_force_first_line_to_zero() {
        let chapters = vec![
            Chapter {
                start_secs: 12.0,
                title: "正片开始".into(),
            },
            Chapter {
                start_secs: 95.0,
                title: "第二段".into(),
            },
        ];
        assert_eq!(
            to_youtube_chapters(&chapters),
            "00:00 正片开始\n01:35 第二段"
        );

        let blank = vec![Chapter {
            start_secs: 0.0,
            title: "   ".into(),
        }];
        assert_eq!(to_youtube_chapters(&blank), "00:00 章节 1");

        // 标题里的换行会把一章劈成两行，必须压平
        let multiline = vec![Chapter {
            start_secs: 0.0,
            title: "上\n下".into(),
        }];
        assert_eq!(to_youtube_chapters(&multiline), "00:00 上 下");

        assert_eq!(to_youtube_chapters(&[]), "");
    }

    /// JSON：pretty 数组、字段名 `start` / `title`、转义正确、空输入是 `[]`。
    #[test]
    fn json_export_is_pretty_array_of_start_and_title() {
        let chapters = vec![
            Chapter {
                start_secs: 0.0,
                title: "开场".into(),
            },
            Chapter {
                start_secs: 65.5,
                title: "A \"quoted\" 标题".into(),
            },
        ];
        let json = to_json(&chapters);
        let value: serde_json::Value = serde_json::from_str(&json).expect("JSON 必须可解析");
        let array = value.as_array().expect("顶层必须是数组");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["start"].as_f64(), Some(0.0));
        assert_eq!(array[0]["title"].as_str(), Some("开场"));
        assert_eq!(array[1]["start"].as_f64(), Some(65.5));
        assert_eq!(array[1]["title"].as_str(), Some("A \"quoted\" 标题"));
        assert!(json.contains('\n'), "要求 pretty 输出: {json}");
        assert_eq!(to_json(&[]), "[]");
    }

    /// 提示词：章数、每章起始时间、正文摘录都在；按提示词格式写出的示例能被解析回去。
    #[test]
    fn title_prompt_lists_every_group_with_its_start() {
        let groups = vec![
            (0.0, 300.0, "开场白。".to_string()),
            (3661.0, 3900.0, "第二部分。".to_string()),
            (3900.0, 4000.0, "第三\n部分。".to_string()),
        ];
        let prompt = build_title_prompt(&groups);
        assert!(prompt.contains("3 个章节"), "{prompt}");
        assert!(prompt.contains("[00:00]"), "{prompt}");
        assert!(prompt.contains("[1:01:01]"), "{prompt}");
        assert!(prompt.contains("开场白"));
        assert!(prompt.contains("第二部分"));
        assert!(
            !prompt.contains("第三\n部分"),
            "正文摘录必须压成一行，否则模型会当成新的章节行"
        );
        assert!(prompt.contains("第三 部分"));

        // 「模型照做」写出的输出必须真的能被解析回来
        let echoed = "[00:00] 开场\n[1:01:01] 第二部分\n[1:05:00] 第三部分";
        assert_eq!(
            parse_titles(echoed, groups.len()),
            Some(vec![
                "开场".to_string(),
                "第二部分".to_string(),
                "第三部分".to_string()
            ])
        );
    }
}
