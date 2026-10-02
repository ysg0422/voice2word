//! Voice2Word — 音视频智能字幕生成器 (Rust + GPUI)
//! 遵循 Codex / Zed 极简现代设计风格

#![recursion_limit = "512"]

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

    // 1.5 清扫上次运行崩溃/取消时留在 %TEMP% 的中间产物（压实 WAV、切块目录、
    // whisper JSON、预览代理视频等）。转写管线的删除逻辑只在正常路径执行，
    // 报错/取消路径会漏删，因此这里加一道启动兜底，避免 TEMP 无限膨胀。
    let swept = utils::temp_cleanup::sweep_stale_temp_files();
    if swept > 0 {
        info!("启动清扫：已删除 {swept} 个过期的临时中间产物");
    }

    // 2. 加载配置文件
    let config = AppConfig::load_from_file("config.toml")?;
    info!("配置加载成功: {:?}", config);
    // 主题必须在任何界面构建之前设定：颜色 token 在渲染时才读取全局开关
    ui::theme::Theme::set_light(config.ui.is_light());
    info!("界面主题: {}", config.ui.label());
    let mut hardware = if !config.gpu.hwaccel_decode && !config.gpu.whisper_offload {
        engines::HardwareProfile::cpu_only()
    } else {
        engines::HardwareProfile::detect()
    };
    // GPU 占用策略（config.toml [gpu]）：给桌面/其他应用留显卡的两位总闸
    if !config.gpu.hwaccel_decode {
        hardware.hardware_decode = false;
        info!("GPU 策略: 预览硬解已关闭 (gpu.hwaccel_decode=false)，预览走 CPU 软解");
    }
    let whisper_gpu = hardware.use_gpu_pipeline() && config.gpu.whisper_offload;
    if !whisper_gpu {
        info!("GPU 策略: 转写推理走纯 CPU (gpu.whisper_offload=false 或无独显/核显不可用)");
    }
    if config.gpu.yield_to_desktop {
        info!("GPU 策略: 让路模式开启，转写/预览子进程降为低于正常优先级，桌面与前台程序优先");
    } else {
        info!("GPU 策略: 让路模式关闭，转写子进程按正常优先级抢占 CPU/GPU");
    }
    // GPU 占用上限：让路只降 CPU 调度优先级，压不住已经排进 GPU 队列的命令缓冲，
    // 想让桌面真正跟手必须按占空比给 GPU 留空窗
    let gpu_limit_percent = if whisper_gpu {
        config.gpu.effective_gpu_limit()
    } else {
        100
    };
    if gpu_limit_percent < 100 {
        info!(
            gpu_limit_percent,
            "GPU 策略: 占空比限速开启，whisper-cli 会周期性挂起给桌面留出 GPU 空窗，转写耗时约放大 {:.2} 倍",
            100.0 / gpu_limit_percent as f64
        );
    }
    info!(use_gpu_pipeline = hardware.use_gpu_pipeline(), adapter = %hardware.adapter_name, "媒体渲染后端已选择");

    // 3. 打开 SQLite 数据库
    let db = Database::open("voice2word.db")?;
    info!("SQLite 数据库已连接");

    // 4. 初始化底层计算引擎
    let ffmpeg = Arc::new(FFmpegEngine::new(AppConfig::resolve_path(
        &config.paths.ffmpeg,
    )));

    // Python 解释器：纯命令名走 PATH，含路径则按项目根展开
    let python_path = AppConfig::resolve_command(&config.paths.python);
    info!("Python 解释器: {:?}", python_path);

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
    } else {
        info!("配置已关闭 Silero VAD，Whisper 将扫描完整音频");
        None
    };

    let whisper = Arc::new(WhisperEngine::with_device(
        AppConfig::resolve_path(&config.paths.whisper_cli),
        AppConfig::resolve_path(&config.paths.whisper_model),
        vad_model_path,
        config.pipeline.whisper_threads,
        config.pipeline.whisper_processors,
        whisper_gpu,
        config.pipeline.whisper_no_fallback,
        config.pipeline.whisper_max_context,
        config.gpu.yield_to_desktop,
        gpu_limit_percent,
    ));

    let llm = Arc::new(LLMEngine::new(
        AppConfig::resolve_path(&config.paths.llama_cli),
        AppConfig::resolve_path(&config.paths.llm_model),
        config.pipeline.llm_ctx,
        config.pipeline.llm_threads,
    ));

    let sensevoice = {
        let runner = AppConfig::resolve_path("tools/sensevoice_runner.py");
        let model = config.paths.sensevoice_model.as_ref().map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/model.int8.onnx"));
        let tokens = config.paths.sensevoice_tokens.as_ref().map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/tokens.txt"));
        let vad = config.paths.sensevoice_vad.as_ref().map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/silero_vad.onnx"));
        if runner.exists() && model.exists() && tokens.exists() && vad.exists() {
            info!("SenseVoice 极速非自回归语音识别引擎已就绪: {:?}", model);
            Some(Arc::new(engines::SenseVoiceEngine::with_python(
                runner,
                model,
                tokens,
                vad,
                config.pipeline.whisper_threads,
                python_path.clone(),
            )))
        } else {
            warn!(
                "SenseVoice 引擎未就绪 (runner={}, model={}, tokens={}, vad={})",
                runner.exists(),
                model.exists(),
                tokens.exists(),
                vad.exists()
            );
            None
        }
    };

    let punc = {
        let runner = AppConfig::resolve_path("tools/punc_runner.py");
        let model = config.paths.punc_model.as_ref().map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/punc/model.int8.onnx"));
        if runner.exists() && model.exists() {
            info!("CT-Transformer 极速标点引擎已就绪: {:?}", model);
            Some(Arc::new(engines::PunctuationEngine::with_python(
                runner,
                model,
                4,
                python_path.clone(),
            )))
        } else {
            warn!("CT-Transformer 极速标点引擎未就绪 (runner={}, model={})", runner.exists(), model.exists());
            None
        }
    };

    // 5. 编排流水线管线
    let pipeline = Arc::new(TaskPipeline::new(ffmpeg, whisper, sensevoice, llm, punc));


    // 6. 初始化全局应用状态
    let state = AppState::with_hardware(config, db, pipeline, hardware);

    // 7. 启动 GPUI 应用程序
    Application::new().run(move |cx: &mut App| {
        // 全局快捷键（F-017）：键位表必须在使用任何窗口之前注册
        ui::shortcuts::bind_default_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.0), px(860.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(1080.0), px(720.0))),
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

    // 8. 退出收尾：预览用的 ffplay 进程「发起后没人 wait」，`-autoexit` 只在正常播完
    // 时生效。用户在看预览时关窗，这些进程会变成孤儿继续占着视频句柄与音频设备，
    // 因此必须在 `run()` 返回后统一 kill + wait 回收。
    utils::child_registry::retire_all();
    info!("应用已退出，辅助子进程已回收");

    Ok(())
}
