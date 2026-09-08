//! Voice2Word — 音视频智能字幕生成器 (Rust + GPUI)
//! 遵循 Codex / Zed 极简现代设计风格

mod app;
mod core;
mod engines;
mod storage;
mod subtitle;
mod ui;
mod utils;

use anyhow::Result;
use gpui::*;
use std::sync::Arc;
use tracing::info;

use app::AppState;
use core::TaskPipeline;
use engines::{FFmpegEngine, LLMEngine, WhisperEngine};
use storage::Database;
use ui::MainWindow;
use utils::AppConfig;

fn main() -> Result<()> {
    // 1. 初始化双写日志系统 (同时输出至控制台与 logs/ 文本文件，并拦截记录 panic 堆栈)
    let _log_file = utils::init_logger()?;

    info!("==========================================");
    info!("Voice2Word 启动中 (Rust + GPUI Edition)...");
    info!("==========================================");

    // 2. 加载配置文件
    let config = AppConfig::load_from_file("config.toml")?;
    info!("配置加载成功: {:?}", config);

    // 3. 打开 SQLite 数据库
    let db = Database::open("voice2word.db")?;
    info!("SQLite 数据库已连接");

    // 4. 初始化底层计算引擎
    let ffmpeg = Arc::new(FFmpegEngine::new(AppConfig::resolve_path(
        &config.paths.ffmpeg,
    )));

    let vad_model_path = if config.pipeline.enable_vad {
        config
            .paths
            .vad_model
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .or_else(|| {
                let default_vad = AppConfig::resolve_path("models/whisper/ggml-silero-v6.2.0.bin");
                if default_vad.exists() {
                    Some(default_vad)
                } else {
                    None
                }
            })
    } else {
        None
    };

    let whisper = Arc::new(WhisperEngine::with_vad(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        vad_model_path,
        config.pipeline.whisper_threads,
        config.pipeline.whisper_processors,
    ));

    let llm = Arc::new(LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    // 5. 编排流水线管线
    let pipeline = Arc::new(TaskPipeline::new(ffmpeg, whisper, llm));

    // 6. 初始化全局应用状态
    let state = AppState::new(config, db, pipeline);

    // 7. 启动 GPUI 应用程序
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1120.0), px(740.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Voice2Word — 音视频智能字幕生成器".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| MainWindow::new(state.clone(), cx)),
        )
        .expect("打开主窗口失败");
    });

    Ok(())
}
