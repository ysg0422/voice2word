//! 编辑审计日志：把「每一次字幕编辑」留成可持久化、可导出的记录。
//!
//! 现状是**编辑不可见**：用户改了一句台词、拖过一段起止时间、拆过一句、删过一句，
//! 事后没有任何痕迹。接手别人项目的用户（或两周后的同一个用户）看不出改了什么、
//! 什么时候改的；一整批编辑事后发现改错时，也看不到「改之前是什么」。
//!
//! 撤销栈救不了这件事，三道墙都过不去：深度只有 50 层、只活在内存里、而且新一轮
//! 转写替换整份文档时会被清空——重启程序或换一个项目文件，「改过什么」就彻底没了。
//!
//! 本模块只做两件事：攒记录（[`EditLog`]）与渲染（[`render_audit`]）。两条刻意的
//! 边界：
//!
//! - **不碰数据库**。落盘时机、表结构、保留策略全由调用方决定；这里只是数据 +
//!   格式化，换一种持久化方式不需要动本模块。
//! - **纯函数不读时钟**。时间一律由调用方传入 chrono::DateTime<chrono::Local>，
//!   于是所有逻辑都能用固定时间戳单测，不必和「现在几点」赛跑。
//!
//! 记录**有界**（[`MAX_RECORDS`]）：一次整轨重排会给几百句各留一条，长时间会话下
//! 无界增长会吃光内存；审计是给人看的，保留最近的即可。

use crate::subtitle::qc::QcFormat;

/// 一次编辑的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// 改了原文（识别文本 / 润色文本）
    Text,
    /// 改了译文
    Translation,
    /// 调了起止时间
    Timing,
    /// 拆句
    Split,
    /// 合并
    Merge,
    /// 删除
    Delete,
    /// 批量替换
    Replace,
    /// 整轨时间轴变换
    Retime,
}

/// 全部种类的**声明顺序**清单。
///
/// 为什么要有它：界面按这个顺序渲染统计条，顺序由声明顺序单点决定（各写一份
/// match，迟早会「枚举加了新成员、统计漏了它」）；[`EditLog::counts`] 也靠它保证
/// 零条的种类照样出现——少一行统计，用户分不清「没查」和「查了没有」。
const ALL_KINDS: [EditKind; 8] = [
    EditKind::Text,
    EditKind::Translation,
    EditKind::Timing,
    EditKind::Split,
    EditKind::Merge,
    EditKind::Delete,
    EditKind::Replace,
    EditKind::Retime,
];

impl EditKind {
    /// 中文短名（界面与导出共用，避免两处各写一份）
    pub fn label(self) -> &'static str {
        match self {
            EditKind::Text => "改原文",
            EditKind::Translation => "改译文",
            EditKind::Timing => "调时间",
            EditKind::Split => "拆句",
            EditKind::Merge => "合并",
            EditKind::Delete => "删除",
            EditKind::Replace => "替换",
            EditKind::Retime => "重排",
        }
    }

    /// 是否属于「破坏性」编辑（删除 / 合并）——界面据此用警示色。
    ///
    /// 为什么合并也算：它把两句合成一句，下一句就此消失，和删除一样是不可逆的
    /// 结构变更；只标删除会让「合并」披着无副作用的皮过去。
    pub fn is_destructive(self) -> bool {
        matches!(self, EditKind::Delete | EditKind::Merge)
    }
}

/// 一条审计记录。
#[derive(Debug, Clone, PartialEq)]
pub struct EditRecord {
    /// 句序号（`Segment::index`）。删除类操作记录被删那句的序号。
    pub index: usize,
    pub kind: EditKind,
    /// 变更前（无则空串）
    pub before: String,
    /// 变更后（无则空串）
    pub after: String,
    /// 发生时间（由调用方传入，本模块不读时钟）
    pub at: chrono::DateTime<chrono::Local>,
}

impl EditRecord {
    /// 造一条文本类记录（`before`/`after` 都是原文）
    pub fn text(
        index: usize,
        before: impl Into<String>,
        after: impl Into<String>,
        at: chrono::DateTime<chrono::Local>,
    ) -> Self {
        Self {
            index,
            kind: EditKind::Text,
            before: before.into(),
            after: after.into(),
            at,
        }
    }

