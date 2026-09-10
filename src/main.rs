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
use tracing::{info, warn};

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
    let hardware = engines::HardwareProfile::detect();
    info!(use_gpu_pipeline = hardware.use_gpu_pipeline(), adapter = %hardware.adapter_name, "媒体渲染后端已选择");

    // 3. 打开 SQLite 数据库
    let db = Database::open("voice2word.db")?;
    info!("SQLite 数据库已连接");

    // 4. 初始化底层计算引擎
    let ffmpeg = Arc::new(FFmpegEngine::new(AppConfig::resolve_path(
        &config.paths.ffmpeg,
    )));

    let default_vad = AppConfig::resolve_path("models/whisper/ggml-silero-v6.2.0.bin");
    let vad_model_path = if config.pipeline.enable_vad {
        config
            .paths
            .vad_model
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .or_else(|| {
                if default_vad.exists() {
                    Some(default_vad.clone())
                } else {
                    None
                }
            })
    } else if default_vad.exists() {
        info!("检测到 Silero VAD 模型，默认启用 VAD 静音切片加速识别");
        Some(default_vad)
    } else {
        None
    };

    let whisper = Arc::new(WhisperEngine::with_device(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        vad_model_path,
        config.pipeline.whisper_threads,
        config.pipeline.whisper_processors,
        hardware.use_gpu_pipeline(),
    ));

    let llm = Arc::new(LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let punc = {
        let runner = AppConfig::resolve_path("tools/punc_runner.py");
        let model = config.paths.punc_model.as_ref().map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/punc/model.int8.onnx"));
        if runner.exists() && model.exists() {
            info!("CT-Transformer 极速标点引擎已就绪: {:?}", model);
            Some(Arc::new(engines::PunctuationEngine::new(runner, model, 4)))
        } else {
            warn!("CT-Transformer 极速标点引擎未就绪 (runner={}, model={})", runner.exists(), model.exists());
            None
        }
    };

    // 5. 编排流水线管线
    let pipeline = Arc::new(TaskPipeline::new(ffmpeg, whisper, llm, punc));


    // 6. 初始化全局应用状态
    let state = AppState::with_hardware(config, db, pipeline, hardware);

    // 7. 启动 GPUI 应用程序
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);

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
