pub mod background_thread;
pub mod backup;
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
pub mod queue_store;
pub mod temp_cleanup;
pub mod temp_guard;
pub mod time;
pub mod update_check;
pub mod zip_extract;

pub use config::{
    AppConfig, NamedStylePreset, SubtitleStyleConfig, SUBTITLE_COLOR_PALETTE,
    SUBTITLE_MARGIN_LR_MAX, SUBTITLE_OUTLINE_OPTIONS, SUBTITLE_PRESETS,
};
pub use frame_cache::FrameCache;
pub use glossary::{GlossaryEntry, MergeReport, ParseReport};
pub use logger::{init_logger, open_latest_log_file, open_logs_folder, record_error_log, ERROR_LOG_NAME};
pub use local_models::{scan_installed_models, scan_llm_models, LocalModel};
pub use model_download::{DownloadItem, ItemGroup, ITEMS};
pub use monitor::SystemMonitor;
pub use path_safety::{normalize_path_slashes, safe_temp_dir, to_cli_safe_path};
pub use temp_guard::TempPathGuard;