    /// 时间变更记录：`before`/`after` 存 `"1.000-2.500"` 形式的区间文本。
    ///
    /// 用同一个字符串格式而不是新增字段，是为了让导出/对比只有一套表示
    /// （界面上改完时间要跟日志逐字比对，两种表示必然对不上）。
    pub fn timing(
        index: usize,
        before: (f64, f64),
        after: (f64, f64),
        at: chrono::DateTime<chrono::Local>,
    ) -> Self {
        Self {
            index,
            kind: EditKind::Timing,
            before: format_interval(before),
            after: format_interval(after),
            at,
        }
    }
}

/// 把 `(start, end)` 秒对渲染成区间文本：`1.000-2.500`。
///
/// 保留三位小数与字幕文件的时间精度一致（毫秒），否则「日志说 1.0-2.5、文件里
/// 是 1.000-2.500」会让审校怀疑自己看错了记录。
fn format_interval(span: (f64, f64)) -> String {
    format!("{:.3}-{:.3}", span.0, span.1)
}

/// 编辑日志（有界）。
#[derive(Debug, Clone, Default)]
pub struct EditLog {
    records: Vec<EditRecord>,
}

/// 展示时默认的合并窗口（秒）。
///
/// 为什么是 60 秒：文本编辑是**逐键**记一条，同一个句子里连打十几个字就是十几条；
/// 一分钟内的连续同句同类型改动，在人的记忆里就是「我刚改了这一句」，合成一条才对。
/// 放成公开常量而不是让每个调用点各写一个字面量——两处不一致会让「界面看到的条数」
/// 与「导出的条数」对不上。
pub const DEFAULT_COALESCE_SECS: i64 = 60;

/// 日志上限。为什么有界：一次「整轨重排」会给几百句各留一条记录，长时间会话下
/// 无界增长会吃光内存；审计是给人看的，保留最近的就够了。
pub const MAX_RECORDS: usize = 500;

impl EditLog {
    /// 空日志。
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// 追加一条；超过上限时**从最旧的开始丢**（保留最近的是唯一合理的取舍——
    /// 用户关心的是「我刚改了什么」）。
    pub fn push(&mut self, record: EditRecord) {
        self.records.push(record);
        if self.records.len() > MAX_RECORDS {
            // 一次只可能超出有限条，按差值裁掉最旧的，避免整表重建。
            let excess = self.records.len() - MAX_RECORDS;
            self.records.drain(..excess);
        }
    }

    /// 全部记录（按写入顺序，即时间顺序）。
    pub fn records(&self) -> &[EditRecord] {
        &self.records
    }

    /// 记录条数。
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 是否一条都没有。
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// 清空（切换项目 / 批量编辑作废时用）。
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 某一句的全部记录（升序）
    pub fn for_index(&self, index: usize) -> Vec<&EditRecord> {
        self.records.iter().filter(|r| r.index == index).collect()
    }

    /// 按种类过滤
    pub fn of_kind(&self, kind: EditKind) -> Vec<&EditRecord> {
        self.records.iter().filter(|r| r.kind == kind).collect()
    }

    /// 统计每种各有多少条（顺序固定为 [`EditKind`] 的声明顺序，便于界面稳定渲染）
    pub fn counts(&self) -> Vec<(EditKind, usize)> {
        ALL_KINDS
            .iter()
            .map(|kind| {
                let n = self.records.iter().filter(|r| r.kind == *kind).count();
                (*kind, n)
            })
            .collect()
    }

