//! Voice2Word — 音视频智能字幕生成器 (Rust + GPUI)
//! 遵循 Codex / Zed 极简现代设计风格

#![recursion_limit = "512"]

// ── 单入口：模块只经 lib crate 引用（voice2word::…），本文件不再 mod 一遍 ──
//
// 此前这里是 `mod app; mod core; mod engines; …`，而 src/lib.rs 又导出同一批
// 模块，结果是整棵 ~2.4 万行代码树（含 2935 行的 ui/editor.rs 与全部 GPUI 界面）
// 被编译两遍：一遍进 lib，一遍进 bin。代价是 check/build 时间与 .rlib/.rmeta 体积
// 无谓翻倍；更隐蔽的副作用是 warning 里混进大量假阳性——`pub fn` 在 lib 里是公开
// API，在 bin 的私有模块里却是「从未使用」，于是真正的新增死代码被淹没。
//
// 改为引用 lib 后模块只有一份定义，dead_code 报的就是真实情况。
use voice2word::app::AppState;
use voice2word::core::TaskPipeline;
use voice2word::engines::{self, FFmpegEngine, LLMEngine, WhisperEngine};
use voice2word::storage::Database;
use voice2word::ui::{self, MainWindow};
use voice2word::utils::{self, AppConfig};

use anyhow::Result;
use gpui::*;
use std::sync::Arc;
use tracing::{info, warn};

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
    // 模型下载的中间文件落在 models/ 与 tools/ 下（不是 %TEMP%），
    // 上面的清扫覆盖不到。进程被强杀时可能残留近 1 GB 的 .part 文件。
    let parts = voice2word::utils::model_download::sweep_stale_parts();
    if parts > 0 {
        info!("启动清扫：已删除 {parts} 个残留的下载中间文件");
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
    // 把让路设置下发给所有引擎的公共出口。此前只有 whisper 与预览播放器
    // 读了配置，SenseVoice / 标点 / LLM / FFmpeg 全部硬编码 CREATE_NO_WINDOW，
    // 用户打开「让路」对它们毫无作用——长音频多进程并行时桌面照样卡。
    engines::media_pipeline::set_yield_to_desktop(config.gpu.yield_to_desktop);
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

    // 3.5 外部依赖体检：把「路径配错」从「转写跑到一半才失败」提前到启动时暴露。
    //
    // 配置分层之后 config.toml 里是便携默认值（tools/ffmpeg.exe 等），真正的位置
    // 可能来自 config.local.toml。若两者都没命中，用户看到的会是转写中途的
    // 「找不到文件」，而根因（某台机器的 SDK/工具装别处去了）很难定位。
    // 这里在启动时逐项核对并一次性打印，缺失的给出可操作的修复指引。
    check_external_dependencies(&config);

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

/// 启动时核对关键外部依赖是否就位，缺失项一次性汇总打印。
///
/// 只**报告**不阻断：模型/工具缺失时程序仍可打开界面（例如用户只想翻看历史工程、
/// 或只想用 SenseVoice 而没装 whisper.cpp），把启动直接打死反而更糟。但必须让
/// 用户在启动日志里就能看到「哪个路径没命中、该往哪写」。
fn check_external_dependencies(config: &AppConfig) {
    // (显示名, 配置值, 是否必需)
    let mut missing: Vec<(String, String)> = Vec::new();

    let required: [(&str, &str); 2] = [
        ("ffmpeg", config.paths.ffmpeg.as_str()),
        ("whisper-cli", config.paths.whisper_cli.as_str()),
    ];
    for (name, raw) in required {
        let resolved = AppConfig::resolve_path(raw);
        if !resolved.exists() {
            missing.push((name.to_string(), resolved.display().to_string()));
        }
    }

    // 模型属于「用到才需要」：缺失不报缺失，交由对应引擎在启用时报错
    let optional: [(&str, Option<&str>); 5] = [
        ("whisper 模型", Some(config.paths.whisper_model.as_str())),
        ("llama-cli", Some(config.paths.llama_cli.as_str())),
        ("SenseVoice 模型", config.paths.sensevoice_model.as_deref()),
        ("CT-Punc 标点模型", config.paths.punc_model.as_deref()),
        ("Silero VAD", config.paths.vad_model.as_deref()),
    ];
    let mut optional_missing = Vec::new();
    for (name, raw) in optional {
        let Some(raw) = raw else { continue };
        let resolved = AppConfig::resolve_path(raw);
        if !resolved.exists() {
            optional_missing.push(format!("{name} ({})", resolved.display()));
        }
    }

    if !missing.is_empty() {
        warn!("外部依赖体检：以下必需组件未找到");
        for (name, path) in &missing {
            warn!("  - {name}: {path}");
        }
        warn!(
            "修复：把这些路径写进 {}（本机专属、不进版本库），例如\n  [paths]\n  ffmpeg = 'C:/path/to/ffmpeg.exe'",
            AppConfig::LOCAL_OVERRIDE
        );
    }
    if !optional_missing.is_empty() {
        info!(
            "外部依赖体检：以下可选组件未找到，对应功能不可用 —— {}",
            optional_missing.join("；")
        );
    }
    if missing.is_empty() && optional_missing.is_empty() {
        info!("外部依赖体检：全部组件就位");
    }

    // 模型/组件缺失时给出**可操作**的指引。此前用户 clone 下来只看到各引擎
    // 「未就绪」，不知道缺什么、也不知道去哪拿。这里明确告诉他：界面上有
    // 一键下载，且下载源是国内镜像（不需要梯子）。
    // 一次扫描算完两个数：分别调用会各读一遍 config.toml（判定要按配置路径找文件）
    // 复用 main 开头已加载的 config，不再读一次 TOML
    let ctx = voice2word::utils::model_download::PresenceContextRef::new(config);
    let absent = voice2word::utils::ITEMS.iter().filter(|i| !ctx.is_present(i)).count();
    if absent > 0 {
        let required = voice2word::utils::ITEMS
            .iter()
            .filter(|i| i.required && !ctx.is_present(i))
            .count();
        info!(
            "模型体检：{absent} 个组件未就位（其中 {required} 个为必需）。\
             启动后可在「性能设置 → 模型与组件」一键下载，模型走国内镜像 hf-mirror、             可执行组件走免梯子的 GitHub 代理，均无需代理。"
        );
    }
}
