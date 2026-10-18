//! 复核表导出：把一次 [`QualityReport`] 与字幕片段渲染成可交付的复核清单。
//!
//! 应用已经能**发现**质量问题——低置信、术语违规、未翻译、空/超短句——界面也会把
//! 这些行高亮出来，但检测结果被困在界面里：一位在 Excel 里干活的审校，或一个问
//! 「你们标了哪几句？」的客户，手上什么都没有；也没有办法把复核任务交给别人。
//! 本模块把 `QualityReport` + `Segment` 变成一份复核表（CSV 或 Markdown），能直接
//! 在 Excel 打开，或粘进工单 / PR 描述。它只做格式化——不碰 IO、不碰 UI——因此
//! 完全可以单测。
//!
//! 两条贯穿全局的约定：
//!
//! - **一句一行，而不是「一行一个判据」**。审校拿到表是去改台词的，同一句若因为
//!   低置信 + 术语违规拆成两行，Excel 里筛选、排序、删除都会错位，还给人「有三句
//!   要改」的错觉。合并后「一行 = 一处要修的地方」，全部理由挤在「问题」一栏。
//! - **只汇报真正检查过的项**。SenseVoice 转写与旧记录不产生逐句 `confidence`，
//!   此时「置信度」一列全是占位符。只给一张表，客户会把空列读成「检查过、没问题」
//!   ——那是把「没测」说成了「通过」。所以那份报告一旦 [`QualityReport::confidence_unavailable`]，
//!   表尾必定追加一段明文说明。
//!
//! 本模块同时收着一条**不进复核表**的可读性判据：句尾标点缺失
//! （[`missing_terminal_punctuation`] / [`plan_punctuation_fix`]）。它的输出形态与复核表
//! 不同——给的是「哪几句值得看一眼」的下标——但同属质检，放在一起是为了让「字幕有
//! 哪些毛病」这件事只有一处清单；它**只报告、绝不改文本**，理由见该函数的文档。

use crate::subtitle::segment::{QualityCategory, QualityReport, Segment, MIN_EDIT_DUR};
use crate::subtitle::stats::{cps_thresholds, visible_char_count};
use crate::utils::time::seconds_to_timestamp;

/// 复核表的输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QcFormat {
    /// CSV（UTF-8 **带 BOM**，Excel 双击打开不乱码）
    Csv,
    /// Markdown 表格（贴进工单 / PR 描述）
    Markdown,
}

impl QcFormat {
    /// 文件扩展名（不含点）
    pub fn extension(self) -> &'static str {
        match self {
            QcFormat::Csv => "csv",
            QcFormat::Markdown => "md",
        }
    }

    /// 界面用中文名
    pub fn label(self) -> &'static str {
        match self {
            QcFormat::Csv => "CSV 复核表",
            QcFormat::Markdown => "Markdown 复核表",
        }
    }
}

/// 七列的表头文案。两种格式共用一份常量：各写一份，迟早会「CSV 改了列名、
/// Markdown 忘改」，下游脚本与工单模板就会按两种列名解析同一份数据。
const COLUMNS: [&str; 7] = ["序号", "开始", "时长", "问题", "置信度", "原文", "译文"];

/// 缺失值的占位符（`confidence` 为空、没有译文时用）。
const DASH: &str = "—";

/// 逐句置信度完全缺席时追加到表尾的说明。
///
/// 为什么必须写出来：没有这一句，空的「置信度」列本身就是一句假话——客户会读出
/// 「这些句子都检查过、都可靠」。它落在**表尾**而不是顶部：表体（要改的行）永远是
/// 第一眼看到的内容，说明是给「为什么这一列是空的」兜底。
const CONFIDENCE_NOTE: &str = "本次转写没有可用的逐句置信度（SenseVoice / 旧记录不产生该数据），\
     「置信度」列留空只表示「未评估」，并不代表这些句子已经检查过。";

/// 生成复核表。`report` 由调用方先用 [`quality_report`](super::segment::quality_report) 算好。
pub fn render_review_sheet(
    segments: &[Segment],
    report: &QualityReport,
    format: QcFormat,
) -> String {
    let rows = collect_rows(segments, report);
    let mut out = String::new();
    // BOM 必须在最前面：Excel 双击打开 CSV 时靠它判定「这是 UTF-8」，缺了中文
    // 会被按本地代码页（GBK）解释成乱码。Markdown 反过来**绝不能**带 BOM——它会
    // 渲染成表格前的一个多余字符。
    if format == QcFormat::Csv {
        out.push('\u{FEFF}');
    }
    out.push_str(&render_summary(segments, report, rows.len(), format));
    out.push('\n');

    if rows.is_empty() {
        // 没有待办也要给出结论：留着空表体，用户会以为下面漏了行。
        out.push_str("未发现需要复核的句子。\n");
    } else {
        out.push_str(&render_table(&rows, format));
    }

    if report.confidence_unavailable() {
        out.push_str(&render_confidence_note(format));
    }
    out
}

/// 复核表的文件名主体建议（调用方再补扩展名与目录）。
///
/// 为什么必须有这个后缀：复核表默认跟字幕导出到同一个目录（`课程01.srt` 旁边），
/// 文件名相同就会**直接覆盖**刚交付出去的字幕——一份已经发给客户的产出被质检清单
/// 顶掉，是不可逆的数据丢失。加了「-质检」后 `课程01.srt` 与 `课程01-质检.csv`
/// 并存，用户也一眼看得出哪份是产出、哪份是待办。
pub fn review_file_stem(source_stem: &str) -> String {
    let stem = source_stem.trim();
    if stem.is_empty() {
        // 名字为空时直接拼接会得到 `-质检` 这种以连字符开头的文件名（在类 Unix 下
        // 看起来像隐藏文件），退回一个自明的固定名，用户自己还会再改名。
        return "质检".to_string();
    }
    format!("{stem}-质检")
}

/// 一条待复核记录：一句台词 + 它命中的全部判据标签。
struct IssueRow<'a> {
    seg: &'a Segment,
    reasons: Vec<&'static str>,
}

/// 把报告里的句序号变成「一句一行」的行列表（升序）。
fn collect_rows<'a>(segments: &'a [Segment], report: &QualityReport) -> Vec<IssueRow<'a>> {
    // 先把每类的句序号取一次（`indices` 自带去重），再在行循环里查表；否则
    // 「每行 × 每类」都会重算一遍同一份桶。
    let buckets: Vec<(QualityCategory, Vec<usize>)> = QualityCategory::ALL
        .iter()
        .map(|cat| (*cat, cat.indices(report)))
        .collect();

    // all_issues() 自带升序去重——直接拿它当行序，与界面「一键定位」的推进顺序一致。
    report
        .all_issues()
        .into_iter()
        .filter_map(|index| {
            // 报告里的序号在 `segments` 找不到对应句时跳过该行而不 panic：报告与片段
            // 可能来自两次快照（导出期间用户刚删了句）。为一行孤儿数据让整份导出崩掉，
            // 是把小瑕疵放大成事故。
            let seg = segments.iter().find(|s| s.index == index)?;
            let reasons = buckets
                .iter()
                .filter(|(_, indices)| indices.contains(&index))
                .map(|(cat, _)| cat.label())
                .collect();
            Some(IssueRow { seg, reasons })
        })
        .collect()
}

/// 概览段：总句数、待复核句数、每类计数。
fn render_summary(
    segments: &[Segment],
    report: &QualityReport,
    flagged: usize,
    format: QcFormat,
) -> String {
    let (title, bullet) = match format {
        QcFormat::Csv => ("# 字幕复核表\n", "# "),
        QcFormat::Markdown => ("# 字幕复核表\n\n", "- "),
    };
    let mut out = String::from(title);
    out.push_str(&format!("{bullet}总句数：{}\n", segments.len()));
    out.push_str(&format!("{bullet}待复核：{flagged} 句\n"));
    // 每类都列出来（哪怕是 0 句）：用户要看得出「这一项确实查过、结果是 0」，
    // 而不是被悄悄藏掉——与转写页质检卡胶囊同一套语义、同一批 `label()` 文案。
    for cat in QualityCategory::ALL {
        let count = cat.indices(report).len();
        out.push_str(&format!("{bullet}{}：{count} 句\n", cat.label()));
    }
    out
}