    /// 合并「连续、同句、同类型、且时间差 < `coalesce_secs`」的记录为一条
    /// （before 取最早、after 取最新）。为什么：逐键打字会为每敲一个字留一条记录，
    /// 日志会被单个句子刷屏。返回**新的** `EditLog`，不改自身。
    pub fn coalesced(&self, coalesce_secs: i64) -> EditLog {
        let mut out: Vec<EditRecord> = Vec::with_capacity(self.records.len());
        for rec in &self.records {
            if let Some(last) = out.last_mut() {
                // 与「上一条已归并的记录」比较，而不是与整段的起点比较：所谓连续，
                // 指的是相邻两条之间没有断档，串起来的连写才会收成一条。
                let adjacent = last.index == rec.index
                    && last.kind == rec.kind
                    && (rec.at - last.at).num_seconds().abs() < coalesce_secs;
                if adjacent {
                    // before 保留最早那份（已在该条里），after 换成最新那份；
                    // 时间戳也推到最新——这条记录代表这串连写的最终结果。
                    last.after = rec.after.clone();
                    last.at = rec.at;
                    continue;
                }
            }
            out.push(rec.clone());
        }
        EditLog { records: out }
    }
}

/// 审计表列头。两种格式共用一份常量：各写一份，迟早会「CSV 改了列名、
/// Markdown 忘改」，下游脚本与界面会按两种列名解析同一份数据。
const COLUMNS: [&str; 5] = ["时间", "序号", "类型", "变更前", "变更后"];

/// 没有任何记录时写在表尾的结论行。
///
/// 为什么必须有：空的 CSV / 空字符串看起来就是「导出失败」，用户会反复重导。
/// 明写一句「本次没有编辑记录」才能把「导出了、只是没改过」和「导出坏了」分开。
const EMPTY_NOTE: &str = "本次没有编辑记录。";

