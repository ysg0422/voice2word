//! 崩溃自检与报告：把「用户根本不知道发生过」的那次崩溃捡回来。
//!
//! # 为什么需要这个模块
//!
//! 进程崩溃时，`logger.rs` 里注册的 panic 钩子会把一段
//! `[CRITICAL PANIC DETECTED]` 块（时间 / 详情 / 堆栈）追加进 `logs/` 下的
//! 日志文件，然后进程就没了。**但没有任何东西告诉用户**：他们重新打开程序，
//! 一切正常，于是永远不提这次崩溃——bug 因此既不被人报告，也永远修不掉。
//! 更糟的是，真要提交问题所需的那段日志，被埋在一个用户根本不知道存在的文件里。
//!
//! 本模块负责把这个闭环合上：启动时检测「上一次运行是不是崩过」，并产出
//! 两样东西——
//!
//! - (a) 一句给界面横幅显示的人类可读摘要（`short_summary`）；
//! - (b) 一份自包含、可整段复制/保存的报告（`build_report`）。
//!
//! # 设计约束
//!
//! - **只有 `detect_previous_crash` 碰文件系统**，其余全是纯函数，因此可以
//!   脱离 UI 做单元测试。
//! - **绝不 panic**：目录不存在、文件读不动、文件不是 UTF-8、文件是空的、
//!   有标记但没有后续字段……全部退化成 `None` 或一条字段为空的 `CrashRecord`。
//!   这个模块的使命就是「处理一次已经崩掉的运行」，它自己再崩一次毫无意义。
//! - **绝不打印密钥**：报告只搬运解析出来的崩溃字段，**不整段复制日志文件**，
//!   也**不读取任何配置值**（`config.toml` 里的 `translate.api_key` 之类）。
//!   报告是要贴进聊天窗口 / 邮件 / issue 的，带密钥等于泄露。见
//!   `report_contains_version_and_backtrace_but_no_secrets` 测试。
//!
//! # 容错解析
//!
//! 钩子里的那段文本是 `logger.rs` 用**手写 `format!`** 拼出来的，没有结构化
//! 序列化，字段顺序和文案都可能被人改。因此解析只认三个前缀
//! （`时间:` / `详情:` / `堆栈:`），任一缺失都不丢整条记录：`message` 抓不到就
//! 留空，界面至少还能说一句「上次异常退出」。`PANIC_MARKER` 是唯一的硬约束，
//! 见其文档。

use std::path::{Path, PathBuf};

/// 日志里标记崩溃开始的固定串。**必须与 `logger.rs` 写出的完全一致**，
/// 因此这里定义为常量并在文档里点名来源；两处一旦漂移，崩溃检测会静默失效。
///
/// 来源：`logger.rs` 中 panic 钩子里的
/// `"==================== [CRITICAL PANIC DETECTED] ===================="`
/// 这一行。单测 `panic_marker_matches_logger_literal` 直接读该文件断言两处一致。
pub const PANIC_MARKER: &str = "[CRITICAL PANIC DETECTED]";

/// `message` 保留的最大字符数。
///
/// 用**字符**而不是字节计数：本项目的 panic 消息大量是中文，按字节切会切进
/// UTF-8 码点中间，在「正在报告崩溃」的路径上再制造一次 panic——最坏的时机。
/// 300 足够装下一句人话，又能保证横幅/报告不被超长消息撑爆。
pub const MAX_MESSAGE_CHARS: usize = 300;

/// 上一次运行留下的崩溃信息（如果检测到）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRecord {
    /// 崩溃时间原文（从日志里抓到的「时间: ...」行；抓不到则空串）
    pub at: String,
    /// panic 详情首行（`详情: ...`），已截断
    pub message: String,
    /// 堆栈原文（可能为空 —— release 构建默认不捕获回溯）
    pub backtrace: String,
    /// 该记录来自哪个日志文件
    pub source: std::path::PathBuf,
}