/// 表体：表头 + 每一行数据。
fn render_table(rows: &[IssueRow<'_>], format: QcFormat) -> String {
    let mut out = String::new();
    match format {
        QcFormat::Csv => {
            out.push_str(&COLUMNS.join(","));
            out.push('\n');
            for row in rows {
                let cells: Vec<String> = row_cells(row)
                    .iter()
                    .map(|c| csv_field(c.as_str()))
                    .collect();
                out.push_str(&cells.join(","));
                out.push('\n');
            }
        }
        QcFormat::Markdown => {
            out.push_str(&format!("| {} |\n", COLUMNS.join(" | ")));
            let separator = vec!["---"; COLUMNS.len()];
            out.push_str(&format!("| {} |\n", separator.join(" | ")));
            for row in rows {
                let cells: Vec<String> = row_cells(row)
                    .iter()
                    .map(|c| markdown_cell(c.as_str()))
                    .collect();
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
        }
    }
    out
}

/// 一行的七个单元格（顺序与 [`COLUMNS`] 一一对应）。
fn row_cells(row: &IssueRow<'_>) -> [String; 7] {
    let seg = row.seg;
    [
        seg.index.to_string(),
        seconds_to_timestamp(seg.start),
        format!("{:.2}", duration_secs(seg)),
        row.reasons.join("、"),
        seg.confidence
            .map(|c| format!("{c}"))
            .unwrap_or_else(|| DASH.to_string()),
        seg.display_text().to_string(),
        translation_cell(seg),
    ]
}

/// 句时长（秒）。倒挂或负时长钳到 0：表里出现 `-1.20` 会让人以为时间轴坏了，
/// 而这里能如实表达的只有「看不清时长」，那不如写 0。
fn duration_secs(seg: &Segment) -> f64 {
    (seg.end - seg.start).max(0.0)
}

/// 译文单元格：没有译文（含空串 / 纯空白）时用占位符。
fn translation_cell(seg: &Segment) -> String {
    if seg.has_translation() {
        seg.translation
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string()
    } else {
        DASH.to_string()
    }
}

/// 能力缺口说明：CSV 走注释行、Markdown 走引用块，正文共用同一份文案。
fn render_confidence_note(format: QcFormat) -> String {
    match format {
        QcFormat::Csv => format!("# 注意：{CONFIDENCE_NOTE}\n"),
        QcFormat::Markdown => format!("> **注意**：{CONFIDENCE_NOTE}\n"),
    }
}

/// CSV 字段转义：先把公式前缀钉成文本，再把含 `,` / `"` / 换行的字段整体加双引号，
/// 内部的 `"` 翻倍。
///
/// 字幕文本里逗号与引号极常见（对话、术语、引用）。裸拼的 CSV 会被 Excel 静默按
/// 逗号切列——打开看像成功，实则整列数据错位到相邻列，是「看着没问题、内容全错」
/// 的那类 bug。
///
/// 公式注入同理：首个字符是 `=` `+` `@`、制表符或回车时，Excel/WPS 会按公式求值
/// （`=cmd` 之类能拉起外部程序），必须前置单引号钉成文本；前导 `-` 只在后面不是
/// 数字时才需要防护。
fn csv_field(value: &str) -> String {
    // 单引号前缀要在双引号包裹之前加：既危险又含逗号的字段应得 `"'=a,b"`。
    let guarded = match value.chars().next() {
        // `=` / `+` / `@` / 制表符 / 回车开头一律钉成文本（`+1` 会被 Excel/WPS 补成
        // 公式 `=+1`）。
        Some('=' | '+' | '@' | '\t' | '\r') => format!("'{value}"),
        // 前导 `-` 只有后面不是合法数字时才有公式风险（`-2+3`）；`-0.9` 这类纯数值
        // Excel 按数字解析，加前缀反而会把置信度列变成文本。
        Some('-') if value.parse::<f64>().is_err() => format!("'{value}"),
        _ => value.to_string(),
    };
    let needs_quoting = guarded.contains(',')
        || guarded.contains('"')
        || guarded.contains('\n')
        || guarded.contains('\r');
    if !needs_quoting {
        return guarded;
    }
    let escaped = guarded.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

/// Markdown 单元格转义：`|` 变 `\|`，换行变 `<br>`。
///
/// 裸 `|` 会被解析成列分隔符，把一行拆成错位的多列；裸换行直接把表格从中间截断，
/// 后半张表渲染成一堆普通段落。两种都只在「字幕里恰好有这些字符」时才出现，
/// 所以必须在这里一次性处理干净。
fn markdown_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
}

/// 「这一句已经说完」的句尾符号。
///
/// 纳入的不只是句号类：破折号（被打断的台词）、省略号、波浪号（拖长的语气）、音符
/// （歌词行）在字幕里都是**有意**的收尾写法。把它们排除在外，一份歌词或访谈字幕会
/// 整片飘红，复核清单立刻失去信噪比——宁可信用户，也不要把正常写法当缺陷。
const TERMINAL_MARKS: &[char] = &[
    '。', '．', '！', '？', '｡', '…', '‥', // 句末（汉字 / 全角 / 半角）
    '.', '!', '?', // 拉丁句末
    '—', '–', '―', // 破折号：被打断的台词
    '~', '～', // 拖音 / 语气
    '♪', '♫', // 歌词句
];

/// 句尾允许出现的**包裹符号**：先跳过它们，再看里面那个字符。
///
/// 不然 `他说"好。"` 会因为末尾多出一个引号而误报——引号是标点的一部分，不是
/// 「句尾没有标点」。
const CLOSERS: &[char] = &[
    '"', '\'', '”', '’', '」', '』', '）', ')', '】', ']', '》', '〉', '〕', '｝', '}',
];

/// 文本「句尾有收尾标点」的判定：从后往前跳过空白与包裹符号，看第一个真字符。
pub fn ends_with_terminal_punctuation(text: &str) -> bool {
    for c in text.chars().rev() {
        if c.is_whitespace() || CLOSERS.contains(&c) {
            continue;
        }
        return TERMINAL_MARKS.contains(&c);
    }
    false
}

/// 句尾标点的复核计划。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PunctuationPlan {
    /// 句尾没有收尾标点、值得看一眼的句子下标（按片段顺序升序；同一句只出现一次）。
    pub indices: Vec<usize>,
    /// 无法判定的句子数（空文本 / 纯空白 / 全标点）：它们**没有**被算成「没问题」。
    pub skipped: usize,
}

/// 句尾标点的复核计划：只报告，绝不自动补。
///
/// 判定：文本里有实际内容（至少一个字母 / 汉字 / 数字），但去掉空白与句尾引号、括号后，
/// 最后一个字符不在 [`TERMINAL_MARKS`] 里。
///
/// # 为什么明确**不做**自动补标点
///
/// 补标点必须猜这句是陈述、疑问还是感叹：`你确定` 补成 `你确定。` 与 `你确定？` 是
/// 完全相反的语气。字幕是**交付物**，正文属于用户——工具替用户改正文，比漏报一句标点
/// 严重得多。所以这里只产出「哪几句值得看一眼」，改法留给用户（界面可以把这些句子
/// 一键定位过去）。
///
/// 空文本、纯空白、全标点（`。。。` / `♪`）不报：它们不是句子，报了只是噪声。这些
/// 「没法判定」的句子数记在 [`PunctuationPlan::skipped`] 里如实交代，避免用户把
/// 「没检查」读成「通过」。
pub fn plan_punctuation_fix(segments: &[Segment]) -> PunctuationPlan {
    let mut plan = PunctuationPlan::default();
    for seg in segments {
        let text = seg.display_text();
        // 一个字都数不出字母/汉字/数字的，不是句子：空行、纯空白、以及 `♪` 这种
        // 歌曲标记行都归到「没法判定」，而不是「句尾没标点」。
        if !text.chars().any(char::is_alphanumeric) {
            plan.skipped += 1;
            continue;
        }
        if !ends_with_terminal_punctuation(text) && !plan.indices.contains(&seg.index) {
            plan.indices.push(seg.index);
        }
    }
    plan
}

/// 句尾标点缺失的句子下标（= [`plan_punctuation_fix`] 的下标，语义见其文档）。
pub fn missing_terminal_punctuation(segments: &[Segment]) -> Vec<usize> {
    plan_punctuation_fix(segments).indices
}

/// 相邻片段「时间交叠」的容差（秒）。
///
/// 为什么非要这个常量：本项目所有导入/导出格式都是毫秒量级取整（SRT/VTT 的
/// `.mmm`、FCPXML 的有理帧号、内部 `f64` 秒），两句**本想首尾相接**的台词经过
/// 换算后可能差出一个远小于 1ms 的浮点尾巴。这种交叠在播放上根本不可见，报出来
/// 只会稀释清单的信噪比。1ms 恰是这些格式的最细刻度：小于它的差异不可能是用户
/// 能感知、值得动手改的真实交叠。
const OVERLAP_TOLERANCE_SECS: f64 = 0.001;

/// 时间交叠的句子下标：`seg[i].end` 明显晚于 `seg[i+1].start` 时返回**前一句**
/// 的下标（要收紧的是前一句的 `end`，不是后一句的 `start`——后一句的入场时间通常
/// 已经对上了画面/说话起点）。
///
/// 只比对**相邻**两句：字幕里真正要处理的交叠几乎都发生在时间轴接缝，隔句的「重叠」
/// 多半是正常的插叙，报了反成噪声。回退语义与同模块其它判据一致：只报告、绝不改时间。
///
/// 容差见 [`OVERLAP_TOLERANCE_SECS`]：恰好相接（`end == next.start`）与浮点噪声级的
/// 交叠都不报。
pub fn overlapping_segments(segments: &[Segment]) -> Vec<usize> {
    let mut out = Vec::new();
    for pair in segments.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        if prev.end > next.start + OVERLAP_TOLERANCE_SECS {
            out.push(prev.index);
        }
    }
    out
}

