//! 术语表的「文本 ↔ 结构 ↔ CSV」纯逻辑层。
//!
//! # 定位（与配置的关系）
//!
//! 配置里存的是**一段多行文本**（`src/utils/config.rs` 的 `TranslateConfig::glossary`），
//! 因为「用户手写、原样落盘、原样注入提示词」这条链路最不需要解释。本模块不碰配置
//! 结构、不碰 IO、不碰 UI，只负责把那段文本与**结构 / 文件格式**互转：
//!
//! - 文本 ↔ [`GlossaryEntry`]：[`parse`] / [`to_text`]（编辑框回填与保存）；
//! - 表格文件 ↔ 结构：[`to_csv`] / [`from_csv`]（导入导出、与表格软件互通）；
//! - 两份术语表的合并：[`merge`]（用户新导入的那份覆盖旧表里的同名条目）；
//! - 体检：[`validate`]（写回文件前，先把「一定会变形」的条目标出来）。
//!
//! 于是「同一份术语表在文本、结构、CSV 三种表示之间不丢内容」这件事被压成一组纯函数，
//! 可以脱离界面直接单测；界面的活只剩「拿哪个文件、什么时候调用、结果怎么展示」。
//!
//! # 为什么文本格式只认半角 `=`
//!
//! 配置层（`TranslateConfig::glossary_entries`）为了让用户「从聊天记录里粘一行」也能用，
// 额外接受 `->` / `→` / `:` / `：`。本模块**不能**跟着放宽：那些写法会把译文里本来
// 就有的字符（`时间: 2024`）切坏，而这里要保证 `to_text → parse` 逐条往返一致——
// 一个只在某一个方向成立的分隔符，正是「保存一次术语表就少一条」的来源。两者用途
// 不同，各留各的口径。

use std::collections::HashMap;

/// 键值分隔符：只认半角 `=`。
const SEPARATOR: char = '=';

/// CSV 表头。用 ASCII 列名而不是中文：这份文件要被脚本和表格软件读，
/// 固定英文列名最不容易在编码 / 本地化上出岔子；[`from_csv`] 也认它并跳过。
const CSV_HEADER: [&str; 2] = ["source", "target"];

/// 一条术语：原文（字幕里出现的写法）与它必须采用的译文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryEntry {
    pub source: String,
    pub target: String,
}

impl GlossaryEntry {
    /// 建一条术语。
    pub fn new(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
        }
    }
}

/// [`parse_report`] 的结果：解析出的条目 + 被跳过的行号。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseReport {
    /// 解析出的条目，保持文件里的先后顺序。
    pub entries: Vec<GlossaryEntry>,
    /// 有内容但没能变成条目的行号（**1 起**，便于在编辑器里直接跳行）。
    ///
    /// 只记「丢了东西」的行。空行与 `#` 注释是用户**有意**写的，不在此列——把它们也
    /// 算成「跳过」，界面就会报「导入时跳过了 12 行」而用户根本没写错，这类假警报会
    /// 让真的那一条被顺手忽略掉。
    pub skipped_lines: Vec<usize>,
}

/// 解析术语表文本，并汇报哪些行被跳过了。
///
/// 规则：
/// - 每行一条 `原文=译文`，只按**第一个** `=` 切分（译文里再出现 `=` 不会被切坏）；
/// - 两侧各自 trim；
/// - 空行、`#` 开头的注释行安静跳过；
/// - 没有 `=`、以及任一侧为空的**内容行**计入 [`ParseReport::skipped_lines`]。
pub fn parse_report(text: &str) -> ParseReport {
    let mut report = ParseReport::default();
    for (offset, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line_no = offset + 1;
        let Some((left, right)) = line.split_once(SEPARATOR) else {
            report.skipped_lines.push(line_no);
            continue;
        };
        let source = left.trim();
        let target = right.trim();
        if source.is_empty() || target.is_empty() {
            report.skipped_lines.push(line_no);
            continue;
        }
        report.entries.push(GlossaryEntry::new(source, target));
    }
    report
}

/// 解析术语表文本，只取条目（跳过的行号见 [`parse_report`]）。
pub fn parse(text: &str) -> Vec<GlossaryEntry> {
    parse_report(text).entries
}