/// 在日志目录里找**最后一次**崩溃。
///
/// - 扫 `log_dir` 下的 `.txt`（含 `latest.txt`），逐个找 `PANIC_MARKER`；
/// - 返回最后一次出现的那一条（按文件内出现顺序取最后一个：一次运行可能崩多次，
///   用户关心的是最后一次）；
/// - 一个都没有 → `None`。
///
/// 为什么扫目录而不是只读 `latest.txt`：`latest.txt` 会被后续正常运行的启动日志
/// 覆盖/追加，而按日期命名的历史日志还留着上次崩溃的原文。只读 latest 会漏掉
/// 「崩了、然后正常跑了一次」这种最常见的情形。
///
/// 多个文件都含崩溃时，按**路径排序后取最后一个**（`latest.txt` 排在
/// `voice2word_*.txt` 之前，日期文件名天然按时间序）——这样结果是确定的，
/// 同一次目录内容总能得到同一条记录，用户和测试都能复现。
///
/// 目录不存在 / 不可读、单个文件读不动、文件非 UTF-8（按字节读入后做有损解码）
/// 都只会让对应文件被跳过，绝不 panic。
pub fn detect_previous_crash(log_dir: &Path) -> Option<CrashRecord> {
    // 目录不存在或没有权限：视作「没崩过」，而不是报错——首次运行时这很正常。
    let entries = std::fs::read_dir(log_dir).ok()?;

    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        })
        .collect();
    // 排序让「最后一个文件」可复现：目录枚举顺序在 Windows 上并不稳定。
    candidates.sort();

    let mut found: Option<CrashRecord> = None;
    for path in candidates {
        if let Some(record) = last_crash_in_file(&path) {
            // 覆盖式赋值 → 目录序里最靠后的那个文件里的最后一次崩溃胜出。
            found = Some(record);
        }
    }
    found
}

/// 在单个文件里找**最后一次** `PANIC_MARKER`，并解析它后面的字段。
///
/// 读字节再做 `from_utf8_lossy`：日志可能被外部工具写成 GBK、或某次写入
/// 在半路被截断成非法字节序列，直接 `read_to_string` 会返回 `Err` 而丢掉整条
/// 记录；有损解码只把坏字节换成 U+FFFD，标记和字段依然能认出来。
fn last_crash_in_file(path: &Path) -> Option<CrashRecord> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);

    // 取最后一次出现：一次运行可能崩多次，用户关心的是最后一次。
    let last_pos = text.match_indices(PANIC_MARKER).map(|(i, _)| i).last()?;
    Some(parse_block(&text[last_pos..], path))
}

/// 从 `PANIC_MARKER` 处开始解析一个崩溃块；字段缺失时留空，不返回 `None`。
///
/// 容忍点（对应「日志格式由手写 `format!` 生成、可能变化」）：
/// - 横幅行之后找不到 `时间:` / `详情:` → 对应字段为空串；
/// - 找不到 `堆栈:` → `backtrace` 为空串；
/// - `堆栈:` 之后一直读到收尾的 `====` 行为止，中间空行原样保留；
/// - 行首空白会被 `trim_start` 吃掉，防止钩子里 `\` 续行改动导致前缀对不上。
fn parse_block(block: &str, source: &Path) -> CrashRecord {
    let mut at = String::new();
    let mut message = String::new();
    let mut backtrace = String::new();

    let mut lines = block.lines();
    // 第一行是含 PANIC_MARKER 的横幅，跳过。
    let _banner = lines.next();

    let mut in_backtrace = false;
    for line in lines {
        if in_backtrace {
            // 收尾横幅（`====...`）标记块结束，后面的内容属于下一次输出。
            if line.trim_start().starts_with("====") {
                break;
            }
            if !backtrace.is_empty() {
                backtrace.push('\n');
            }
            backtrace.push_str(line);
            continue;
        }

        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("时间:") {
            at = rest.trim().to_string();
        } else if let Some(rest) = trimmed.strip_prefix("详情:") {
            message = truncate_chars(rest.trim(), MAX_MESSAGE_CHARS);
        } else if let Some(rest) = trimmed.strip_prefix("堆栈:") {
            in_backtrace = true;
            // 极少数情况堆栈首行与 `堆栈:` 同行，一并收下。
            let rest = rest.trim();
            if !rest.is_empty() {
                backtrace.push_str(rest);
            }
        }
    }

    CrashRecord {
        at,
        message,
        backtrace: backtrace.trim_end().to_string(),
        source: source.to_path_buf(),
    }
}