/// 相邻两句**原文**规范化后相同的**后一句**下标。
///
/// 规范化只做两件保守的事：去掉全部空白、把 ASCII 字母折叠成小写。为什么不再多剥一层
/// （例如像「修复常见错误」那样连标点一起丢掉）：那套激进规范化是「折叠删除」的前置
/// 判断；这里只产出「值得看一眼」，误报会直接消耗清单的可信度，所以仅在「两串文本除
/// 空白与大小写外逐字相同」时才报。
///
/// 空白句（空串 / 纯空白）不参与：它们归核心四类的「空/超短句」，在这里再点一次名只是
/// 噪声。与同模块其它判据一致：只报告、绝不改文本（删哪句、留哪句由用户定）。
pub fn duplicate_neighbors(segments: &[Segment]) -> Vec<usize> {
    let mut out = Vec::new();
    for pair in segments.windows(2) {
        let a = normalize_for_adjacent_compare(pair[0].display_text());
        let b = normalize_for_adjacent_compare(pair[1].display_text());
        if !a.is_empty() && a == b {
            out.push(pair[1].index);
        }
    }
    out
}

/// 相邻重复句的文本规范化：去全部空白 + ASCII 大小写折叠（取值理由见
/// [`duplicate_neighbors`]）。
fn normalize_for_adjacent_compare(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// 「一键修复质检项」的纯函数计划层
// ---------------------------------------------------------------------------
//
// 这一层只负责**算出要改什么**：不碰 IO、不碰 UI、不改输入，也不落库。界面先拿
// [`plan_repairs`] 显示「将改动 N 句」并让用户预览，确认后再调 [`apply_repairs`]。
// 拆成两步是为了可撤销：计划本身是一份可审阅的数据（`before` / `after`），用户能看到
// 每一句会被改成什么样，而不是点一下按钮就让正文和时间轴一起静默变化。
//
// 与同模块其它判据（[`missing_terminal_punctuation`] 等）的关系：那些函数**只报告**，
// 这里则给出**具体改法**。自动补标点在本项目原本是明确禁止的（会猜错语气），因此
// 本计划只补最保守的「。」/「，」：间隔够大才断句，句中是逗号，不做疑问/感叹猜测。

/// 重叠修复时相邻两句之间保留的空隙（秒）。
///
/// 20ms 是「上一句先消失、下一句再出现」的最小可见差；再小就又变成视觉上的相贴。
const REPAIR_OVERLAP_GAP_SECS: f64 = 0.02;

/// 「下一句隔了这么久就算另起一句」的阈值（秒）。
///
/// 低于它的相邻句视为同一段话的延续，补逗号；超过它（或已是片尾）补句号。
const REPAIR_SENTENCE_GAP_SECS: f64 = 0.8;

/// 时间与速度比较的浮点容差：只用来吸掉 `end - start` 这类减法的尾数，不改变阈值语义。
const REPAIR_EPS: f64 = 1e-9;

/// 除 [`TERMINAL_MARKS`] 之外、用户已经写下的「不需要再补」句尾符号。
///
/// 逗号/分号/冒号在句末不是标准收尾，但**用户写了就是用户的选择**：再补一个「，」
/// 会变成「，。」这种明显错误。修复计划宁可不碰，也不叠加第二个符号。
const REPAIR_EXTRA_ENDINGS: &[char] = &['，', '；', '：', '、', ',', ';', ':'];

/// 一条自动修复建议的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairKind {
    /// 句末缺标点（补「。」或「，」）
    Punctuation,
    /// 相邻两句时间重叠
    Overlap,
    /// 阅读速度（CPS）超限
    Cps,
    /// 相邻重复句（删空文本，不真的删数组元素）
    Duplicate,
}

impl RepairKind {
    /// 固定顺序。`counts()` 与界面胶囊都按它排列，避免每次调用顺序漂移。
    pub const ALL: [RepairKind; 4] = [
        RepairKind::Punctuation,
        RepairKind::Overlap,
        RepairKind::Cps,
        RepairKind::Duplicate,
    ];

    /// 界面用中文名。
    pub fn label(self) -> &'static str {
        match self {
            RepairKind::Punctuation => "句末标点",
            RepairKind::Overlap => "时间重叠",
            RepairKind::Cps => "语速超限",
            RepairKind::Duplicate => "相邻重复",
        }
    }
}

/// 一条修复建议。
///
/// `start` / `end` / `text` 三者**只填真正要改的字段**：`None` = 这一项不动它。
/// [`apply_repairs`] 严格按这个约定落地，所以「计划里没给的字段」永远不会被顺手改掉。
#[derive(Debug, Clone, PartialEq)]
pub struct RepairItem {
    /// 修复类型
    pub kind: RepairKind,
    /// 句子 `index`（不是列表位置——界面与报告都用 `Segment::index` 对齐）
    pub index: usize,
    /// 演示用：修复前的文本（时间类为 `开始–结束` 时间码）
    pub before: String,
    /// 演示用：修复后的文本（时间类为时间码，重复句为空串）
    pub after: String,
    /// 时间类修复的新起点（`None` = 不动起点）
    pub start: Option<f64>,
    /// 时间类修复的新终点（`None` = 不动终点）
    pub end: Option<f64>,
    /// 文本类修复的新文本（`Some("")` 表示删空，见 [`RepairKind::Duplicate`]）
    pub text: Option<String>,
}

/// 一次「一键修复」的完整计划。顺序即应用顺序（见 [`plan_repairs`]）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepairPlan {
    pub items: Vec<RepairItem>,
}

impl RepairPlan {
    /// 没有任何可修的项。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 按 [`RepairKind::ALL`] 的固定顺序给出每类条数（含 0 条）。
    pub fn counts(&self) -> [(RepairKind, usize); 4] {
        let mut out = [
            (RepairKind::Punctuation, 0usize),
            (RepairKind::Overlap, 0usize),
            (RepairKind::Cps, 0usize),
            (RepairKind::Duplicate, 0usize),
        ];
        for item in &self.items {
            for slot in out.iter_mut() {
                if slot.0 == item.kind {
                    slot.1 += 1;
                }
            }
        }
        out
    }
}

