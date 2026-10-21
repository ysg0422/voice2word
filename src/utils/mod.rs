pub mod background_thread;
pub mod backup;
pub mod chapters;
pub mod child_registry;
pub mod config;
pub mod crash_report;
pub mod diagnostics;
pub mod duplicates;
pub mod fingerprint;
pub mod frame_cache;
pub mod glossary;
pub mod library_query;
pub mod local_models;
pub mod logger;
pub mod media_scan;
pub mod model_download;
pub mod monitor;
pub mod path_safety;
pub mod pe_imports;
pub mod project_audit;
pub mod queue_store;
pub mod temp_cleanup;
pub mod temp_guard;
pub mod time;
pub mod typo_memory;
pub mod update_check;
pub mod zip_extract;

pub use config::{
    AppConfig, NamedStylePreset, SubtitleStyleConfig, SUBTITLE_COLOR_PALETTE,
    SUBTITLE_MARGIN_LR_MAX, SUBTITLE_OUTLINE_OPTIONS, SUBTITLE_PRESETS,
};
pub use frame_cache::FrameCache;
pub use glossary::{GlossaryEntry, MergeReport, ParseReport};
pub use local_models::{scan_installed_models, scan_llm_models, LocalModel};
pub use logger::{
    init_logger, open_latest_log_file, open_logs_folder, record_error_log, ERROR_LOG_NAME,
};
pub use model_download::{DownloadItem, ItemGroup, ITEMS};
pub use monitor::SystemMonitor;
pub use path_safety::{normalize_path_slashes, safe_temp_dir, to_cli_safe_path};

// ── 单实例探测的结果（由 `main` 在启动时置位，界面据此提示） ──
//
// 为什么要有这道防线：两个实例同时开着，会各写各的 `voice2word.db`。SQLite 本身
// 有 WAL + busy_timeout，**不会把库写坏**，但用户会在两个窗口里改同一份字幕，
// 谁后写谁赢——「我刚改的那句怎么没了」就是这么来的，而且没有任何提示。
//
// 这里刻意**不阻断启动**（用户可能就想开第二个窗口看别的素材），只把事实告诉用户。
static OTHER_INSTANCE_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 解码一段**用户可编辑**的文本文件字节（配置、术语表等）。
///
/// 顺序：严格 UTF-8 → GBK（`had_errors` 为假才算命中）→ lossy 兜底。
///
/// 为什么必须有这一步：中文 Windows 上不少编辑器（老版记事本、Notepad++ 的默认设置、
/// 部分 IDE 插件）保存 .toml/.txt 时用的是 **GBK**。而 `fs::read_to_string` 要求严格
/// UTF-8，一遇到 GBK 就返回 `InvalidData`——程序把它当成「配置文件损坏」，
/// **直接拒绝启动**，只抛一条看不懂的 io 错误。用户只是改了行配置而已。
///
/// 返回 `(文本, 是否用了非 UTF-8 兜底)`：第二项供调用方记一条提示，
/// 让用户知道「建议另存为 UTF-8」，而不是让编码问题永久潜伏。
pub fn decode_text_bytes(raw: &[u8]) -> (String, bool) {
    match std::str::from_utf8(raw) {
        Ok(s) => (s.to_string(), false),
        Err(_) => {
            let (decoded, _, had_errors) = encoding_rs::GBK.decode(raw);
            if !had_errors {
                (decoded.into_owned(), true)
            } else {
                // 既不是 UTF-8 也不是干净 GBK：按 lossy 保留可读部分，绝不因为
                // 「有几个字节不认识」就把整份配置丢弃。
                (String::from_utf8_lossy(raw).into_owned(), true)
            }
        }
    }
}

/// 记录「启动时检测到另一个实例在运行」。
pub fn set_other_instance_running(value: bool) {
    OTHER_INSTANCE_RUNNING.store(value, std::sync::atomic::Ordering::SeqCst);
}

/// 启动时是否检测到另一个实例在运行。
pub fn other_instance_running() -> bool {
    OTHER_INSTANCE_RUNNING.load(std::sync::atomic::Ordering::SeqCst)
}

pub use temp_guard::TempPathGuard;