/// 按**字符**截断到至多 `max_chars` 个字符（不是字节）。
///
/// 为什么强调字符边界：panic 消息可能是中文，按字节切片会切进 UTF-8 码点中间
/// 并 panic —— 而这发生在「正在报告崩溃」的路径上，是最不能崩的地方。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

/// 生成给用户复制/保存的完整报告（纯函数）。
/// 含：版本号、崩溃时间、详情、堆栈（若有）、以及一句「去哪儿提交」。
///
/// **堆栈不截断**：它是开发者唯一真正需要的东西，截掉就等于让问题无法定位。
/// 与之相对，`message` 在解析阶段就已按字符截断（见 `MAX_MESSAGE_CHARS`）。
///
/// **不含配置值与密钥**：本函数只接收 `CrashRecord` 和版本号，签名上就拿不到
/// `AppConfig`，也没有任何读配置文件的分支；它只搬运解析出来的崩溃字段，
/// 不会把日志文件整段抄进报告（日志里可能有启动时打印的密钥行）。
/// 报告里还明写一句「不包含任何配置值或密钥」，免得用户不敢外发。
pub fn build_report(record: &CrashRecord, app_version: &str) -> String {
    let mut out = String::new();
    out.push_str("Voice2Word 崩溃报告\n");
    out.push_str(&"=".repeat(64));
    out.push('\n');
    out.push_str(&format!("版本: {app_version}\n"));
    out.push_str(&format!(
        "崩溃时间: {}\n",
        or_placeholder(&record.at, "未知（日志里没有时间行）")
    ));
    out.push_str(&format!(
        "详情: {}\n",
        or_placeholder(&record.message, "（日志里没有详情行）")
    ));
    out.push_str(&format!("日志文件: {}\n", record.source.display()));
    out.push_str("说明：本报告不包含任何配置值或密钥，可放心复制外发。\n");
    out.push_str(&"-".repeat(64));
    out.push('\n');
    out.push_str("堆栈:\n");
    if record.backtrace.trim().is_empty() {
        out.push_str("（无堆栈：release 构建默认不捕获回溯）\n");
    } else {
        out.push_str(&record.backtrace);
        out.push('\n');
    }
    out.push_str(&"-".repeat(64));
    out.push('\n');
    out.push_str("请把以上内容提交到项目的 issue 区，或直接发给开发者。\n");
    out
}

/// 给界面显示的一句话（纯函数，不要换行）。
/// 例如 `上次运行异常退出（2026-10-09 10:31:02）：attempt to subtract with overflow`
///
/// 无换行是硬要求：这句话会被塞进单行横幅，一旦带 `\n` 就会把界面撑破；
/// 因此这里把消息里的 CR/LF 一律折成空格（panic 详情理论上可能含换行）。
pub fn short_summary(record: &CrashRecord) -> String {
    let at = record.at.trim();
    let head = if at.is_empty() {
        "上次运行异常退出".to_string()
    } else {
        format!("上次运行异常退出（{at}）")
    };
    let message = or_placeholder(record.message.trim(), "（无详情）");
    format!("{head}：{}", one_line(message))
}

/// 空串（含全空白）时给出占位文案，否则原样返回。用于「字段可能缺失」的报告渲染。
fn or_placeholder<'a>(value: &'a str, placeholder: &'a str) -> &'a str {
    if value.trim().is_empty() {
        placeholder
    } else {
        value
    }
}

