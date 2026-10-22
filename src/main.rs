//! Voice2Word — 音视频智能字幕生成器 (Rust + GPUI)
//! 遵循 Codex / Zed 极简现代设计风格

#![recursion_limit = "512"]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
use std::time::Instant;
use tracing::{info, warn};

fn main() -> Result<()> {
    // CLI / 无头模式分流：命中 transcribe、--help、--version 时按对应退出码结束进程，
    // 不初始化日志与 GPUI；无参数或 --gui 返回 None，继续走下面的图形界面启动路径。
    if let Some(code) = voice2word::cli::run(std::env::args()) {
        std::process::exit(code);
    }

    #[cfg(windows)]
    {
        std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1");
    }

    #[cfg(windows)]
    unsafe {
        #[link(name = "user32")]
        extern "system" {
            fn ChangeWindowMessageFilter(message: u32, dwflag: u32) -> i32;
        }
        // 允许 Explorer 拖拽文件进入（即使以管理员身份运行也不受 Windows UIPI 限制）
        const MSGFLT_ADD: u32 = 1;
        ChangeWindowMessageFilter(0x0233, MSGFLT_ADD); // WM_DROPFILES
        ChangeWindowMessageFilter(0x004A, MSGFLT_ADD); // WM_COPYDATA
        ChangeWindowMessageFilter(0x0049, MSGFLT_ADD); // WM_COPYGLOBALDATA
    }
    // 冷启动量化（P1-A9）：从进程入口开始打点，每完成一个阶段就记一次耗时。
    // 只在启动路径上，之后不再有任何开销。
    let mut startup = StartupTimer::new();

    // 1. 初始化双写日志系统 (同时输出至控制台与 logs/ 文本文件，并拦截记录 panic 堆栈)
    let _log_file = match utils::init_logger() {
        Ok(path) => path,
        Err(err) => {
            // 日志系统都没起来，这条错误连 `logs/` 都写不进去——只能弹窗，否则
            // 用户面对的是一个「双击没反应」的进程。
            report_startup_failure("初始化日志系统（创建 logs 目录）", &format!("{err:#}"));
            return Err(err);
        }
    };
    startup.mark("logger_init");

    info!("==========================================");
    info!("Voice2Word 启动中 (Rust + GPUI Edition)...");
    info!("==========================================");

    // 1.2 单实例探测（F6）：只警告、绝不阻断启动。guard 必须活到进程结束，
    //     drop 即释放端口，因此绑定成功本身就成了「已有实例在跑」的判据。
    let _instance_guard = probe_single_instance();

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
    startup.mark("startup_sweep");

    // 2. 加载配置文件
    let config = match AppConfig::load_from_file("config.toml") {
        Ok(cfg) => cfg,
        Err(err) => {
            report_startup_failure("读取配置文件 config.toml", &format!("{err:#}"));
            return Err(err);
        }
    };
    // 配置摘要：**绝不**整份 `{:?}` 打印。`AppConfig` 里有 `translate.api_key`，
    // 一旦随日志写进 `logs/latest.txt`，而崩溃横幅恰好请用户外发日志，密钥就只能吊销。
    // 摘要只保留排障真正需要的几项；`TranslateConfig` 的 `Debug` 也已对密钥打码（双保险）。
    info!(
        engine = %config.paths.whisper_model,
        mode = %config.translate.mode,
        threads = config.pipeline.whisper_threads,
        processors = config.pipeline.whisper_processors,
        language = %config.pipeline.language,
        "配置加载成功"
    );
    startup.mark("config_load");
    // 主题必须在任何界面构建之前设定：颜色 token 在渲染时才读取全局开关。
    //
    // 暂时固定为深色（只用黑色）：界面上已移除主题切换入口，
    // 若还按 `config.ui.theme` 读，一旦旧配置里留着 `light`，用户会被困在浅色里无法回到深色。
    ui::theme::Theme::set_light(false);
    info!("界面主题: 深色");
    // 硬件档案采集（P1-A9 / 冷启动）。此前这里是两步串行、都卡在首帧之前：
    // - sysinfo 的 CPU/内存首采约 850 ms（实测 16 核；结果缓存在**进程级**静态
    //   状态里，之后同一进程内再取 ~1 ms）；
    // - wgpu 适配器枚举（`HardwareProfile::detect`）约 2.7 s（同样实测）。
    //
    // 现在整段挪进一条采集线程，主线程立刻继续建库 / 依赖体检 / 初始化引擎，
    // 到第 3.6 步才 join：等待量从「两笔之和」变成「枚举剩余量」，而档案本身不变。
    //
    // 判定**完全等价**，不是「先按 CPU 跑、后面偷偷改」：
    // - 两个 GPU 开关都关时（`hwaccel_decode=false` 且 `whisper_offload=false`）
    //   探出来的结果只影响预览硬解与 Whisper 卸载，而这两个开关已经把两条路都
    //   钉死在 CPU 上——档案直接取 `cpu_only()`，线程里连适配器都不枚举，
    //   结果与旧写法逐位相同；
    // - 只要有一个开关打开，跑的就是旧写法**同一个** `HardwareProfile::detect()`，
    //   只是执行位置从主线程挪到采集线程；join 之后所有下游判定
    //   （`whisper_gpu` / `gpu_limit_percent` / proxy 硬解后端 / 播放器解码策略 /
    //   `gpu_mode`）读到的值与旧写法逐位相同。
    //
    // 采集线程 panic 时退回 `cpu_only()` 并记一条 warn：与「探不出适配器」同样是
    // 纯 CPU 路线，越保守越不会出现「按有 GPU 跑、其实没有」的坏情况。
    // 另外，CPU/内存那笔首采仍会在这条线程里先做掉（结果丢弃），把 sysinfo 的
    // 冷采样与后面的建库/引擎初始化并行起来——两笔耗时都不再占主线程。
    let gpu_flags_enabled = config.gpu.hwaccel_decode || config.gpu.whisper_offload;
    let hardware_probe = std::thread::spawn(move || {
        if !gpu_flags_enabled {
            // 两个开关都关：无需枚举适配器，瞬间返回纯 CPU 档案 (0ms)
            return engines::HardwareProfile::cpu_only();
        }
        // 采集线程里 panic 不该把启动带死（旧写法只会让进程直接崩），
        // 捕获后按纯 CPU 降级，并留下带位置的日志。
        std::panic::catch_unwind(engines::HardwareProfile::detect).unwrap_or_else(|_| {
            warn!("硬件探测线程异常，按纯 CPU 档案继续启动");
            engines::HardwareProfile::cpu_only()
        })
    });
    // GPU 占用策略（config.toml [gpu]）：给桌面/其他应用留显卡的两位总闸。
    // 这三条只读配置，与适配器无关，可以在采集线程还在跑的时候就先落地。
    if !config.gpu.hwaccel_decode {
        info!("GPU 策略: 预览硬解已关闭 (gpu.hwaccel_decode=false)，预览走 CPU 软解");
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

    // 3. 打开 SQLite 数据库
    //
    // 路径必须经 `resolve_path` 锚到**项目根**，不能直接把相对名交给 SQLite：
    // 相对路径是相对 cwd 解析的，而 cwd 由启动器决定。从开始菜单/桌面快捷方式
    // 启动时 cwd 是 `C:\Windows\System32`，SQLite 会试图在系统目录下建库并因
    // 无写权限而失败——`Database::open` 返回 Err，`main` 直接 `?` 退出，
    // **用户连界面都看不到**（实测报错：`unable to open database file`）。
    // 库仍然留在项目根（与旧行为一致），只是不再随 cwd 漂移。
    // 项目根不可写时的回退：数据库退到 %LOCALAPPDATA%\Voice2Word（每用户可写）。
    //
    // 此前这里只有「项目根」一个位置，于是装在只读目录（Program Files、企业策略、
    // 只读 U 盘、拷贝自别的用户）时 SQLite 报 `unable to open database file`，
    // 程序**直接退出、连界面都看不到**，而错误文案只说「打开 SQLite 数据库失败」，
    // 完全指向不了真正的原因（目录只读）。这正是「本机好使、别人电脑不好使」
    // 的一类：开发机的项目根可写，客户机的安装目录不一定可写。
    let mut db_path = AppConfig::resolve_path("voice2word.db");
    let db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(err) => {
            // 第一次失败：换到可写目录再试一次。
            let fallback_root = AppConfig::writable_data_dir();
            let fallback_path = fallback_root.join("voice2word.db");
            if fallback_path != db_path {
                match Database::open(&fallback_path) {
                    Ok(db) => {
                        warn!(
                            original = %db_path.display(),
                            fallback = %fallback_path.display(),
                            error = %format!("{err:#}"),
                            "项目根不可写，数据库已退到可写目录"
                        );
                        db_path = fallback_path;
                        db
                    }
                    Err(fallback_err) => {
                        report_startup_failure(
                            "打开 SQLite 数据库",
                            &format!(
                                "原路径：{}\n回退路径：{}\n原始错误：{err:#}\n回退错误：{fallback_err:#}",
                                db_path.display(),
                                fallback_path.display()
                            ),
                        );
                        return Err(fallback_err);
                    }
                }
            } else {
                report_startup_failure(
                    "打开 SQLite 数据库",
                    &format!("路径：{}\n{err:#}", db_path.display()),
                );
                return Err(err);
            }
        }
    };
    info!(db = %db_path.display(), "SQLite 数据库已连接");
    startup.mark("db_open");

    // 3.5 外部依赖体检：把「路径配错」从「转写跑到一半才失败」提前到启动时暴露。
    //
    // 配置分层之后 config.toml 里是便携默认值（tools/ffmpeg.exe 等），真正的位置
    // 可能来自 config.local.toml。若两者都没命中，用户看到的会是转写中途的
    // 「找不到文件」，而根因（某台机器的 SDK/工具装别处去了）很难定位。
    // 这里在启动时逐项核对并一次性打印，缺失的给出可操作的修复指引。
    // 外部依赖体检是**纯诊断**（只做文件探测 + 写日志），不参与任何决策：
    // 引擎该用什么路径、GPU 策略怎么定，都不看它的结论。它却要在启动关键路径上
    // 花 ~30-50ms（含 PE 导入表递归解析）。丢到后台线程，与后面的建库/引擎初始化
    // 重叠，从「首帧之前」的时间轴上整个移走。
    let dep_config = config.clone();
    std::thread::spawn(move || check_external_dependencies(&dep_config));
    startup.mark("dependency_check_deferred");

    // 3.6 与硬件采集线程汇合：主线程已把建库、依赖体检跑完，此刻只剩「枚举的
    //     剩余时间」要等——这正是把探测丢到后台线程的目的。
    //     档案本身来自采集线程的 `detect()`（开关打开时），或这里的 `cpu_only()`
    //     （开关全关时，与旧写法同一条分支）。线程若崩溃，`join` 返回 Err，
    //     同样退回纯 CPU 档案。
    let mut hardware = match hardware_probe.join() {
        Ok(profile) if gpu_flags_enabled => profile,
        _ => engines::HardwareProfile::cpu_only(),
    };
    // 用户关掉硬解开关时把档案里的能力也按下去（与旧写法同一处、同一顺序）
    if !config.gpu.hwaccel_decode {
        hardware.hardware_decode = false;
    }
    let whisper_gpu = hardware.use_gpu_pipeline() && config.gpu.whisper_offload;
    if !whisper_gpu {
        info!("GPU 策略: 转写推理走纯 CPU (gpu.whisper_offload=false 或无独显/核显不可用)");
    }
    // GPU 占用上限：让路只降 CPU 调度优先级，压不住已经排进 GPU 队列的命令缓冲，
    // 想让桌面真正跟手必须按占空比给 GPU 留空窗。
    // 这一步必须在 join 之后：占空比取决于 `whisper_gpu`，而它取决于真实适配器。
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
    info!(
        use_gpu_pipeline = hardware.use_gpu_pipeline(),
        adapter = %hardware.adapter_name,
        vendor = hardware.vendor.map(|v| v.label()).unwrap_or("—"),
        "媒体渲染后端已选择"
    );
    startup.mark("hardware_detect");

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
        let model = config
            .paths
            .sensevoice_model
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/model.int8.onnx"));
        let tokens = config
            .paths
            .sensevoice_tokens
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/tokens.txt"));
        let vad = config
            .paths
            .sensevoice_vad
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/sensevoice/silero_vad.onnx"));
        if runner.exists() && model.exists() && tokens.exists() && vad.exists() {
            let provider = config.gpu.resolve_onnx_provider();
            info!(
                "SenseVoice 极速非自回归语音识别引擎已就绪: {:?} (provider={provider})",
                model
            );
            Some(Arc::new(
                engines::SenseVoiceEngine::with_python_and_provider(
                    runner,
                    model.clone(),
                    tokens,
                    vad,
                    config.pipeline.whisper_threads,
                    python_path.clone(),
                    provider,
                )
                // 族由模型路径决定：指向 models/dolphin/ 就是 Dolphin，指向 SenseVoice 目录
                // 就是原样（认不出也收敛到 SenseVoice，行为不变）。
                .with_family(engines::sensevoice::model_family_from_path(&model), None),
            ))
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
        let model = config
            .paths
            .punc_model
            .as_ref()
            .map(|p| AppConfig::resolve_path(p))
            .unwrap_or_else(|| AppConfig::resolve_path("models/punc/model.int8.onnx"));
        if runner.exists() && model.exists() {
            let provider = config.gpu.resolve_onnx_provider();
            info!(
                "CT-Transformer 极速标点引擎已就绪: {:?} (provider={provider})",
                model
            );
            Some(Arc::new(
                engines::PunctuationEngine::with_python_and_provider(
                    runner,
                    model,
                    4,
                    python_path.clone(),
                    provider,
                ),
            ))
        } else {
            warn!(
                "CT-Transformer 极速标点引擎未就绪 (runner={}, model={})",
                runner.exists(),
                model.exists()
            );
            None
        }
    };

    startup.mark("engine_init");

    // 5. 编排流水线管线（此时硬件档案已落定）
    let pipeline = Arc::new(TaskPipeline::new(ffmpeg, whisper, sensevoice, llm, punc));

    // 6. 初始化全局应用状态
    let state = AppState::with_hardware(config, db, pipeline, hardware);
    // 退出时要对数据库做收尾维护（WAL checkpoint），但 `state` 会被 move 进
    // GPUI 的 run 闭包；`Database` 内部是 `Arc<Mutex<Connection>>`，先克隆一份句柄。
    let db_for_shutdown = state.db.clone();
    startup.mark("app_state_init");

    // 7. 启动 GPUI 应用程序
    Application::new().run(move |cx: &mut App| {
        // 全局快捷键（F-017）：键位表必须在使用任何窗口之前注册
        ui::shortcuts::bind_default_keys(cx);
        let default_size = size(px(1280.0), px(860.0));
        // 恢复上次的窗口大小与位置（换显示器/改分辨率后会被拉回屏幕内）。
        // 没有记录、记录损坏、或算出来的位置不合法时，一律回到默认居中。
        let bounds = utils::window_state::load()
            .map(|geo| {
                // 显示器尺寸是物理像素，窗口几何是逻辑像素：除以缩放系数换算。
                // 只做一次「左上角别跑到屏幕外」的兜底；拿不到显示器信息就跳过。
                let screen = cx.primary_display().map(|d| {
                    let s = d.bounds().size;
                    (f32::from(s.width), f32::from(s.height))
                });
                let geo = geo.clamp_origin_to(screen);
                Bounds {
                    origin: point(px(geo.x), px(geo.y)),
                    size: size(px(geo.width), px(geo.height)),
                }
            })
            .unwrap_or_else(|| Bounds::centered(None, default_size, cx));

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // 最小尺寸要留得比「常见小屏的逻辑分辨率」还小，否则窗口根本放不下。
                //
                // 1080p 笔记本在 **150% 缩放**下逻辑分辨率只有 1280×720，减去任务栏后
                // 工作区约 1280×680：原来的下限 1080×720 比工作区还高，用户既不能把窗口
                // 缩到屏幕内、又不会自动最大化，底部的时间轴/状态栏永远在屏幕外。
                // 这是很常见的一档（Win10/11 对 1080p 的推荐缩放就是 125%~150%）。
                //
                // 960×600 仍高于布局的实际需求（左侧栏可折叠成 38px，剪辑台在 <820 宽时
                // 自动改成上下堆叠），但足够小，任何常见屏幕都能容纳。
                window_min_size: Some(size(px(960.0), px(600.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Voice2Word — 音视频智能字幕生成器".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let root = cx.new(|cx| MainWindow::new(state.clone(), cx));
                // 系统级关窗（Alt+F4 / 任务栏右键 / 系统关闭按钮）也要过一遍脏数据守卫，
                // 否则用户改完字幕直接关窗会丢掉最后一段未落库的编辑。
                ui::install_window_close_guard(window, &root, cx);
                root
            },
        )
        .expect("打开主窗口失败");
        startup.mark("first_window");
    });

    // 8. 退出收尾：预览用的 ffplay 进程「发起后没人 wait」，`-autoexit` 只在正常播完
    // 时生效。用户在看预览时关窗，这些进程会变成孤儿继续占着视频句柄与音频设备，
    // 因此必须在 `run()` 返回后统一 kill + wait 回收。
    utils::child_registry::retire_all();
    info!("应用已退出，辅助子进程已回收");

    // 9. 数据库收尾维护：把 WAL 并回主库并截断，避免 `-wal` 长期膨胀、
    //    拖慢下次冷启动。纯尽力而为，失败不影响退出。
    db_for_shutdown.maintain_on_shutdown();

    Ok(())
}

/// 单实例探测（F6）：用「绑定固定本地端口」当锁，**不引新依赖**。
///
/// 为什么是 `TcpListener`：`std` 自带；Windows 上端口独占是内核语义，进程退出
/// （含崩溃 / 任务管理器结束）端口立即释放，不会像锁文件那样留下「僵尸锁」，
/// 也不需要往项目根写任何东西。
///
/// # 语义：**只警告，绝不阻断**
///
/// - bind 成功 → 返回 listener，由 `main` 持有到进程结束，这就是「本进程是唯一实例」
///   的凭据；
/// - bind 失败 → 可能是已有实例，也可能是端口被别的程序占用 / 系统策略禁止监听
///   （防火墙、受限容器）。两者无法区分，因此**绝不能**据此退出：让程序因为
///   「端口被占」起不来，比两个实例并发写库的损害大得多。只记一条警告。
///
/// # 已知局限（为什么不顺便把已有窗口前置）
///
/// 这里在首个窗口创建**之前**执行，主窗口还不存在；跨进程激活已有窗口要按标题
/// 枚举 `user32` 窗口并处理最小化/多桌面/权限隔离等分支，在启动路径上并不可靠。
/// 宁可只做「记录警告」这一半，也不做半吊子的窗口激活。
fn probe_single_instance() -> Option<std::net::TcpListener> {
    /// 固定探测端口：仅用于「同一台机器上是否已有本应用」，不对外服务。
    const PROBE_PORT: u16 = 39217;
    match std::net::TcpListener::bind(("127.0.0.1", PROBE_PORT)) {
        Ok(listener) => Some(listener),
        Err(err) => {
            warn!(
                %err,
                port = PROBE_PORT,
                "单实例探测：端口已被占用（可能已有另一个 Voice2Word 在运行）。继续启动——但两个实例同时写 voice2word.db 可能互相覆盖，建议只保留一个窗口"
            );
            // 记下这个事实：界面启动后会据此提示用户，而不是只在日志里留一句。
            // 两个实例同时写同一个库不会把库写坏（WAL + busy_timeout），
            // 但会互相覆盖对方的编辑——那是**看不见的丢改动**。
            voice2word::utils::set_other_instance_running(true);
            None
        }
    }
}

/// 启动关键路径的**致命**错误出口：写日志 + 原生弹窗。
///
/// 这些错误一旦发生，`main` 就在建窗之前返回、进程直接退出。此前它们只经 `?`
/// 冒泡到 `main` 的 `Result`：Release 版是 Windows GUI 子系统、没有控制台，Rust
/// 默认把 `Error: …` 写到 stderr，随进程一起被丢弃；`tracing` 也拿不到（`?` 不落
/// 日志）。三种最常见的启动失败——安装目录只读（写不了 `logs/` 或 `voice2word.db`）、
/// 配置写坏、数据库打不开——用户看到的都是「双击没反应」，连日志文件都没有。
///
/// 这里统一处理：能记就记进日志（日志订阅者已就绪时），再弹一个原生错误框，
/// 让桌面用户至少看到「哪一步失败、为什么、怎么办」。
fn report_startup_failure(stage: &str, detail: &str) {
    tracing::error!(stage, detail, "启动致命错误");
    let message = voice2word::app::format_startup_failure(stage, detail);
    show_startup_error_dialog(&message);
}

/// 弹出启动失败对话框。
///
/// Windows 走原生 `MessageBoxW`（与安装程序同一条路）：GUI 子系统下没有控制台，
/// 这是唯一能被用户看到的出口。它自带模态消息循环，可在 GPUI 起来之前的主线程调用。
#[cfg(windows)]
fn show_startup_error_dialog(message: &str) {
    // MessageBoxW 的 uType：MB_OK(=0) | MB_ICONERROR(=0x10)
    const MB_OK_ICONERROR: u32 = 0x0000_0010;

    let title = to_wide("Voice2Word 启动失败");
    let body = to_wide(message);
    // 只关心「用户看到了」；按钮返回值（恒为 IDOK）不参与流程。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK_ICONERROR,
        );
    }
}