/// 渲染成 CSV（UTF-8 **带 BOM**，Excel 双击不乱码）或 Markdown 表格。
/// 列：时间 / 序号 / 类型 / 变更前 / 变更后。CSV 转义规则与 `subtitle::qc` 相同
/// （含 `,` `"` 换行时加引号、`"` 双写）；Markdown 里 `|` 转义为 `\|`、换行转 `<br>`。
pub fn render_audit(records: &[EditRecord], format: crate::subtitle::qc::QcFormat) -> String {
    let mut out = String::new();
    // BOM 必须在最前面：Excel 靠它判定 UTF-8，缺了中文会按本地代码页解释成乱码；
    // Markdown 反过来绝不能带 BOM——它会在第一个 `|` 之前渲染出一个多余字符。
    if format == QcFormat::Csv {
        out.push('\u{FEFF}');
    }

    match format {
        QcFormat::Csv => {
            out.push_str(&COLUMNS.join(","));
            out.push('\n');
            for rec in records {
                let cells = row_cells(rec);
                let escaped: Vec<String> = cells.iter().map(|c| csv_field(c)).collect();
                out.push_str(&escaped.join(","));
                out.push('\n');
            }
        }
        QcFormat::Markdown => {
            out.push_str(&format!("| {} |\n", COLUMNS.join(" | ")));
            let separator = vec!["---"; COLUMNS.len()];
            out.push_str(&format!("| {} |\n", separator.join(" | ")));
            for rec in records {
                let cells: Vec<String> = row_cells(rec).iter().map(|c| markdown_cell(c)).collect();
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
        }
    }

    if records.is_empty() {
        out.push_str(EMPTY_NOTE);
        out.push('\n');
    }
    out
}

/// 一条记录的五个单元格（顺序与 [`COLUMNS`] 一一对应）。
///
/// 时间格式固定 `%Y-%m-%d %H:%M:%S`：秒级精度足够定位「哪次会话改的」，又不带
/// 时区后缀/纳秒尾巴——那些只会把列撑宽，还让两次导出的同一条记录文本不一致。
fn row_cells(rec: &EditRecord) -> [String; 5] {
    [
        rec.at.format("%Y-%m-%d %H:%M:%S").to_string(),
        rec.index.to_string(),
        rec.kind.label().to_string(),
        rec.before.clone(),
        rec.after.clone(),
    ]
}

/// CSV 字段转义：含 `,` / `"` / 换行的字段整体加双引号，内部的 `"` 翻倍。
///
/// 与 `subtitle::qc` 的规则逐字一致（那里是私有实现，无法复用，故本地重写一份并在
/// 测试里钉住行为）。字幕文本里逗号与引号极常见，裸拼的 CSV 会被 Excel 静默切列：
/// 打开看像成功，实则整列数据错位。
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
/// 后半张表渲染成一堆普通段落。同样与 `subtitle::qc` 的私有实现保持一致。
fn markdown_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\r', '\n'], "<br>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Local, TimeDelta, TimeZone};

    /// 固定时间戳 2026-01-02 03:04:05。
    ///
    /// 为什么不能用 `Local::now()`：合并窗口与渲染断言都依赖具体时间值，读时钟会让
    /// 「时间差 < 窗口」这类断言随运行时刻漂移（跨秒、跨日翻转时偶发失败）。
    fn fixed_time() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    /// 造一条文本记录，时间固定在基准点上（正文只关心文本与序号）。
    fn text_at(index: usize, after: &str) -> EditRecord {
        EditRecord::text(index, "", after, fixed_time())
    }

    /// push / len / is_empty / clear 的基本契约与 clear 后的「干净」状态。
    ///
    /// Why：`clear` 若只清计数不清内容，界面「已清空」之后导出仍能带出旧记录——
    /// 这是审计模块最不能被接受的一类错误（说了删掉却没删掉）。
    #[test]
    fn push_len_is_empty_and_clear() {
        let mut log = EditLog::new();
        assert!(log.is_empty(), "新日志必须是空的");
        assert_eq!(log.len(), 0);
        assert!(log.records().is_empty());

        log.push(text_at(1, "第一句"));
        log.push(text_at(2, "第二句"));
        assert_eq!(log.len(), 2);
        assert!(!log.is_empty());
        assert_eq!(log.records().len(), 2);

        log.clear();
        assert!(log.is_empty(), "clear 之后必须一条不剩");
        assert_eq!(log.len(), 0);
        assert!(log.records().is_empty(), "clear 后 records() 必须是空切片");
    }

    /// 超过上限时从最旧的开始丢，最新的必须留下。
    ///
    /// Why：上限的意义是「保最近」。若丢的是最新的（或整体拒绝写入），用户刚做的
    /// 编辑会在日志里消失——审计反而丢掉了最该保留的那条。
    #[test]
    fn cap_drops_oldest_and_keeps_newest() {
        let mut log = EditLog::new();
        let ts = fixed_time();
        for i in 0..MAX_RECORDS + 10 {
            log.push(EditRecord::text(i, "", format!("第 {i} 句"), ts));
        }

        assert_eq!(log.len(), MAX_RECORDS, "长度必须被钳在上限");
        assert_eq!(
            log.records().first().map(|r| r.index),
            Some(10),
            "最旧的 10 条必须被丢掉"
        );
        assert_eq!(
            log.records().last().map(|r| r.index),
            Some(MAX_RECORDS + 9),
            "最新的一条必须还在"
        );
        assert!(
            !log.records().iter().any(|r| r.index < 10),
            "被丢掉的记录不许残留"
        );
    }

    /// `for_index` 只返回该句的记录，且保持时间升序。
    ///
    /// Why：界面点某一句要展示「这句话的改动史」，混进别的句子会让人以为自己
    /// 改错了行；顺序错了则「最近一次改动」就不是最后一条。
    #[test]
    fn for_index_keeps_only_that_line_in_order() {
        let base = fixed_time();
        let mut log = EditLog::new();
        log.push(EditRecord::text(3, "a", "b", base));
        log.push(EditRecord::text(7, "x", "y", base));
        log.push(EditRecord::text(3, "b", "c", base + TimeDelta::seconds(60)));

        let hits = log.for_index(3);
        assert_eq!(hits.len(), 2, "只该命中序号 3 的两条");
        assert!(hits.iter().all(|r| r.index == 3));
        assert_eq!(hits[0].before, "a", "升序：最早的一条在前");
        assert_eq!(hits[1].after, "c");
        assert!(log.for_index(99).is_empty(), "不存在的序号返回空而非 panic");
    }

    /// `of_kind` 只返回指定种类的记录。
    #[test]
    fn of_kind_keeps_only_that_kind() {
        let ts = fixed_time();
        let mut log = EditLog::new();
        log.push(text_at(1, "改了原文"));
        log.push(EditRecord::timing(1, (0.0, 1.0), (0.0, 2.0), ts));
        log.push(text_at(2, "又改了原文"));

        let texts = log.of_kind(EditKind::Text);
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().all(|r| r.kind == EditKind::Text));

        let timings = log.of_kind(EditKind::Timing);
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].before, "0.000-1.000");

        assert!(
            log.of_kind(EditKind::Delete).is_empty(),
            "没有的种类返回空而非 panic"
        );
    }

    /// `counts` 覆盖全部种类：顺序是声明顺序，零条的种类也必须出现。
    ///
    /// Why：界面按固定顺序画统计条，缺一行会让同一批数字每次导出都换位置；零条也要
    /// 出现，否则用户分不清「这个项目没删过句」和「统计功能没覆盖删除」。
    #[test]
    fn counts_cover_all_kinds_in_declaration_order() {
        let ts = fixed_time();
        let mut log = EditLog::new();
        log.push(text_at(0, "a"));
        log.push(text_at(1, "b"));
        log.push(EditRecord::timing(1, (0.0, 1.0), (1.0, 2.0), ts));

        let counts = log.counts();
        let order: Vec<EditKind> = counts.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            order,
            vec![
                EditKind::Text,
                EditKind::Translation,
                EditKind::Timing,
                EditKind::Split,
                EditKind::Merge,
                EditKind::Delete,
                EditKind::Replace,
                EditKind::Retime,
            ],
            "顺序必须是声明顺序"
        );
        assert_eq!(counts.len(), ALL_KINDS.len(), "每一种都要有一行");
        assert_eq!(counts[0].1, 2, "改原文两条");
        assert_eq!(counts[2].1, 1, "调时间一条");
        assert_eq!(counts[1].1, 0, "零条的种类照样出现");
        assert_eq!(
            counts.iter().map(|(_, n)| *n).sum::<usize>(),
            log.len(),
            "各计数之和 = 记录总数"
        );
    }

    /// 同句、同类、时间挨着的一串记录合并成一条：before 取最早、after 取最新。
    ///
    /// Why：逐键打字会为每敲一个字留一条记录，一个句子就能刷满整页日志；合并后
    /// 「这一句从 我 变成了 我准备好了」才是人想看的。
    #[test]
    fn coalesced_merges_burst_of_same_line() {
        let base = fixed_time();
        let mut log = EditLog::new();
        log.push(EditRecord::text(3, "我", "我准", base));
        log.push(EditRecord::text(
            3,
            "我准",
            "我准备",
            base + TimeDelta::seconds(1),
        ));
        log.push(EditRecord::text(
            3,
            "我准备",
            "我准备好了",
            base + TimeDelta::seconds(2),
        ));

        let merged = log.coalesced(5);
        assert_eq!(merged.len(), 1, "连写的三条应合成一条");
        assert_eq!(merged.records()[0].before, "我", "before 取最早");
        assert_eq!(merged.records()[0].after, "我准备好了", "after 取最新");
        assert_eq!(log.len(), 3, "coalesced 返回新日志，不得改自身");
    }

    /// 时间差达到（或超过）窗口时不合并，窗口内则合并。
    ///
    /// Why：窗口判断写反（<= 或 < 搞错）会让「改了标题、五分钟后改了同一行」被
    /// 合并成一条，用户就看不到中间那段时间里发生过什么。
    #[test]
    fn coalesced_respects_time_window_boundary() {
        let base = fixed_time();
        let mut log = EditLog::new();
        log.push(EditRecord::text(1, "a", "b", base));
        // 间隔 3 秒：窗口 3 秒时「< 3」不成立，必须分成两条。
        log.push(EditRecord::text(1, "b", "c", base + TimeDelta::seconds(3)));

        let split = log.coalesced(3);
        assert_eq!(split.len(), 2, "间隔 == 窗口不得合并");
        assert_eq!(split.records()[0].after, "b");
        assert_eq!(split.records()[1].before, "b");

        let joined = log.coalesced(4);
        assert_eq!(joined.len(), 1, "间隔小于窗口才合并");
        assert_eq!(joined.records()[0].after, "c");
    }

    /// 不同句序号的记录绝不合并，哪怕时间挨得再近。
    ///
    /// Why：合并只对「同一句」有意义。跨句合并会把两条改动糊成一条，导出的审计表
    /// 就指向了错误的行号，审校照着它去改会改错地方。
    #[test]
    fn coalesced_does_not_merge_across_indices() {
        let base = fixed_time();
        let mut log = EditLog::new();
        log.push(EditRecord::text(1, "a", "b", base));
        log.push(EditRecord::text(2, "x", "y", base + TimeDelta::seconds(1)));

        let merged = log.coalesced(60);
        assert_eq!(merged.len(), 2, "不同句必须各留一条");
        assert_eq!(merged.records()[0].index, 1);
        assert_eq!(merged.records()[1].index, 2);
    }

    /// 不同种类的记录绝不合并。
    ///
    /// Why：改原文与改译文是两件事，合并会让「before/after 都是原文」的约定失效，
    /// 界面里也再分不清这次改动动的是哪一栏。
    #[test]
    fn coalesced_does_not_merge_across_kinds() {
        let base = fixed_time();
        let mut log = EditLog::new();
        log.push(EditRecord::text(1, "a", "b", base));
        log.push(EditRecord::timing(
            1,
            (0.0, 1.0),
            (0.0, 2.0),
            base + TimeDelta::seconds(1),
        ));

        let merged = log.coalesced(60);
        assert_eq!(merged.len(), 2, "不同种类必须各留一条");
        assert_eq!(merged.records()[0].kind, EditKind::Text);
        assert_eq!(merged.records()[1].kind, EditKind::Timing);
    }

    /// 空日志合并后仍是空日志（不 panic、不产生占位记录）。
    #[test]
    fn coalesced_on_empty_stays_empty() {
        let log = EditLog::new();
        let merged = log.coalesced(10);
        assert!(merged.is_empty());
        assert_eq!(merged.len(), 0);
    }

    /// 每种的中文短名非空且互不相同。
    ///
    /// Why：短名是界面与导出共用的唯一标识，重名会让「统计里写着改译文两条」实际
    /// 混进原文改动；空名则在那两处渲染出一个空白列。
    #[test]
    fn labels_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for kind in ALL_KINDS {
            let label = kind.label();
            assert!(!label.is_empty(), "{kind:?} 缺中文短名");
            assert!(seen.insert(label), "短名重复：{label}");
        }
        assert_eq!(seen.len(), ALL_KINDS.len(), "短名数量应等于种类数");
    }

    /// 破坏性判定真值表：只有删除与合并为真。
    ///
    /// Why：界面靠它上警示色。多标会让正常编辑看起来危险（用户学会忽略警告），
    /// 少标则让不可逆操作悄无声息地过去。
    #[test]
    fn is_destructive_matches_truth_table() {
        for kind in ALL_KINDS {
            let expected = matches!(kind, EditKind::Delete | EditKind::Merge);
            assert_eq!(kind.is_destructive(), expected, "{kind:?} 的破坏性判定错了");
        }
    }

    /// `EditRecord::timing` 把区间渲染成 `1.000-2.500` 文本，并且种类是「调时间」。
    #[test]
    fn timing_record_formats_interval_text() {
        let rec = EditRecord::timing(2, (1.0, 2.5), (0.5, 2.0), fixed_time());
        assert_eq!(rec.kind, EditKind::Timing);
        assert_eq!(rec.before, "1.000-2.500");
        assert_eq!(rec.after, "0.500-2.000");
        assert_eq!(rec.index, 2);
    }

    /// CSV 带 BOM、Markdown 不带。
    ///
    /// Why：BOM 是 Excel 判定 UTF-8 的唯一线索，缺了中文会乱码；Markdown 前面多一个
    /// BOM 会在表格第一个 `|` 之前渲染出一个多余字符。
    #[test]
    fn csv_has_bom_and_markdown_does_not() {
        let rec = text_at(1, "文本");

        let csv = render_audit(std::slice::from_ref(&rec), QcFormat::Csv);
        assert!(csv.starts_with('\u{FEFF}'), "CSV 首字符必须是 BOM");
        assert_eq!(csv.matches('\u{FEFF}').count(), 1, "BOM 只该有一个");

        let md = render_audit(&[rec], QcFormat::Markdown);
        assert!(!md.starts_with('\u{FEFF}'), "Markdown 前面不许有 BOM");
        assert!(md.starts_with("| 时间"), "Markdown 直接以表头开头");
    }

    /// CSV 转义：含逗号 / 引号 / 换行的字段加引号并把内部引号翻倍。
    ///
    /// Why：这是复核表与审计表共用的唯一一道防线，写错一次就会让 Excel 静默切列——
    /// 打开看像成功，内容已经全错位。
    #[test]
    fn csv_quotes_comma_quote_and_newline() {
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

        // 端到端：整行渲染也必须遵守同一套规则。
        let rec = EditRecord::text(1, "a,b", "他说\"好\"\n第二行", fixed_time());
        let csv = render_audit(&[rec], QcFormat::Csv);
        assert!(csv.contains("\"a,b\""), "逗号字段要加引号：\n{csv}");
        assert!(
            csv.contains("\"他说\"\"好\"\"\n第二行\""),
            "引号与换行同时出现时也要正确处理：\n{csv}"
        );
    }

    /// Markdown 单元格：`|` 转义成 `\|`，换行压成 `<br>`。
    ///
    /// Why：裸 `|` 被当作列分隔符会拆散整行，裸换行把表格从中间截断；两种都只在
    /// 字幕里恰好含这些字符时发作，最容易被漏掉。
    #[test]
    fn markdown_escapes_pipe_and_newline() {
        let rec = EditRecord::text(1, "第一行|第二行\n第三行", "a|b\r\nc", fixed_time());
        let md = render_audit(&[rec], QcFormat::Markdown);
        assert!(
            md.contains("第一行\\|第二行<br>第三行"),
            "原文单元格：\n{md}"
        );
        assert!(md.contains("a\\|b<br>c"), "译文侧（CRLF 也要压平）：\n{md}");
        assert!(
            !md.contains("第一行|第二行"),
            "转义后不该再有裸竖线：\n{md}"
        );
    }

    /// 空日志也要导出成一张可读的表：带列头 + 一句「本次没有编辑记录」。
    ///
    /// Why：空字符串或没有列头的文件看起来就是「导出失败」，用户会反复重导；列头
    /// 让人一眼知道这是审计表，结论行把「没改过」和「导出坏了」分开。
    #[test]
    fn empty_log_renders_note_and_header() {
        let csv = render_audit(&[], QcFormat::Csv);
        assert!(csv.starts_with('\u{FEFF}'), "CSV 仍要带 BOM");
        assert!(csv.contains("时间,序号,类型,变更前,变更后"), "列头必须在");
        assert!(csv.contains(EMPTY_NOTE), "必须有明确的结论行：\n{csv}");
        assert_eq!(csv.lines().count(), 2, "空表只有列头与结论行两行");

        let md = render_audit(&[], QcFormat::Markdown);
        assert!(md.contains("| 时间 | 序号 | 类型 | 变更前 | 变更后 |"));
        assert!(md.contains(EMPTY_NOTE));
    }

    /// 中文的 before/after 原样进 CSV，且整行形状（时间 / 序号 / 类型 / 两栏文本）正确。
    ///
    /// Why：审计的对象全是中文台词，任何一次编码或转义失误都会把内容变成乱码或问号；
    /// 这条同时钉住时间格式（秒级、无时区尾巴）与类型列是否用了中文短名。
    #[test]
    fn cjk_text_survives_csv_unchanged() {
        let rec = EditRecord::text(7, "机器学习", "机器学习（ML）", fixed_time());
        let csv = render_audit(&[rec], QcFormat::Csv);
        assert!(
            csv.contains("2026-01-02 03:04:05,7,改原文,机器学习,机器学习（ML）"),
            "整行形状不对：\n{csv}"
        );
        assert!(csv.ends_with('\n'), "行尾要带换行，方便追加导出");
    }
}