/// 生成修复计划：缺句末标点、相邻句时间重叠、CPS 超限、相邻重复句。
///
/// 纯函数：只读 `segments`，返回新计划；同一份输入永远得到同一份计划。
///
/// # 顺序与相互影响
///
/// 条目按「时间类在前、文本类在后」生成：重叠 → CPS → 重复句 → 标点。生成时维护一份
/// 工作用时间轴，后一个判据看到的是前一个判据改过之后的时间，因此计划内部不会互相
/// 打架（例如 CPS 拉长 `end` 的幅度已按重叠修复后的下一句起点截断）。
///
/// - **标点**：只给句末补「。」/「，」——下一句间隔 > 0.8s 或已是片尾补句号，否则补
///   逗号；已经以 [`TERMINAL_MARKS`] 或 [`REPAIR_EXTRA_ENDINGS`] 收尾的句子、纯符号行
///   一律跳过。
/// - **重叠**：只处理相邻两句，把前一句 `end` 压到 `cur.start - 0.02`；若这样会把
///   前一句压到 [`MIN_EDIT_DUR`] 以下，则改为把后一句 `start` 推到 `prev.end + 0.02`，
///   保住前一句的完整性（后一句连这个位置都放不下时，退回收前一句，至少消掉重叠）。
/// - **CPS**：字符数用 [`visible_char_count`]，阈值用 [`cps_thresholds`] 的舒适上限
///   （中日韩 9、拉丁 17 字符/秒）。超限时把 `end` 拉到 `start + chars / 阈值`，上界
///   是下一句 `start`（函数签名没有媒体总时长，末句因此只受自身文本约束）。
/// - **重复**：相邻两句归一化（去空白与标点、小写）后完全相同 → 计划把后一句文本
///   清空（`text: Some(String::new())`）。**不从数组里删**：删元素会让 `index` 与
///   界面选中项错位，清空是可撤销的最小动作。
pub fn plan_repairs(segments: &[Segment]) -> RepairPlan {
    let mut plan = RepairPlan::default();
    if segments.is_empty() {
        return plan;
    }

    // 工作用时间轴：后续判据基于「前面已计划」的时间，避免两条建议对同一字段给出
    // 互相矛盾的目标值。
    let mut times: Vec<(f64, f64)> = segments.iter().map(|s| (s.start, s.end)).collect();

    // 重复句先判定：被计划删空的句子不再参与标点修复，否则「清空」与「补标点」
    // 会写同一个字段，谁后应用谁说话。
    let mut duplicate_positions = vec![false; segments.len()];
    for pos in 1..segments.len() {
        let a = normalize_for_repair(segments[pos - 1].display_text());
        let b = normalize_for_repair(segments[pos].display_text());
        if !a.is_empty() && a == b {
            duplicate_positions[pos] = true;
        }
    }

    // 1. 时间重叠：优先收前一句的 end，收不下才推后一句的 start。
    for pos in 1..segments.len() {
        let (prev_start, prev_end) = times[pos - 1];
        let (cur_start, cur_end) = times[pos];
        // 与 [`overlapping_segments`] 同口径：浮点噪声级交叠不算问题。
        if prev_end <= cur_start + OVERLAP_TOLERANCE_SECS {
            continue;
        }
        let compressed_end = cur_start - REPAIR_OVERLAP_GAP_SECS;
        if compressed_end - prev_start >= MIN_EDIT_DUR {
            times[pos - 1].1 = compressed_end;
            plan.items.push(RepairItem {
                kind: RepairKind::Overlap,
                index: segments[pos - 1].index,
                before: range_label(prev_start, prev_end),
                after: range_label(prev_start, compressed_end),
                start: None,
                end: Some(compressed_end),
                text: None,
            });
            continue;
        }

        let pushed_start = prev_end + REPAIR_OVERLAP_GAP_SECS;
        if cur_end - pushed_start >= MIN_EDIT_DUR {
            times[pos].0 = pushed_start;
            plan.items.push(RepairItem {
                kind: RepairKind::Overlap,
                index: segments[pos].index,
                before: range_label(cur_start, cur_end),
                after: range_label(pushed_start, cur_end),
                start: Some(pushed_start),
                end: None,
                text: None,
            });
        } else {
            // 两边都放不下：仍然收前一句（用户最直接的诉求是「别叠在一起」），
            // 哪怕它会短于下限——这比留下可见的重叠更接近目标状态。
            times[pos - 1].1 = compressed_end;
            plan.items.push(RepairItem {
                kind: RepairKind::Overlap,
                index: segments[pos - 1].index,
                before: range_label(prev_start, prev_end),
                after: range_label(prev_start, compressed_end),
                start: None,
                end: Some(compressed_end),
                text: None,
            });
        }
    }

    // 2. 阅读速度：按字符集分档，拉长 end 直到读得完，但不越过下一句起点。
    for pos in 0..segments.len() {
        let (start, end) = times[pos];
        let text = segments[pos].display_text();
        let chars = visible_char_count(text);
        if chars == 0 {
            continue;
        }
        let (comfortable, _too_fast) = cps_thresholds(text);
        if !comfortable.is_finite() || comfortable <= 0.0 {
            continue;
        }
        let duration = end - start;
        let cps = if duration > REPAIR_EPS {
            chars as f64 / duration
        } else {
            f64::INFINITY
        };
        if cps <= comfortable + REPAIR_EPS {
            continue;
        }
        let wanted_end = start + chars as f64 / comfortable;
        // 上界 = 下一句 start：签名里没有媒体总时长，末句只能按自身文本拉长。
        let upper_bound = times
            .get(pos + 1)
            .map(|&(next_start, _)| next_start)
            .unwrap_or(f64::INFINITY);
        let new_end = wanted_end.min(upper_bound);
        if !new_end.is_finite() || new_end <= end + REPAIR_EPS || new_end <= start {
            continue;
        }
        times[pos].1 = new_end;
        plan.items.push(RepairItem {
            kind: RepairKind::Cps,
            index: segments[pos].index,
            before: range_label(start, end),
            after: range_label(start, new_end),
            start: None,
            end: Some(new_end),
            text: None,
        });
    }

    // 3. 相邻重复句：清空后一句的文本（不删元素）。
    for pos in 1..segments.len() {
        if !duplicate_positions[pos] {
            continue;
        }
        let seg = &segments[pos];
        plan.items.push(RepairItem {
            kind: RepairKind::Duplicate,
            index: seg.index,
            before: seg.display_text().to_string(),
            after: String::new(),
            start: None,
            end: None,
            text: Some(String::new()),
        });
    }

    // 4. 句末标点：片尾或间隔够大补「。」，否则补「，」。
    for pos in 0..segments.len() {
        if duplicate_positions[pos] {
            continue;
        }
        let text = segments[pos].display_text();
        // 与 [`plan_punctuation_fix`] 同口径：数不出字母/汉字/数字的行不是句子。
        if !text.chars().any(char::is_alphanumeric) {
            continue;
        }
        if ends_with_any_punctuation(text) {
            continue;
        }
        let end = times[pos].1;
        let mark = match times.get(pos + 1) {
            None => '。',
            Some(&(next_start, _)) if next_start - end > REPAIR_SENTENCE_GAP_SECS => '。',
            Some(_) => '，',
        };
        let after = format!("{text}{mark}");
        plan.items.push(RepairItem {
            kind: RepairKind::Punctuation,
            index: segments[pos].index,
            before: text.to_string(),
            after: after.clone(),
            start: None,
            end: None,
            text: Some(after),
        });
    }

    plan
}

/// 应用计划，返回**被实际改动的句子条数**（同一句多个字段只算一条）。
///
/// 只改 `item` 里给了值的字段；找不到 `index` 的条目静默跳过（与复核表的孤儿行同
/// 策略：计划与片段可能来自两次快照，不该为一条过期建议 panic）。
///
/// 文本字段的落点：删除（空文本）同时清 `text` 与 `polished`；新增/替换文本时优先写
/// 非空的 `polished`（界面展示的就是它），没有润色文本才写 `text`，避免把原始转写
/// 覆盖成「带标点的展示文本」。
///
/// 幂等：同一份计划连续应用两次，第二次返回 0——每条建议写的都是**绝对值**，不是
/// 「在原值上追加」。
pub fn apply_repairs(segments: &mut [Segment], plan: &RepairPlan) -> usize {
    let mut touched = vec![false; segments.len()];
    for item in &plan.items {
        let Some(pos) = segments.iter().position(|s| s.index == item.index) else {
            continue;
        };
        let seg = &mut segments[pos];
        let before_start = seg.start;
        let before_end = seg.end;
        let before_text = seg.text.clone();
        let before_polished = seg.polished.clone();

        if let Some(start) = item.start {
            seg.start = start;
        }
        if let Some(end) = item.end {
            seg.end = end;
        }
        if let Some(text) = &item.text {
            if text.trim().is_empty() {
                seg.text.clear();
                seg.polished.clear();
            } else if seg.polished.trim().is_empty() {
                seg.text = text.clone();
            } else {
                seg.polished = text.clone();
            }
        }

        // 源文一变，**旧译文与词级时间戳立刻失效**：
        // - 译文是按旧原文翻的，留着会让 `translation_matches` 判「已完成」，
        //   后续翻译永远跳过它，导出里是「新原文 + 旧译文」的张冠李戴；
        // - 词级时间戳描述的是旧 token，卡拉OK 会拿旧时长点亮新字。
        // 这与手工改字（`save_selected_text` 的 P0-2）走同一条失效规则。
        let text_changed = seg.text != before_text || seg.polished != before_polished;
        if text_changed {
            seg.words.clear();
            seg.translation = None;
            seg.translation_lang = None;
            seg.translations.clear();
        }

        let changed = seg.start != before_start
            || seg.end != before_end
            || seg.text != before_text
            || seg.polished != before_polished;
        if changed {
            touched[pos] = true;
        }
    }
    touched.into_iter().filter(|changed| *changed).count()
}

/// 时间码区间文案（演示用 `开始–结束`，与计划项的 `before` / `after` 同格式）。
fn range_label(start: f64, end: f64) -> String {
    format!(
        "{}–{}",
        seconds_to_timestamp(start),
        seconds_to_timestamp(end)
    )
}