/// 非 Windows 平台没有原生弹窗可用，退回标准错误输出（有终端时可见）。
#[cfg(not(windows))]
fn show_startup_error_dialog(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(
        hwnd: *mut std::ffi::c_void,
        text: *const u16,
        caption: *const u16,
        u_type: u32,
    ) -> i32;
}

/// 启动阶段耗时打点器：`mark()` 打印「上一阶段 → 此刻」与「进程入口 → 此刻」的
/// 毫秒数。用来量化冷启动各阶段（P1-A9），不做任何常驻开销。
struct StartupTimer {
    last: Instant,
    start: Instant,
}

impl StartupTimer {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last: now,
            start: now,
        }
    }

    fn mark(&mut self, stage: &str) {
        let now = Instant::now();
        let stage_ms = now.duration_since(self.last).as_millis() as u64;
        let total_ms = now.duration_since(self.start).as_millis() as u64;
        info!(stage, stage_ms, total_ms, "启动耗时");
        self.last = now;
    }
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

    // whisper-cli 就位但**缺少运行库**（MinGW libgcc/libstdc++/libgomp）时，exe
    // 存在也启动不了，进程会以 0xC0000139（STATUS_ENTRYPOINT_NOT_FOUND）退出，
    // 且无任何 stderr 输出——用户看到的是「转写失败：」空串，无从排查。
    // 这里在启动时就把这类「就位但不可运行」的坑提前点破。
    check_whisper_cli_runtime_deps(config);
    // llama.cpp 同理：入口 exe 只有几 KB，缺同目录 DLL 会静默以 0xC0000139 退出。
    check_llama_runtime_deps(config);

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
    let absent = voice2word::utils::ITEMS
        .iter()
        .filter(|i| !ctx.is_present(i))
        .count();
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

