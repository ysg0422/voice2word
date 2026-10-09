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

use crate::subtitle::segment::{QualityCategory, QualityReport, Segment};
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

/// CSV 字段转义：含 `,` / `"` / 换行的字段整体加双引号，内部的 `"` 翻倍。
///
/// 字幕文本里逗号与引号极常见（对话、术语、引用）。裸拼的 CSV 会被 Excel 静默按
/// 逗号切列——打开看像成功，实则整列数据错位到相邻列，是「看着没问题、内容全错」
/// 的那类 bug。
fn csv_field(value: &str) -> String {
    let needs_quoting =
        value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r');
    if !needs_quoting {
        return value.to_string();
    }
    let escaped = value.replace('"', "\"\"");
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