/// 句子结尾是否已经有任意标点（[`TERMINAL_MARKS`] 或 [`REPAIR_EXTRA_ENDINGS`]）。
///
/// 与 [`ends_with_terminal_punctuation`] 一样先跳过空白与包裹符号：`他说"好，"` 的
/// 句尾是逗号，不该再补一个。
fn ends_with_any_punctuation(text: &str) -> bool {
    if ends_with_terminal_punctuation(text) {
        return true;
    }
    for c in text.chars().rev() {
        if c.is_whitespace() || CLOSERS.contains(&c) {
            continue;
        }
        return REPAIR_EXTRA_ENDINGS.contains(&c);
    }
    false
}

/// 重复句归一化：只保留字母/汉字/数字，再整体小写。
///
/// 与 [`normalize_for_adjacent_compare`] 不同，这里连标点一起丢掉——修复计划要**删**
/// 句子，判定必须比「只报告」更严格：`你好。` 与 `你好！` 归一化后相同，确实该删其一；
/// 而报告路径保留标点差异，避免误报消耗清单可信度。
fn normalize_for_repair(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subtitle::segment::{quality_report, DEFAULT_LOW_CONFIDENCE_THRESHOLD};

    /// 造一句带（可选）译文的片段。七处样板收成一个构造器，测试正文才能聚焦在
    /// 「断言什么」而不是「怎么拼字段」。
    fn seg(index: usize, start: f64, end: f64, text: &str, translation: Option<&str>) -> Segment {
        let mut s = Segment::new(index, start, end, text);
        s.translation = translation.map(str::to_string);
        s
    }

    /// 同一句命中两类问题时只出现一行，且两类理由合并在「问题」一栏。
    ///
    /// Why：复核者按行改台词。若按 (行 × 判据) 展开，同一句会出现两三遍，Excel 里
    /// 排序、筛选、删除全部错位，还给人「有三句要改」的错觉。
    #[test]
    fn multi_hit_line_appears_once_with_all_reasons() {
        let mut multi = seg(1, 1.0, 3.0, "机器学习", Some("machine learning"));
        multi.confidence = Some(-0.9);
        let segs = vec![multi];
        let entries = vec![("机器".to_string(), "machinery".to_string())];
        let report = quality_report(&segs, &entries, DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert_eq!(report.low_confidence, vec![1]);
        assert_eq!(
            report.glossary_violations,
            vec![1],
            "术语在原文出现、译文缺席 → 违规"
        );

        let sheet = render_review_sheet(&segs, &report, QcFormat::Csv);
        let row = "1,00:00:01.000,2.00,低置信、术语违规,-0.9,机器学习,machine learning";
        assert_eq!(sheet.matches(row).count(), 1, "同一句只能占一行：\n{sheet}");
        let data_rows = sheet.lines().filter(|l| l.starts_with("1,")).count();
        assert_eq!(data_rows, 1, "数据行也只该有一条：\n{sheet}");
    }

    /// 报告内部桶的顺序与字幕顺序不一致时，表里的行仍按句序号升序排列。
    ///
    /// Why：报告由几条路径拼出来（术语违规来自另一次扫描），桶顺序不保证与字幕一致。
    /// 审校从上往下改，行序必须是字幕顺序，否则每次都要在时间轴上前后乱跳。
    #[test]
    fn rows_are_ascending_regardless_of_bucket_order() {
        let mut a = seg(9, 9.0, 10.0, "第九句", Some("nine"));
        a.confidence = Some(-0.9);
        let mut b = seg(3, 3.0, 4.0, "第三句", Some("three"));
        b.confidence = Some(-0.8);
        let mut c = seg(5, 5.0, 6.0, "第五句", Some("five"));
        c.confidence = Some(-0.7);
        // 故意把片段列表打乱，让 quality_report 的桶按输入顺序 push。
        let segs = vec![a, b, c];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert_eq!(report.all_issues(), vec![3, 5, 9]);

        let sheet = render_review_sheet(&segs, &report, QcFormat::Csv);
        let order: Vec<usize> = sheet
            .lines()
            .filter_map(|l| l.split(',').next())
            .filter_map(|first| first.parse::<usize>().ok())
            .collect();
        assert_eq!(order, vec![3, 5, 9], "行序 = 句序号升序：\n{sheet}");
    }

    /// CSV 转义：含逗号 / 引号 / 换行的字段加引号并把内部引号翻倍；普通字段原样。
    ///
    /// Why：字幕文本里逗号和引号极常见。裸拼的 CSV 会被 Excel 静默切列——打开像成功，
    /// 内容已全错。
    #[test]
    fn csv_field_quotes_only_when_needed() {
        assert_eq!(csv_field("普通文本"), "普通文本", "不需要引号时保持原样");
        assert_eq!(csv_field("张三，李四"), "张三，李四", "全角逗号不是分隔符");
        assert_eq!(csv_field("a,b"), "\"a,b\"", "半角逗号必须加引号");
        assert_eq!(
            csv_field("他说\"好\""),
            "\"他说\"\"好\"\"\"",
            "内部引号翻倍"
        );
        assert_eq!(
            csv_field("第一行\n第二行"),
            "\"第一行\n第二行\"",
            "换行加引号"
        );
        assert_eq!(csv_field("回车\r结尾"), "\"回车\r结尾\"", "回车同样加引号");
    }

    /// CSV 公式注入防护：首字符是 `=` `+` `@`、制表符或回车的字段，以及 `-` 开头但
    /// 不是数字的字段，必须前置单引号，否则 Excel/WPS 打开表格时会按公式求值。
    ///
    /// Why：字幕原文/译文完全可能以 `=` 或 `@` 开头，裸导出等于「一打开表格就执行
    /// 内容里的公式」，属于导出即中招的问题。
    #[test]
    fn csv_field_guards_formula_injection() {
        assert_eq!(csv_field("=cmd"), "'=cmd");
        assert_eq!(csv_field("+1"), "'+1");
        assert_eq!(csv_field("@x"), "'@x");
        assert_eq!(csv_field("-2+3"), "'-2+3");
        for raw in ["=cmd", "+1", "@x", "-2+3", "\tTAB", "\rCR"] {
            let out = csv_field(raw);
            assert!(
                out.starts_with('\'') || out.starts_with("\"'"),
                "危险首字符必须前置单引号：{raw:?} -> {out:?}"
            );
        }
        // 纯数值的负号不是公式（Excel 按数字解析），别把置信度列变成文本。
        assert_eq!(csv_field("-0.9"), "-0.9");
        // 单引号在内、双引号在外：既危险又含逗号的字段两套规则都要生效。
        assert_eq!(csv_field("=a,b"), "\"'=a,b\"");
        // 普通字段不受影响。
        assert_eq!(csv_field("普通文本"), "普通文本");
    }

    /// CSV 带 BOM、Markdown 不带。
    ///
    /// Why：BOM 是 Excel 判定 UTF-8 的唯一线索，缺了中文会按本地代码页解释成乱码；
    /// Markdown 前面多一个 BOM 会在表格的第一个 `|` 之前渲染出一个多余字符。
    #[test]
    fn csv_has_bom_and_markdown_does_not() {
        let mut s = seg(1, 1.0, 2.0, "文本", Some("text"));
        s.confidence = Some(-0.9);
        let segs = vec![s];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);

        let csv = render_review_sheet(&segs, &report, QcFormat::Csv);
        assert!(csv.starts_with('\u{FEFF}'), "CSV 首字符必须是 BOM");
        assert_eq!(csv.matches('\u{FEFF}').count(), 1, "BOM 只该有一个");

        let md = render_review_sheet(&segs, &report, QcFormat::Markdown);
        assert!(!md.starts_with('\u{FEFF}'), "Markdown 前面不许有 BOM");
        assert!(md.starts_with("# 字幕复核表"), "Markdown 直接以标题开头");
    }

    /// Markdown 单元格：`|` 转义成 `\|`，换行压成 `<br>`。
    ///
    /// Why：裸 `|` 被当成列分隔符会拆散行；裸换行把表格从中间截断，后半张表渲染成
    /// 普通段落。两种都只在字幕里恰好含这些字符时发作，最容易被漏掉。
    #[test]
    fn markdown_escapes_pipe_and_breaks_newline() {
        let mut s = seg(1, 1.0, 2.0, "第一行|第二行\n第三行", Some("a|b\r\nc"));
        s.confidence = Some(-0.9);
        let segs = vec![s];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);

        let md = render_review_sheet(&segs, &report, QcFormat::Markdown);
        assert!(
            md.contains("第一行\\|第二行<br>第三行"),
            "原文单元格：\n{md}"
        );
        assert!(
            md.contains("a\\|b<br>c"),
            "译文单元格（CRLF 也要压平）：\n{md}"
        );
        assert!(
            !md.contains("第一行|第二行"),
            "转义后不该再有裸竖线：\n{md}"
        );
    }

    /// 逐句置信度完全缺席时才追加说明，且它落在表尾。
    ///
    /// Why（最要紧的一条）：SenseVoice 转写与旧记录没有 `confidence`，空白的
    /// 「置信度」列会被客户读成「都检查过、没问题」——那是对客户说的假话。反过来，
    /// 有置信度时多出这段说明会让人以为数据缺失。
    #[test]
    fn confidence_note_only_when_unavailable() {
        // 甲：整篇没有置信度 → 两种格式都必须有说明，且它在表尾。
        let no_conf = vec![seg(1, 0.0, 1.0, "第一句", Some("one"))];
        let report = quality_report(&no_conf, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert!(report.confidence_unavailable(), "全缺置信度 → 判据不可用");
        for format in [QcFormat::Csv, QcFormat::Markdown] {
            let sheet = render_review_sheet(&no_conf, &report, format);
            assert!(
                sheet.contains("没有可用的逐句置信度"),
                "{format:?} 缺说明：\n{sheet}"
            );
            let last = sheet.trim_end().lines().last().unwrap_or_default();
            assert!(last.contains("未评估"), "说明必须收在表尾：\n{sheet}");
        }

        // 乙：有置信度 → 这段说明一个字都不该出现。
        let mut scored = seg(1, 0.0, 1.0, "第一句", Some("one"));
        scored.confidence = Some(-0.1);
        let segs = vec![scored];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert!(!report.confidence_unavailable());
        for format in [QcFormat::Csv, QcFormat::Markdown] {
            let sheet = render_review_sheet(&segs, &report, format);
            assert!(
                !sheet.contains("没有可用的逐句置信度"),
                "{format:?} 多了说明：\n{sheet}"
            );
        }
    }

    /// 空输入：给出「未发现」的明文结论，且不含任何表体。
    ///
    /// Why：没有表体却留一行表头，Excel 用户会以为下面漏了行；明说「未发现需要复核
    /// 的句子」才是可交付的结论。
    #[test]
    fn empty_input_yields_clean_sheet_without_table() {
        let report = quality_report(&[], &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert!(!report.confidence_unavailable(), "空输入算不上能力缺口");

        let csv = render_review_sheet(&[], &report, QcFormat::Csv);
        assert!(
            csv.contains("未发现需要复核的句子。"),
            "要明说没有待办：\n{csv}"
        );
        assert!(!csv.contains("序号"), "没有表体就不该有序号表头：\n{csv}");

        let md = render_review_sheet(&[], &report, QcFormat::Markdown);
        assert!(md.contains("未发现需要复核的句子。"));
        assert!(!md.contains("| 序号"), "Markdown 也不该有表头：\n{md}");
    }

    /// 概览行的计数与报告逐项一致（含 0 句的类别）。
    ///
    /// Why：概览是给客户看的「本次质检结论」。数字对不上报告，整份表的可信度就没了；
    /// 0 句的类别也必须出现，否则用户分不清「没查」和「查了没发现」。
    #[test]
    fn summary_counts_match_report() {
        let mut low = seg(1, 0.0, 1.0, "正常句子", Some("ok"));
        low.confidence = Some(-0.9);
        let mut raw = seg(2, 1.0, 2.0, "有原文没译文", None);
        raw.confidence = Some(-0.01);
        let mut short = seg(3, 2.0, 3.0, "嗯", Some("hmm"));
        short.confidence = Some(-0.02);
        let segs = vec![low, raw, short];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);
        assert_eq!(report.all_issues().len(), 3, "低置信 / 未翻译 / 超短各一句");

        let sheet = render_review_sheet(&segs, &report, QcFormat::Csv);
        for expected in [
            "# 总句数：3",
            "# 待复核：3 句",
            "# 低置信：1 句",
            "# 术语违规：0 句",
            "# 未翻译：1 句",
            "# 空/超短句：1 句",
        ] {
            assert!(sheet.contains(expected), "缺 {expected}：\n{sheet}");
        }
    }

    /// 文件名主体追加「-质检」后缀。
    ///
    /// Why：复核表与字幕导出到同一目录，同名会覆盖刚交付的字幕（不可逆）。后缀让
    /// 两者并存，也让人一眼看出哪份是产出。
    #[test]
    fn review_file_stem_appends_suffix() {
        assert_eq!(review_file_stem("课程01"), "课程01-质检");
        assert_eq!(
            review_file_stem(" 课程01 "),
            "课程01-质检",
            "首尾空白先去掉"
        );
        assert_eq!(
            review_file_stem("   "),
            "质检",
            "空名字退回固定名，不产生 `-质检`"
        );
    }

    /// 起始时间列用 `HH:MM:SS.mmm`，跨分、跨时的进位都正确。
    ///
    /// Why：这里复用 `utils::time::seconds_to_timestamp`，本模块不另写一份时间格式化
    /// ——两份实现迟早分叉，评审查到两种时间码会怀疑数据本身不对。本测试钉住
    /// 「表里的时间列就是那个复用结果」，含 3599.9999 → `01:00:00.000` 的进位。
    #[test]
    fn timestamps_roll_over_minute_and_hour() {
        let mut a = seg(1, 65.123, 66.0, "一分零五秒", Some("a"));
        a.confidence = Some(-0.9);
        let mut b = seg(2, 3599.9999, 3601.0, "快到一小时", Some("b"));
        b.confidence = Some(-0.9);
        let segs = vec![a, b];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);

        let csv = render_review_sheet(&segs, &report, QcFormat::Csv);
        assert!(csv.contains("1,00:01:05.123,"), "分内时间：\n{csv}");
        assert!(csv.contains("2,01:00:00.000,"), "跨分进位到整点：\n{csv}");
    }

    /// 两行输入的金标准：表头逐字一致，两行数据都在。
    ///
    /// Why：表头是 Excel 排序/筛选的锚点，也是 Markdown 渲染成表格的前提。文案悄悄
    /// 改动（多一个空格、换列序）会让依赖它的下游脚本与工单模板静默失配。
    #[test]
    fn two_line_sheet_matches_golden_shape() {
        let mut first = seg(1, 1.0, 3.0, "你好，世界", Some("Hello, world"));
        first.confidence = Some(-0.9);
        let mut second = seg(2, 3.5, 4.25, "这是测试", None);
        second.confidence = Some(0.1);
        let segs = vec![first, second];
        let report = quality_report(&segs, &[], DEFAULT_LOW_CONFIDENCE_THRESHOLD, true);

        let csv = render_review_sheet(&segs, &report, QcFormat::Csv);
        assert_eq!(
            csv.lines().next(),
            Some("\u{FEFF}# 字幕复核表"),
            "首行 = BOM + 概览标题：\n{csv}"
        );
        assert!(
            csv.lines()
                .any(|l| l == "序号,开始,时长,问题,置信度,原文,译文"),
            "CSV 表头逐字一致：\n{csv}"
        );
        let row1 = "1,00:00:01.000,2.00,低置信,-0.9,你好，世界,\"Hello, world\"";
        let row2 = "2,00:00:03.500,0.75,未翻译,0.1,这是测试,—";
        assert!(
            csv.contains(row1),
            "第一行（译文含半角逗号 → 加引号）：\n{csv}"
        );
        assert!(csv.contains(row2), "第二行（没有译文 → 占位符）：\n{csv}");

        let md = render_review_sheet(&segs, &report, QcFormat::Markdown);
        assert!(
            md.lines()
                .any(|l| l == "| 序号 | 开始 | 时长 | 问题 | 置信度 | 原文 | 译文 |"),
            "Markdown 表头逐字一致：\n{md}"
        );
        let md1 = "| 1 | 00:00:01.000 | 2.00 | 低置信 | -0.9 | 你好，世界 | Hello, world |";
        let md2 = "| 2 | 00:00:03.500 | 0.75 | 未翻译 | 0.1 | 这是测试 | — |";
        assert!(md.contains(md1), "Markdown 第一行：\n{md}");
        assert!(md.contains(md2), "Markdown 第二行：\n{md}");
    }

    /// 报告里的序号在片段里找不到时跳过该行，绝不 panic。
    ///
    /// Why：报告与片段可能来自两次快照（导出时用户刚删了句）。为一行孤儿数据让整份
    /// 导出崩掉，是把小瑕疵放大成事故。
    #[test]
    fn dangling_index_is_skipped_without_panic() {
        let report = QualityReport {
            low_confidence: vec![7],
            ..QualityReport::default()
        };
        let sheet = render_review_sheet(&[], &report, QcFormat::Csv);
        assert!(
            sheet.contains("未发现需要复核的句子。"),
            "孤儿序号不入表：\n{sheet}"
        );
    }
}

