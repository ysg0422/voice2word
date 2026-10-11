pub mod backup;
pub mod child_registry;
pub mod config;
pub mod crash_report;
pub mod diagnostics;
pub mod duplicates;
pub mod fingerprint;
pub mod frame_cache;
pub mod library_query;
pub mod logger;
pub mod media_scan;
pub mod model_download;
pub mod monitor;
pub mod pe_imports;
pub mod queue_store;
pub mod temp_cleanup;
pub mod temp_guard;
pub mod time;
pub mod update_check;
pub mod zip_extract;

pub use config::{
    AppConfig, SubtitleStyleConfig, SUBTITLE_COLOR_PALETTE, SUBTITLE_OUTLINE_OPTIONS,
    SUBTITLE_PRESETS,
};
pub use frame_cache::FrameCache;
pub use logger::init_logger;
pub use model_download::{DownloadItem, ItemGroup, ITEMS};
pub use monitor::SystemMonitor;
pub use temp_guard::TempPathGuard;