/// 把任意文本压成一行：CR/LF 折成空格，保证横幅不会被换行撑破。
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个专属临时目录（先清后建）。
    ///
    /// 为什么带 `pid` 和 `tag`：cargo 默认并行跑测试，共用一个目录会互相删对方
    /// 的文件；带上进程 id 再各自带 tag 就完全隔离。所有路径都指向系统临时目录，
    /// **绝不**写进仓库树。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_crash_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 拼一个与 `logger.rs` 钩子同构的崩溃块（含时间 / 详情 / 堆栈）。
    fn block(at: &str, message: &str, backtrace: &str) -> String {
        format!("{PANIC_MARKER}\n时间: {at}\n详情: {message}\n堆栈:\n{backtrace}\n")
    }

    /// 单文件里有一个完整崩溃块 → 能检测到，三个字段都解析出来，来源路径正确。
    #[test]
    fn detects_full_block_in_single_file() {
        let dir = temp_dir("single");
        let log = dir.join("latest.txt");
        std::fs::write(&log, block("2026-10-09 10:31:02", "boom", "frame0\nframe1")).unwrap();

        let rec = detect_previous_crash(&dir).expect("应检测到崩溃");
        assert_eq!(rec.at, "2026-10-09 10:31:02");
        assert_eq!(rec.message, "boom");
        assert_eq!(rec.backtrace, "frame0\nframe1");
        assert_eq!(rec.source, log);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个文件里崩了两次 → 取**最后一次**（用户关心最近那次）。
    #[test]
    fn detects_last_block_when_file_has_two() {
        let dir = temp_dir("two_blocks");
        let log = dir.join("latest.txt");
        let mut text = String::new();
        text.push_str(&block("2026-10-09 10:00:00", "first crash", "f1"));
        text.push_str("\n正常日志一行\n");
        text.push_str(&block("2026-10-09 10:31:02", "second crash", "f2"));
        std::fs::write(&log, text).unwrap();

        let rec = detect_previous_crash(&dir).expect("应检测到崩溃");
        assert_eq!(rec.message, "second crash");
        assert_eq!(rec.at, "2026-10-09 10:31:02");
        assert_eq!(rec.backtrace, "f2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 崩溃只留在按日期命名的历史日志里、`latest.txt` 是后来正常运行的启动日志
    /// → 仍要检测到（这正是「只读 latest 会漏掉」的场景）。
    #[test]
    fn detects_crash_only_in_dated_file() {
        let dir = temp_dir("dated");
        let dated = dir.join("voice2word_20261009_103102.txt");
        std::fs::write(&dated, block("2026-10-09 10:31:02", "crashed", "bt")).unwrap();
        std::fs::write(
            dir.join("latest.txt"),
            "Voice2Word 文本日志系统初始化成功\n一切正常\n",
        )
        .unwrap();

        let rec = detect_previous_crash(&dir).expect("应检测到历史日志里的崩溃");
        assert_eq!(rec.source, dated);
        assert_eq!(rec.message, "crashed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 日志里没有标记 → `None`（正常跑过一次，不该报「上次崩了」）。
    #[test]
    fn no_marker_returns_none() {
        let dir = temp_dir("no_marker");
        std::fs::write(dir.join("latest.txt"), "启动成功\n干活中\n").unwrap();
        assert!(detect_previous_crash(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录根本不存在 → `None`，不 panic（首次运行、日志目录还没建）。
    #[test]
    fn missing_dir_returns_none() {
        let dir = temp_dir("missing").join("does-not-exist");
        assert!(detect_previous_crash(&dir).is_none());
    }

    /// 空文件 → `None`，不 panic。
    #[test]
    fn empty_file_returns_none() {
        let dir = temp_dir("empty");
        std::fs::write(dir.join("latest.txt"), b"").unwrap();
        assert!(detect_previous_crash(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只有标记、没有时间/详情/堆栈行 → 仍然返回 `Some`，字段为空串。
    ///
    /// 容错的要点：不能因为「日志格式变了/写到一半断电」就把整条记录丢掉，
    /// 界面至少还能说一句「上次异常退出」。
    #[test]
    fn marker_without_fields_still_yields_record() {
        let dir = temp_dir("marker_only");
        std::fs::write(dir.join("latest.txt"), format!("x\n{PANIC_MARKER}\n")).unwrap();

        let rec = detect_previous_crash(&dir).expect("有标记就该给记录");
        assert!(rec.at.is_empty());
        assert!(rec.message.is_empty());
        assert!(rec.backtrace.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 UTF-8 字节序列 → 有损解码，不 panic，标记后的字段照样解析。
    #[test]
    fn non_utf8_bytes_do_not_panic() {
        let dir = temp_dir("non_utf8");
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE, 0x80, 0x00];
        bytes.extend_from_slice(block("2026-10-09 10:31:02", "坏字节", "bt").as_bytes());
        std::fs::write(dir.join("latest.txt"), &bytes).unwrap();

        let rec = detect_previous_crash(&dir).expect("有损解码后仍应检测到");
        assert_eq!(rec.message, "坏字节");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 超长中文详情 → 按**字符**截断到恰好 `MAX_MESSAGE_CHARS`，且不 panic。
    #[test]
    fn truncates_cjk_message_on_char_boundary() {
        let dir = temp_dir("cjk_truncate");
        let long: String = "崩".repeat(MAX_MESSAGE_CHARS + 500);
        std::fs::write(
            dir.join("latest.txt"),
            block("2026-10-09 10:31:02", &long, "bt"),
        )
        .unwrap();

        let rec = detect_previous_crash(&dir).expect("应检测到崩溃");
        assert_eq!(rec.message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(rec.message.chars().all(|c| c == '崩'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 报告含版本号与**完整**堆栈（不截断），且不夹带日志里的密钥行。
    ///
    /// 密钥用「日志里另一行」的方式植入：报告只搬运解析出的崩溃字段，
    /// 绝不整段复制日志文件，因此这行密钥不会被带出去。
    #[test]
    fn report_contains_version_and_backtrace_but_no_secrets() {
        let dir = temp_dir("secrets");
        const PLANTED: &str = "sk-planted-crash-key-do-not-leak";
        let long_bt: String = (0..400)
            .map(|i| format!("frame{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!(
            "启动配置: api_key={PLANTED}\n{}",
            block("2026-10-09 10:31:02", "boom", &long_bt)
        );
        std::fs::write(dir.join("latest.txt"), content).unwrap();

        let rec = detect_previous_crash(&dir).expect("应检测到崩溃");
        let report = build_report(&rec, "9.9.9");

        assert!(report.contains("9.9.9"), "报告缺版本号");
        assert!(report.contains("frame399"), "报告截断了堆栈");
        assert!(report.contains("2026-10-09 10:31:02"), "报告缺崩溃时间");
        assert!(!report.contains(PLANTED), "报告泄露了密钥");
        assert!(!report.contains("sk-"), "报告出现疑似密钥片段");
        assert!(report.contains("不包含任何配置值或密钥"), "应明写不含密钥");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 摘要必须单行：即便详情里混进 CR/LF，输出也不许出现换行。
    #[test]
    fn short_summary_has_no_newline() {
        let record = CrashRecord {
            at: "2026-10-09 10:31:02".to_string(),
            message: "第一行\r\n第二行".to_string(),
            backtrace: String::new(),
            source: PathBuf::from("latest.txt"),
        };
        let summary = short_summary(&record);
        assert!(!summary.contains('\n'));
        assert!(!summary.contains('\r'));
        assert!(summary.starts_with("上次运行异常退出（2026-10-09 10:31:02）："));
        assert!(summary.contains("第一行"));
        assert!(summary.contains("第二行"));
    }

    /// 字段全空时摘要也要成句（不能出现 `（）` 空括号或缺详情就断句）。
    #[test]
    fn short_summary_handles_empty_fields() {
        let record = CrashRecord {
            at: String::new(),
            message: String::new(),
            backtrace: String::new(),
            source: PathBuf::from("latest.txt"),
        };
        let summary = short_summary(&record);
        assert_eq!(summary, "上次运行异常退出：（无详情）");
    }

    /// `PANIC_MARKER` 必须与 `logger.rs` 里手写的那段字面量逐字符一致——两处一旦
    /// 漂移，崩溃检测会**静默失效**（用户永远收不到「上次崩了」的提示）。
    /// 这里直接读源文件断言，而不是抄一份字符串。
    #[test]
    fn panic_marker_matches_logger_literal() {
        assert_eq!(PANIC_MARKER, "[CRITICAL PANIC DETECTED]");
        let logger = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/utils/logger.rs"),
        )
        .expect("读取 logger.rs 失败");
        assert!(
            logger.contains(PANIC_MARKER),
            "logger.rs 里找不到 PANIC_MARKER 字面量"
        );
        for field in ["时间:", "详情:", "堆栈:"] {
            assert!(logger.contains(field), "logger.rs 缺少字段前缀 {field}");
        }
    }
}