/// 把条目写回「一行一条 `原文=译文`」的文本（每条以 `\n` 收尾）。
///
/// 逐条**原样**写出（不 trim、不转义）：调用方要的是「保存我看到的这份」，静默改写
/// 字段等于替用户做决定。含首尾空白 / 换行 / 原文带 `=` 的条目本来就没法用这种平面
/// 格式无损表示，[`validate`] 会先把它们点出来。
///
/// 往返保证：对**合法**条目（无首尾空白、无换行、原文非空且不含 `=`、不以 `#` 开头）
/// 成立 `parse(to_text(e)) == e`（`parse` 会 trim，所以带空白的条目无法往返）。
pub fn to_text(entries: &[GlossaryEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&e.source);
        out.push(SEPARATOR);
        out.push_str(&e.target);
        out.push('\n');
    }
    out
}

/// 导出为 CSV：首行表头 `source,target`，字段按最小 RFC4180 规则转义。
///
/// 行尾用 `\n`（与本项目其它文本产物一致；[`from_csv`] 也读得进 `\r\n`）。
/// 字段**不 trim**：CSV 是无损交换格式，往返必须逐字节相等。
pub fn to_csv(entries: &[GlossaryEntry]) -> String {
    let mut out = String::new();
    out.push_str(CSV_HEADER[0]);
    out.push(',');
    out.push_str(CSV_HEADER[1]);
    out.push('\n');
    for e in entries {
        out.push_str(&escape_csv_field(&e.source));
        out.push(',');
        out.push_str(&escape_csv_field(&e.target));
        out.push('\n');
    }
    out
}

/// 最小 RFC4180 转义：字段里出现逗号 / 引号 / 换行时整体加双引号，内部引号翻倍。
///
/// 只在**需要时**才加引号：用户会用文本编辑器看这份文件，满屏 `"a","b"` 会让他以为
/// 导出坏了。规则本身必须完整（含 `\r`——表格软件的换行常是 `\r\n`）。
fn escape_csv_field(value: &str) -> String {
    if !value.contains([',', '"', '\n', '\r']) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// 解析 CSV（能读 [`to_csv`] 的输出，也读得进表格软件另存出来的版本）。
///
/// - 开头的 BOM 会被吃掉（另存为 UTF-8 时几乎都会带）；
/// - 首行若恰是表头 `source,target` 则跳过（大小写与首尾空白不敏感）；
/// - 字段**不做 trim**，保证 `from_csv(to_csv(e)) == e`；
/// - 畸形输入不 panic：只有一列的行按「有原文、译文为空」收下（[`validate`] 会报），
///   三列以上只取前两列，全空的行忽略。
pub fn from_csv(text: &str) -> Vec<GlossaryEntry> {
    let body = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut records = split_csv_records(body);
    if records.first().is_some_and(|first| is_header(first)) {
        records.remove(0);
    }
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        let mut fields = record;
        if fields.is_empty() {
            continue;
        }
        let source = std::mem::take(&mut fields[0]);
        let target = if fields.len() >= 2 {
            std::mem::take(&mut fields[1])
        } else {
            String::new()
        };
        // 两侧全空的行（空行、`,,`）没有内容可收：收下只会变成一条永远会被 validate
        // 报错、又删不掉的幽灵条目。
        if source.is_empty() && target.is_empty() {
            continue;
        }
        out.push(GlossaryEntry { source, target });
    }
    out
}

/// 把 CSV 正文切成「记录 → 字段」两层（引号内的逗号与换行不算分隔）。
///
/// 这段状态机只处理最小 RFC4180 子集：引号只在字段起始处有意义（我们自己导出的文件
/// 一定如此），字段中间的裸引号按普通字符收下——宁可宽容地多读进一条奇怪的术语，
/// 也不要因为一个多余引号把整份文件读成空表。
fn split_csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        // 翻倍的双引号 = 字段里的一个真引号
                        chars.next();
                        field.push('"');
                    } else {
                        in_quotes = false;
                    }
                }
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            ',' => record.push(std::mem::take(&mut field)),
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            '\r' => {
                // 单独的 `\r` 也算换行；`\r\n` 由后面的 `\n` 收尾。
                if chars.peek() != Some(&'\n') {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
            }
            _ => field.push(c),
        }
    }
    // 末行没有换行（用户手写的 CSV 常常如此）时也要收下。
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