#[cfg(test)]
mod punctuation_tests {
    // 刻意不用 `use super::*`：只依赖本模块的公开接口，实现细节改名不会让测试
    // 「跟着一起改」，也就不会掩盖真正的行为变化。
    use super::{
        ends_with_terminal_punctuation, missing_terminal_punctuation, plan_punctuation_fix,
    };
    use crate::subtitle::segment::Segment;

    fn seg(index: usize, text: &str) -> Segment {
        Segment::new(index, index as f64, index as f64 + 1.0, text)
    }

    /// 没有句末标点的句子进清单，有标点的不进。
    #[test]
    fn punctuation_flags_lines_without_terminal_mark() {
        let segs = [seg(1, "这句话没写完"), seg(2, "这句话写完了。")];
        assert_eq!(missing_terminal_punctuation(&segs), vec![1]);
        assert!(!ends_with_terminal_punctuation("这句话没写完"));
        assert!(ends_with_terminal_punctuation("这句话写完了。"));
    }

    /// 句尾的引号 / 括号 / 破折号 / 波浪号都不算「缺失」：跳过包裹符号看真字符。
    #[test]
    fn punctuation_accepts_closing_quote_dash_and_tilde() {
        for ok in [
            "他说\"好。\"",
            "（真的很好。）",
            "我本想——",
            "等等～",
            "真的吗？",
            "Wait!",
        ] {
            assert!(
                ends_with_terminal_punctuation(ok),
                "「{ok}」的句尾是收尾标点或包裹符号"
            );
        }
        let segs = [seg(1, "他说\"好。\""), seg(2, "他说话了")];
        assert_eq!(missing_terminal_punctuation(&segs), vec![2]);
    }