/// 核对 whisper-cli 及其随附 DLL 的**运行库**是否齐备。
///
/// 背景：whisper.cpp 的 Windows 构建把代码拆进同目录 DLL，且运行库是动态链接的：
/// - **MinGW 构建**依赖 `libgcc_s_seh-1.dll` / `libstdc++-6.dll` / `libgomp-1.dll`；
/// - **MSVC 构建**依赖 `VCRUNTIME140.dll` / `MSVCP140.dll` / `VCOMP140.dll`（VC++ 运行库，
///   Win10/11 大多自带）；
/// - **Vulkan 构建**再依赖 `vulkan-1.dll`（显卡驱动自带）。
///
/// 这些库缺一个，exe 启动即被 Windows 以 `0xC0000139`
/// （STATUS_ENTRYPOINT_NOT_FOUND）秒杀——**没有任何 stderr**，`WhisperEngine`
/// 只会拿到一个空错误串，用户看到「转写失败：」完全无从下手。
///
/// # 为什么不能只查一组写死的 DLL 名
///
/// 启动时核对 **llama.cpp** 推理后端的运行时依赖（同目录 DLL 是否齐全）。
///
/// 为什么必须查：llama.cpp 的 `llama-completion.exe` 只有 ~9 KB，真正的代码在
/// 同目录的 `llama-common.dll` / `llama-server-impl.dll` / `ggml-*.dll` 里。
/// 缺一个时进程会立刻以 `0xC0000139`（STATUS_ENTRYPOINT_NOT_FOUND）退出，
/// **且没有任何 stderr**——用户看到的是「点了离线翻译没反应」，日志里也只有
/// 「转写失败」这种笼统信息，根本指向不了真正的原因。
///
/// 这是「本机好使、别人电脑不好使」的典型形态：开发机上 DLL 就在仓库 `tools/`
/// 下（本机跑过），客户机上是安装包没带过去。
///
/// 与 whisper 那边（[`check_whisper_cli_runtime_deps`]）同一套机制：解析 PE 导入表，
/// 只对「这份二进制真正需要、却找不到」的 DLL 报警。
fn check_llama_runtime_deps(config: &AppConfig) {
    // llama-cli 与 llama-completion 都查：前者是常驻服务、后者是命令行回退，
    // 两条路径都要能用。
    for raw in [&config.paths.llama_cli] {
        let exe = AppConfig::resolve_path(raw);
        if !exe.exists() {
            continue; // exe 缺失已由 optional 检查报告，这里不重复
        }
        let missing = voice2word::utils::pe_imports::missing_imports(&exe);
        if missing.is_empty() {
            continue;
        }
        let hint =
            "离线翻译 / 润色会直接失败。修复：把缺失的 DLL 与 llama-completion.exe 放到同一目录，"
                .to_string()
                + "或在「性能设置 → 模型与组件」里重新下载 llama.cpp 组件。";
        warn!(
            exe = %exe.display(),
            dlls = ?missing,
            "llama.cpp 运行库缺失：exe 存在但启动会立即退出（0xC0000139），且无任何报错。{hint}"
        );
    }
}
/// 早期版本无条件去查 MinGW 三件套。把 whisper-cli 换成**官方 MSVC 构建**后，
/// 它根本不导入 libgcc/libstdc++，于是每次启动都误报「运行库缺失」，把用户往
/// 错误的修复方向带。正确做法是**解析 PE 导入表**，只对「这份二进制真正需要、
/// 却在任何搜索路径都找不到」的 DLL 报警（见 [`voice2word::utils::pe_imports`]）。
///
/// 这里只**报告**不阻断：缺失时在启动日志里明确点出缺哪个，并给可操作的修复指引。
fn check_whisper_cli_runtime_deps(config: &AppConfig) {
    let cli = AppConfig::resolve_path(&config.paths.whisper_cli);
    if !cli.exists() {
        return; // exe 缺失已由 `required` 检查报告，这里不重复
    }

    // 沿同目录 DLL 递归展开导入闭包，逐个判定能否被加载器解析。
    // 官方 whisper.cpp 构建里 VCOMP140 藏在 ggml-cpu.dll 的导入表里，
    // 只看入口 exe 会漏掉它——见 utils::pe_imports::missing_imports。
    let missing = voice2word::utils::pe_imports::missing_imports(&cli);
    if missing.is_empty() {
        return;
    }

    // 按构建类型给出更贴切的修复指引：MinGW 缺的是编译器运行库，
    // MSVC 缺的是 VC++ 运行库（装一次 VC++ Redistributable 即可）。
    let mingw = missing
        .iter()
        .any(|d| d.starts_with("libgcc") || d.starts_with("libstdc++") || d.starts_with("libgomp"));
    let hint = if mingw {
        "把缺失的 DLL 与 whisper-cli 放到同一目录，或用「性能设置 → 模型与组件」重新下载官方包"
    } else {
        "安装「Microsoft Visual C++ 可再发行程序包 (x64)」（vc_redist.x64.exe），或把缺失的 DLL 与 whisper-cli 放到同一目录"
    };
    warn!(
        dlls = ?missing,
        "whisper-cli 运行库缺失：exe 存在但启动会立即退出（0xC0000139），且无任何报错。修复：{}。",
        hint
    );
}