/// 首行是否是我们自己写出的表头。
fn is_header(fields: &[String]) -> bool {
    fields.len() >= 2
        && fields[0].trim().eq_ignore_ascii_case(CSV_HEADER[0])
        && fields[1].trim().eq_ignore_ascii_case(CSV_HEADER[1])
}

/// [`merge`] 的统计：新增 / 覆盖 / 未变。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// incoming 里新出现的来源（追加到末尾）。
    pub added: usize,
    /// 命中了已有来源、且写回后内容**真的变了**的条目数。
    pub overwritten: usize,
    /// 命中了已有来源、且内容逐字相同的条目数（改了等于没改，界面不该报「已更新 8 条」）。
    pub unchanged: usize,
}

impl MergeReport {
    /// 一行摘要，例如 `新增 3 / 覆盖 1 / 未变 5`。三个数都留着 0 也要写出来：
    /// 只显示非零项时，「覆盖 0」与「没做这次合并」在界面上是同一句话。
    pub fn summary(&self) -> String {
        let (added, overwritten, unchanged) = (self.added, self.overwritten, self.unchanged);
        format!("新增 {added} / 覆盖 {overwritten} / 未变 {unchanged}")
    }
}

/// 合并两份术语表：按来源去重，**同名以 incoming 覆盖**。
///
/// 为什么 incoming 赢：新导入的那份就是用户此刻想要的结果——他刚在表格里改完译法，
/// 若旧表还能盖回去，那这次导入就等于白做。
///
/// 其余约定：
/// - **顺序**：先 base 的原顺序，再按 incoming 的顺序追加新条目。覆盖时**留在原位**
///   （术语表是用户按上下文排的序，覆盖不该把条目挪到末尾）；
/// - **大小写视作同一条**（见 [`normalize_key`]）；
/// - base 内部若已有重复来源，保留**第一条**；空来源（trim 后为空）两条都跳过
///   （空字符串做键会把所有空行挤成一条，反而把别的条目覆盖掉）。
pub fn merge(
    base: &[GlossaryEntry],
    incoming: &[GlossaryEntry],
) -> (Vec<GlossaryEntry>, MergeReport) {
    let mut out: Vec<GlossaryEntry> = Vec::with_capacity(base.len() + incoming.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for e in base {
        let key = normalize_key(&e.source);
        if key.is_empty() || index.contains_key(&key) {
            continue;
        }
        index.insert(key, out.len());
        out.push(e.clone());
    }

    let mut report = MergeReport::default();
    for e in incoming {
        let key = normalize_key(&e.source);
        if key.is_empty() {
            continue;
        }
        match index.get(&key).copied() {
            Some(pos) => {
                if out[pos] == *e {
                    report.unchanged += 1;
                } else {
                    out[pos] = e.clone();
                    report.overwritten += 1;
                }
            }
            None => {
                index.insert(key, out.len());
                out.push(e.clone());
                report.added += 1;
            }
        }
    }
    (out, report)
}

/// 术语的**身份键**：去首尾空白 + Unicode 小写。
///
/// 为什么忽略大小写：术语表的键是「一个词」，而 `OpenAI` / `openai` / `OPENAI` 指向
/// 同一个对象。不忽略的话，用户导入一份新表后提示词里会同时出现两条互相矛盾的译法，
/// 模型只能任选一条——术语表反而把一致性搞坏了。这类纯写法差异在品牌名、缩写、
/// 命令行参数上极其常见。
///
/// 口径取 Unicode 小写，与 `subtitle::segment::glossary_violations` 的比对口径一致：
/// 两处若各用一套（那边忽略大小写、这边不忽略），同一份术语表会在「哪些句术语违规」
/// 与「合并后有几条」上给出互相矛盾的结论。
fn normalize_key(source: &str) -> String {
    source.trim().to_lowercase()
}

/// 体检：返回可读的问题清单（空表 = 这份术语表没问题）。
///
/// 每一条都是「会让内容变形或让提示词收到坏数据」的实际问题，而不是风格建议：
/// 空原文、原文含 `=`、原文以 `#` 开头（写回文本会被当注释丢掉）、首尾空白（读回时被
/// trim 掉）、原文 / 译文含换行（平面格式表示不了）、译文为空（会给出一个空译法）、
/// 来源重复（忽略大小写）。位置用**1 起的条目序号**，与界面上的条目列表对得上号。
pub fn validate(entries: &[GlossaryEntry]) -> Vec<String> {
    let mut issues = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (offset, e) in entries.iter().enumerate() {
        let pos = offset + 1;
        let (source, target) = (e.source.as_str(), e.target.as_str());
        if source.contains(['\n', '\r']) {
            issues.push(format!("第 {pos} 条：原文含换行，无法用「一行一条」表示"));
        }
        if target.contains(['\n', '\r']) {
            issues.push(format!("第 {pos} 条：译文含换行，无法用「一行一条」表示"));
        }
        if source.trim().is_empty() {
            // 空原文连身份键都做不了，后面几项再报只会刷屏。
            issues.push(format!("第 {pos} 条：原文为空"));
            continue;
        }
        if source != source.trim() || target != target.trim() {
            issues.push(format!(
                "第 {pos} 条：首尾有空白，保存成文本再读回时会被去掉"
            ));
        }
        if source.contains(SEPARATOR) {
            issues.push(format!(
                "第 {pos} 条：原文含有 `{SEPARATOR}`，写回文本时会被当成分隔符切成两段"
            ));
        }
        if source.starts_with('#') {
            issues.push(format!(
                "第 {pos} 条：原文以 `#` 开头，写回文本后会被当成注释丢掉"
            ));
        }
        if target.trim().is_empty() {
            issues.push(format!(
                "第 {pos} 条：译文为空，注入提示词时会给出一个空译法"
            ));
        }
        let key = normalize_key(source);
        match seen.get(&key) {
            Some(&prev) => issues.push(format!(
                "第 {pos} 条：来源与第 {prev} 条重复（`{source}`，忽略大小写）"
            )),
            None => {
                seen.insert(key, pos);
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    // 刻意不用 `use super::*`：测试只依赖本模块的公开接口，任何一条实现细节改名都
    // 不会让测试「跟着一起改」，也就不会掩盖真正的行为变化。
    use super::{from_csv, merge, parse, parse_report, to_csv, to_text, validate, GlossaryEntry};

    fn entry(source: &str, target: &str) -> GlossaryEntry {
        GlossaryEntry::new(source, target)
    }

    /// 常规行：解析出条目、两侧 trim、空行与注释安静跳过。
    #[test]
    fn parse_trims_and_ignores_comment_and_blank_lines() {
        let text = "  OpenAI = 开放人工智能  \n\n# 这是注释\n   \n智能体=Agent\n";
        assert_eq!(
            parse(text),
            vec![entry("OpenAI", "开放人工智能"), entry("智能体", "Agent")]
        );
    }

    /// 没有 `=` 的行、以及只有一侧的行被跳过，并给出**行号**（1 起）。
    #[test]
    fn parse_report_lists_skipped_line_numbers() {
        let text = "OpenAI=开放人工智能\n这行忘了写等号\n\n# 注释\n=只有译文\n";
        let report = parse_report(text);
        assert_eq!(report.entries, vec![entry("OpenAI", "开放人工智能")]);
        assert_eq!(report.skipped_lines, vec![2, 5]);
    }

    /// 只按**第一个** `=` 切分：译文里带等号或查询串不会被切坏。
    #[test]
    fn parse_splits_on_first_equals_only() {
        assert_eq!(
            parse("地址=https://example.test/a=1"),
            vec![entry("地址", "https://example.test/a=1")]
        );
    }

    /// 文本往返：写回再读回逐条相等，顺序也一致。
    #[test]
    fn to_text_round_trips_through_parse() {
        let entries = vec![entry("OpenAI", "开放人工智能"), entry("token", "词元")];
        let text = to_text(&entries);
        assert!(text.ends_with('\n'), "每条以换行收尾：\n{text}");
        assert_eq!(parse(&text), entries);
    }

    /// CSV 转义：逗号、引号、换行都要能原样往返。
    #[test]
    fn csv_escapes_comma_quote_and_newline() {
        let entries = vec![
            entry("a,b", "含,逗号"),
            entry("引号\"在此", "他说\"好\""),
            entry("换\n行", "第二行"),
        ];
        let csv = to_csv(&entries);
        assert!(csv.starts_with("source,target\n"), "表头固定：\n{csv}");
        assert_eq!(from_csv(&csv), entries);
    }

    /// 读得进表格软件另存的 BOM + `\r\n`，也读得进没有表头的裸两列。
    #[test]
    fn from_csv_accepts_bom_crlf_and_headerless_input() {
        assert_eq!(
            from_csv("\u{FEFF}source,target\r\nOpenAI,开放人工智能\r\n"),
            vec![entry("OpenAI", "开放人工智能")]
        );
        assert_eq!(
            from_csv("OpenAI,开放人工智能\n"),
            vec![entry("OpenAI", "开放人工智能")]
        );
    }

    /// 空文本、只有表头 → 空表，且不 panic。
    #[test]
    fn from_csv_of_empty_or_header_only_is_empty() {
        assert!(from_csv("").is_empty());
        assert!(from_csv("source,target\n").is_empty());
    }

    /// 合并：新增 / 覆盖 / 未变三种结果各自计数，同名以 incoming 覆盖。
    #[test]
    fn merge_counts_added_overwritten_unchanged() {
        let base = vec![
            entry("OpenAI", "开放人工智能"),
            entry("token", "词元"),
            entry("agent", "代理"),
        ];
        let incoming = vec![
            entry("OpenAI", "开放AI"),
            entry("token", "词元"),
            entry("prompt", "提示词"),
        ];
        let (merged, report) = merge(&base, &incoming);
        assert_eq!(
            merged,
            vec![
                entry("OpenAI", "开放AI"),
                entry("token", "词元"),
                entry("agent", "代理"),
                entry("prompt", "提示词"),
            ]
        );
        assert_eq!(
            (report.added, report.overwritten, report.unchanged),
            (1, 1, 1)
        );
        assert_eq!(report.summary(), "新增 1 / 覆盖 1 / 未变 1");
    }

    /// 只差大小写的来源算**同一条**：以新导入的写法与译法为准。
    #[test]
    fn merge_treats_case_differing_sources_as_the_same_entry() {
        let (merged, report) = merge(
            &[entry("OpenAI", "开放人工智能")],
            &[entry("openai", "开放AI")],
        );
        assert_eq!(merged, vec![entry("openai", "开放AI")]);
        assert_eq!(
            (report.added, report.overwritten, report.unchanged),
            (0, 1, 0)
        );
    }

    /// 覆盖时留在原位：术语表是用户按上下文排的序，覆盖不该把条目挪到末尾。
    #[test]
    fn merge_keeps_position_when_overwriting() {
        let base = vec![entry("A", "甲"), entry("B", "乙"), entry("C", "丙")];
        let (merged, _) = merge(&base, &[entry("B", "新乙")]);
        assert_eq!(
            merged,
            vec![entry("A", "甲"), entry("B", "新乙"), entry("C", "丙")]
        );
    }

    /// 体检：空原文、原文含 `=`、含换行、来源重复各报一条可读问题。
    #[test]
    fn validate_reports_readable_problems() {
        let entries = vec![
            entry("  ", "空白原文"),
            entry("a=b", "等号原文"),
            entry("换\n行", "多行"),
            entry("OpenAI", "甲"),
            entry("openai", "乙"),
        ];
        let issues = validate(&entries);
        assert_eq!(issues.len(), 4, "四个问题：{issues:?}");
        assert!(issues.iter().any(|m| m.contains("原文为空")), "{issues:?}");
        assert!(issues.iter().any(|m| m.contains("`=`")), "{issues:?}");
        assert!(issues.iter().any(|m| m.contains("换行")), "{issues:?}");
        assert!(issues.iter().any(|m| m.contains("重复")), "{issues:?}");
    }

    /// 干净条目一条问题都不报（不然界面会把正常术语表标红）。
    #[test]
    fn validate_is_silent_for_clean_entries() {
        let entries = vec![entry("OpenAI", "开放人工智能"), entry("token", "词元")];
        assert!(validate(&entries).is_empty(), "{:?}", validate(&entries));
    }
}