    /// 空文本、纯空白、全标点、纯符号行都算「没法判定」，不报缺失。
    #[test]
    fn punctuation_skips_empty_and_symbol_only_lines() {
        let segs = [seg(1, ""), seg(2, "   "), seg(3, "。。。"), seg(4, "♪♪")];
        let plan = plan_punctuation_fix(&segs);
        assert!(
            plan.indices.is_empty(),
            "这些都不是句子：{:?}",
            plan.indices
        );
        assert_eq!(plan.skipped, 4, "四条都记在「没检查」里，而不是「通过」");
    }

    /// 中英混排与省略号：拉丁句末标点同样被接受，省略号也算说完。
    #[test]
    fn punctuation_handles_mixed_language_and_ellipsis() {
        let segs = [
            seg(1, "OK"),                 // 拉丁、没标点 → 报
            seg(2, "OK."),                // 拉丁句末 → 不报
            seg(3, "但……"),               // 省略号 → 不报
            seg(4, "3.14"),               // 末尾是数字 → 报（没有句末标点）
            seg(5, "mixed 混排没有标点"), // 混排、没标点 → 报
        ];
        assert_eq!(missing_terminal_punctuation(&segs), vec![1, 4, 5]);
    }

    /// 计划同时给出下标与「没法判定」的计数，且下标不重复。
    #[test]
    fn plan_punctuation_fix_reports_indices_and_skipped() {
        let segs = [
            seg(1, "你好"),
            seg(2, "。。"),
            seg(3, "再见。"),
            Segment::new(7, 7.0, 8.0, ""),
        ];
        let plan = plan_punctuation_fix(&segs);
        assert_eq!(plan.indices, vec![1]);
        assert_eq!(plan.skipped, 2);
    }
}

#[cfg(test)]
mod overlap_duplicate_tests {
    // 刻意不用 `use super::*`：只依赖公开接口，实现细节改名不会连累测试。
    use super::{duplicate_neighbors, overlapping_segments};
    use crate::subtitle::segment::Segment;

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    /// 空输入 / 单句：没有相邻对，两个判据都必须是空列表。
    #[test]
    fn empty_and_single_input_report_nothing() {
        assert!(overlapping_segments(&[]).is_empty());
        assert!(duplicate_neighbors(&[]).is_empty());

        let one = [seg(1, 0.0, 1.0, "只有一句")];
        assert!(overlapping_segments(&one).is_empty());
        assert!(duplicate_neighbors(&one).is_empty());
    }

    /// 正常片段（不交叠、不重复）一个都不报。
    #[test]
    fn clean_neighbors_report_nothing() {
        let segs = [
            seg(1, 0.0, 1.0, "第一句"),
            seg(2, 1.2, 2.0, "第二句"),
            seg(3, 2.0, 3.0, "第三句"),
        ];
        assert!(overlapping_segments(&segs).is_empty());
        assert!(duplicate_neighbors(&segs).is_empty());
    }

    /// 恰好相接（`end == next.start`）是最常见的正常形态，绝不能报交叠。
    #[test]
    fn touching_boundaries_are_not_overlaps() {
        let segs = [
            seg(1, 0.0, 1.0, "上一句"),
            seg(2, 1.0, 2.0, "下一句"),
            seg(3, 2.0, 3.0, "再一句"),
        ];
        assert!(overlapping_segments(&segs).is_empty());
    }

    /// 浮点噪声级（小于容差）的交叠不报；真交叠报，且返回的是**前一句**下标。
    #[test]
    fn tiny_overlap_ignored_real_overlap_flags_previous() {
        let noise = [seg(10, 0.0, 1.0, "甲"), seg(11, 1.0 - 1e-9, 2.0, "乙")];
        assert!(
            overlapping_segments(&noise).is_empty(),
            "浮点噪声不该被报成交叠"
        );

        // 略超容差就要报：容差只挡噪声，不是把真交叠一并吞掉。
        let edge = [seg(1, 0.0, 1.0015, "甲"), seg(2, 1.0, 2.0, "乙")];
        assert_eq!(overlapping_segments(&edge), vec![1]);

        let real = [
            seg(3, 0.0, 1.5, "前一句"),
            seg(4, 1.0, 2.0, "后一句"),
            seg(5, 2.5, 3.0, "不相干"),
        ];
        assert_eq!(
            overlapping_segments(&real),
            vec![3],
            "要点出的正是前一句（要收紧的是它的 end）"
        );
    }

    /// 仅空白 / ASCII 大小写差异算重复，返回**后一句**下标；内容不同则不报。
    #[test]
    fn duplicate_neighbors_ignores_whitespace_and_ascii_case() {
        let segs = [
            seg(1, 0.0, 1.0, "Hello   World"),
            seg(2, 1.0, 2.0, "  hello world  "),
            seg(3, 2.0, 3.0, "不一样的一句"),
        ];
        assert_eq!(duplicate_neighbors(&segs), vec![2], "返回后一句下标");

        let distinct = [
            seg(1, 0.0, 1.0, "hello world"),
            seg(2, 1.0, 2.0, "hello  earth"),
        ];
        assert!(duplicate_neighbors(&distinct).is_empty(), "内容不同不报");
    }

    /// 空句 / 纯空白句不参与相邻重复（它们归核心四类的「空/超短句」）。
    #[test]
    fn blank_lines_never_count_as_duplicates() {
        let blank = [seg(1, 0.0, 1.0, ""), seg(2, 1.0, 2.0, "   ")];
        assert!(duplicate_neighbors(&blank).is_empty(), "空白句不参与");

        let mixed = [seg(1, 0.0, 1.0, "有意义的一句"), seg(2, 1.0, 2.0, "   ")];
        assert!(
            duplicate_neighbors(&mixed).is_empty(),
            "有内容句后跟空白句不算重复"
        );
    }

    /// 返回的下标升序且取的是 `Segment::index`（而不是列表位置）：index 非连续、
    /// 非从 0 开始时也要一一对上。
    #[test]
    fn indices_are_ascending_and_use_segment_index() {
        let overlaps = [
            seg(10, 0.0, 1.5, "甲"),
            seg(20, 1.0, 2.0, "乙"),
            seg(30, 2.0, 3.5, "丙"),
            seg(40, 3.0, 4.0, "丁"),
        ];
        assert_eq!(overlapping_segments(&overlaps), vec![10, 30]);

        let dups = [
            seg(5, 0.0, 1.0, "repeat"),
            seg(9, 1.0, 2.0, "REPEAT"),
            seg(9_000, 2.0, 3.0, "RePeAt"),
        ];
        assert_eq!(duplicate_neighbors(&dups), vec![9, 9_000]);
    }
}

#[cfg(test)]
mod repair_tests {
    // 与其它测试模块一致：只依赖公开接口，实现改名不连累断言。
    use super::{apply_repairs, plan_repairs, RepairItem, RepairKind};
    use crate::subtitle::segment::{Segment, MIN_EDIT_DUR};

    fn seg(index: usize, start: f64, end: f64, text: &str) -> Segment {
        Segment::new(index, start, end, text)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn items_of(plan: &super::RepairPlan, kind: RepairKind) -> Vec<&RepairItem> {
        plan.items.iter().filter(|i| i.kind == kind).collect()
    }

    /// 标点：间隔 > 0.8s 或片尾补「。」，间隔不足补「，」；已有标点的句子跳过。
    #[test]
    fn punctuation_plan_chooses_period_or_comma() {
        let segs = [
            seg(1, 0.0, 1.0, "第一句"),   // 与下一句隔 1.2s → 。
            seg(2, 2.2, 3.0, "第二句"),   // 与下一句隔 0.2s → ，
            seg(3, 3.2, 4.0, "第三句，"), // 已有逗号 → 跳过
            seg(4, 4.0, 5.0, "OK"),       // 片尾 → 。
        ];
        let plan = plan_repairs(&segs);
        let punctuation = items_of(&plan, RepairKind::Punctuation);
        let texts: Vec<(usize, &str, &str)> = punctuation
            .iter()
            .map(|i| (i.index, i.before.as_str(), i.after.as_str()))
            .collect();
        assert_eq!(
            texts,
            vec![(1, "第一句", "第一句。"), (2, "第二句", "第二句，"), (4, "OK", "OK。")],
            "只有缺标点的句子入选，且句号/逗号按间隔分档"
        );
        assert!(
            punctuation.iter().all(|i| i.text.is_some()),
            "文本类修复必须给出新文本"
        );
    }

    /// 重叠：优先把前一句 end 压到后一句 start - 0.02；前一句会被压没时改为推后一句起点。
    #[test]
    fn overlap_plan_prefers_compressing_but_never_crushes_previous() {
        let normal = [
            seg(1, 0.0, 1.5, "前一句。"),
            seg(2, 1.0, 2.0, "后一句。"),
        ];
        let plan = plan_repairs(&normal);
        let overlap = items_of(&plan, RepairKind::Overlap);
        assert_eq!(overlap.len(), 1);
        assert_eq!(overlap[0].index, 1, "默认改前一句的终点");
        assert_eq!(overlap[0].start, None, "没给的字段不许写");
        assert!(close(overlap[0].end.unwrap(), 0.98));
        assert!(overlap[0].before.contains('–') && overlap[0].after.contains('–'));

        let mut applied = normal.to_vec();
        let changed = apply_repairs(&mut applied, &plan);
        assert!(changed >= 1);
        assert!(close(applied[0].end, 0.98), "前一句终点被收短");
        assert!(applied[0].end <= applied[1].start, "应用后不允许再重叠");
        assert!(
            applied[0].end - applied[0].start >= MIN_EDIT_DUR - 1e-9,
            "收短后仍须够 MIN_EDIT_DUR"
        );

        // 前一句只有 0.2s（< MIN_EDIT_DUR）：压不了，只能推后一句的起点。
        let tight = [
            seg(1, 0.0, 0.20, "短句。"),
            seg(2, 0.10, 2.0, "后一句。"),
        ];
        let tight_plan = plan_repairs(&tight);
        let tight_items = items_of(&tight_plan, RepairKind::Overlap);
        assert_eq!(tight_items.len(), 1);
        assert_eq!(tight_items[0].index, 2, "改为改后一句的起点");
        assert_eq!(tight_items[0].end, None);
        assert!(close(tight_items[0].start.unwrap(), 0.22));
    }

    /// CPS：超限时拉长 end，但绝不越过下一句起点；正常语速不产项。
    #[test]
    fn cps_plan_extends_end_without_crossing_next_start() {
        // 20 个可见字符、0.5s → 40 cps，远超拉丁 17。
        let fast = [
            seg(1, 0.0, 0.5, "abcdefghij klmnopqrs."),
            seg(2, 5.0, 6.0, "next line."),
        ];
        let plan = plan_repairs(&fast);
        let cps = items_of(&plan, RepairKind::Cps);
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].index, 1);
        assert_eq!(cps[0].text, None);
        let wanted = 20.0 / 17.0;
        assert!(close(cps[0].end.unwrap(), wanted), "按舒适阈值 17 字符/秒拉长");

        // 需要 5.88s 才能读完，但下一句 2.0s 就开始：只能拉到 2.0，绝不越界。
        let long_text = "a".repeat(100);
        let capped = [
            seg(1, 0.0, 0.1, &long_text),
            seg(2, 2.0, 3.0, "next line."),
        ];
        let capped_plan = plan_repairs(&capped);
        let capped_cps = items_of(&capped_plan, RepairKind::Cps);
        assert_eq!(capped_cps.len(), 1);
        assert!(close(capped_cps[0].end.unwrap(), 2.0));
        assert!(capped_cps[0].end.unwrap() <= capped[1].start + 1e-9);

        // 10 字符 / 2s = 5 cps：不超限，不该产项。
        let calm = [
            seg(1, 0.0, 2.0, "abcdefghij."),
            seg(2, 2.5, 3.0, "ok."),
        ];
        assert!(
            items_of(&plan_repairs(&calm), RepairKind::Cps).is_empty(),
            "正常语速不报"
        );
    }

    /// 相邻重复句：去空白/标点、小写后相同 → 计划删空后一句（绝不删数组元素）。
    #[test]
    fn duplicate_plan_clears_text_without_removing_segment() {
        let segs = [
            seg(1, 0.0, 1.0, "Hello, world!"),
            seg(2, 1.0, 2.0, "  hello world  "),
        ];
        let plan = plan_repairs(&segs);
        let dups = items_of(&plan, RepairKind::Duplicate);
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].index, 2);
        assert_eq!(dups[0].before, "  hello world  ");
        assert_eq!(dups[0].after, "");
        assert_eq!(dups[0].text.as_deref(), Some(""));
        assert!(
            items_of(&plan, RepairKind::Punctuation).is_empty(),
            "要删空的句子不该再补标点，否则两条建议写同一个字段"
        );

        let mut applied = segs.to_vec();
        applied[1].polished = "已润色".to_string();
        // 用指向第 2 条的 plan 断言 polished 也被清掉。
        let changed = apply_repairs(&mut applied, &plan);
        assert!(changed >= 1);
        assert_eq!(applied[1].text, "", "text 被删空");
        assert_eq!(applied[1].polished, "", "polished 一并清空");
        assert_eq!(applied.len(), 2, "计划从不从数组里删元素");

        let distinct = [
            seg(1, 0.0, 1.0, "你好。"),
            seg(2, 1.0, 2.0, "再见。"),
        ];
        assert!(
            items_of(&plan_repairs(&distinct), RepairKind::Duplicate).is_empty(),
            "内容不同不删"
        );
    }

    /// 计划纯函数（不改输入）+ 应用幂等（同一 plan 第二次返回 0）+ 计数自洽。
    #[test]
    fn plan_is_pure_and_apply_is_idempotent() {
        let segs = [
            seg(1, 0.0, 2.0, "你好世界"),
            seg(2, 1.0, 3.0, "再见"),
        ];
        let snapshot = segs.to_vec();
        let plan = plan_repairs(&segs);
        assert_eq!(segs.as_slice(), snapshot, "plan_repairs 不得修改输入");
        assert!(!plan.is_empty());

        let counts = plan.counts();
        assert_eq!(
            counts.map(|(kind, _)| kind),
            RepairKind::ALL,
            "计数顺序固定"
        );
        assert_eq!(
            counts.iter().map(|(_, n)| *n).sum::<usize>(),
            plan.items.len(),
            "四类计数之和 = 条目总数"
        );

        let mut applied = segs.to_vec();
        let first = apply_repairs(&mut applied, &plan);
        assert!(first > 0, "第一次应用应有改动");
        let after_first = applied.clone();
        let second = apply_repairs(&mut applied, &plan);
        assert_eq!(second, 0, "同一计划第二次应用必须无改动");
        assert_eq!(applied, after_first, "第二次应用不得引入额外变化");
    }

    /// 干净输入：计划为空、四类计数都是 0。
    #[test]
    fn clean_input_yields_empty_plan() {
        let segs = [
            seg(1, 0.0, 2.0, "第一句。"),
            seg(2, 2.5, 4.0, "第二句！"),
        ];
        let plan = plan_repairs(&segs);
        assert!(plan.is_empty(), "没有可修项：{:?}", plan.items);
        assert!(plan.counts().iter().all(|(_, n)| *n == 0));
        assert_eq!(apply_repairs(&mut segs.to_vec(), &plan), 0);
    }
}
