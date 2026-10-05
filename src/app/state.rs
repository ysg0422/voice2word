//! 应用程序全局状态

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::TaskPipeline;
use crate::engines::TranslateMode;
use crate::storage::{Database, TaskRecord};
use crate::subtitle::{plan_time_edit, Segment, MIN_EDIT_DUR};
use crate::utils::{AppConfig, FrameCache};
use crate::engines::{HardwareProfile, ProxyManager};

/// 字幕翻译目标语言可选清单（主界面翻译面板与状态栏共用同一份，避免两处漂移）
pub const TRANSLATE_TARGET_LANGS: [&str; 8] = [
    "简体中文",
    "繁体中文",
    "English",
    "日本語",
    "한국어",
    "Русский",
    "Français",
    "Deutsch",
];

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    Idle,
    Processing { stage: String, progress: f64, detail: String },
    Completed,
    Failed(String),
}

/// 批量转写队列 (F-012) 中单个文件的处理状态
#[derive(Debug, Clone, PartialEq)]
pub enum QueueState {
    /// 等待处理（含上一轮失败后重新排队）
    Pending,
    /// 正在转写
    Running,
    /// 已完成，附带产出字幕句数
    Done { segments: usize },
    /// 失败或被用户取消，附带原因
    Failed(String),
}

impl QueueState {
    /// 队列卡片上的状态文案
    pub fn label(&self) -> String {
        match self {
            Self::Pending => "等待中".to_string(),
            Self::Running => "转写中…".to_string(),
            Self::Done { segments } => format!("已完成 {segments} 句"),
            Self::Failed(reason) => format!("失败：{reason}"),
        }
    }

    /// 是否还需要（重新）处理。「开始全部」只跑这些，已成功的文件不会被重复烧算力。
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::Pending | Self::Failed(_))
    }

    /// 是否还在等待处理。
    ///
    /// 只有「等待中」算。失败项必须先由「开始全部」显式重新排队才可再跑，
    /// 否则批量续跑会在同一个失败文件上无限打转。
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// 是否为失败项（「开始全部」会把它们重新排队，和待处理项一起重试）
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// 批量队列中的一个待转写文件
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub path: PathBuf,
    pub name: String,
    /// 时长（秒）。入队时先置 0，由后台探测补齐；0 表示尚未探测到
    pub duration: f64,
    pub state: QueueState,
}

impl QueueItem {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("媒体文件")
            .to_string();
        Self {
            path,
            name,
            duration: 0.0,
            state: QueueState::Pending,
        }
    }
}

/// 队列里下一个待处理项的下标。
///
/// 只看「等待中」：已成功的不重复烧算力；失败的也不在这里被自动重挑，
/// 否则单个坏文件会让批量续跑卡在原地无限重试。失败项要重跑，
/// 必须由用户点「开始全部」显式重新排队。
pub fn next_pending_index(queue: &[QueueItem]) -> Option<usize> {
    queue.iter().position(|i| i.state.is_pending())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceMetrics {
    pub sys_cpu: f32,          // 系统总 CPU (0.0 ~ 100.0)
    pub sys_mem_used: u64,     // 系统已用内存 (bytes)
    pub sys_mem_total: u64,    // 系统总内存 (bytes)

    pub proc_name: String,     // 进程标识 (如 "Qwen2.5 LLM" 或 "Voice2Word")
    pub proc_cpu: f32,         // 进程 CPU (0.0 ~ 100.0)
    pub proc_mem: u64,         // 进程占用内存 (bytes)
    pub is_model_running: bool,// 是否有模型正在工作
}

impl ResourceMetrics {
    /// 格式化字节数，如 1.85 GB 或 420.5 MB
    pub fn format_bytes(bytes: u64) -> String {
        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if gb >= 1.0 {
            format!("{:.2} GB", gb)
        } else {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            format!("{:.1} MB", mb)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTab {
    Editor,       // 剪辑校对工作台 (主界面，默认)
    Generate,     // 智能转写生成 (轻量化无卡顿进度)
    Library,      // 历史解析视频库 (视频资产库)
    Performance,  // 性能与推理设置 (硬件检测、性能评估、AI 推理决策)
}

/// 模型档位：控制 Whisper 模型大小与量化精度，平衡速度与抗口音能力
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhisperModelTier {
    SenseVoice,  // 阿里 SenseVoice 极速 — model.int8.onnx (非自回归单次出字，极致提速 5~8 倍)
    Fast,        // 极速 Base — ggml-base.bin (39M 参数)
    #[default]
    Balanced,    // 均衡 Small — ggml-small.bin (244M 参数)
    TurboSpeed,  // 极速 Turbo — ggml-large-v3-turbo-q5_0.bin (Q5 破带宽版，提速 25%~30%)
    Precise,     // 高精 Turbo — ggml-large-v3-turbo-q8_0.bin (Q8 旗舰版，抗口音吞音)
}

/// 润色模式：CT-Punc 极速标点恢复 vs Qwen 大模型深度润色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolishMode {
    #[default]
    PuncFast,  // 极速标点 (CT-Punc · 仅数秒)
    QwenDeep,  // 深度润色 (Qwen · 较慢)
}

impl PolishMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolishMode::PuncFast => "punc",
            PolishMode::QwenDeep => "qwen",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "qwen" | "llm" => PolishMode::QwenDeep,
            _ => PolishMode::PuncFast,
        }
    }
}

impl WhisperModelTier {
    /// 启动时按配置文件选择 UI 档位，避免运行时覆盖掉配置中的 Whisper 模型。
    pub fn from_model_path(path: &str) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
        match name.as_str() {
            "model.int8.onnx" => Some(Self::SenseVoice),
            "ggml-base.bin" => Some(Self::Fast),
            "ggml-small.bin" | "ggml-small-q5_0.bin" => Some(Self::Balanced),
            "ggml-large-v3-turbo-q5_0.bin" => Some(Self::TurboSpeed),
            "ggml-large-v3-turbo-q8_0.bin" => Some(Self::Precise),
            _ => None,
        }
    }

    /// 返回对应的模型文件名（相对于 models/whisper/ 或 models/sensevoice/ 目录）
    pub fn model_filename(self) -> &'static str {
        match self {
            Self::SenseVoice => "model.int8.onnx",
            Self::Fast       => "ggml-base.bin",
            Self::Balanced   => "ggml-small-q5_0.bin",
            Self::TurboSpeed => "ggml-large-v3-turbo-q5_0.bin",
            Self::Precise    => "ggml-large-v3-turbo-q8_0.bin",
        }
    }

    /// 返回相对路径（如果 TurboSpeed 本地未下载完成，自动平滑回退至 Q8）
    pub fn model_relative_path(self) -> String {
        match self {
            Self::SenseVoice => "models/sensevoice/model.int8.onnx".to_string(),
            Self::Fast       => "models/whisper/ggml-base.bin".to_string(),
            Self::Balanced => {
                let preferred = "models/whisper/ggml-small-q5_0.bin".to_string();
                if crate::utils::AppConfig::resolve_path(&preferred).exists() {
                    preferred
                } else {
                    "models/whisper/ggml-small.bin".to_string()
                }
            }
            Self::TurboSpeed => {
                let preferred = "models/whisper/ggml-large-v3-turbo-q5_0.bin".to_string();
                if !crate::utils::AppConfig::resolve_path(&preferred).exists() {
                    "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string()
                } else {
                    preferred
                }
            }
            Self::Precise    => "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string(),
        }
    }

    /// CPU 下相对视频时长的转写耗时系数（校准自 32 分钟样片）。
    fn cpu_realtime_factor(self) -> f64 {
        match self {
            // SenseVoice 实测：32 分钟样片约 134 秒（8 线程）。
            // 旧值 0.4/32 来自早期预估，实际偏低 5.5 倍，会把「预计耗时」
            // 显示成 24 秒而用户要等 2 分多钟，属于误导，故按实测重标。
            Self::SenseVoice => 2.4 / 32.0,
            Self::Fast       => 1.5 / 32.0,
            Self::Balanced   => 3.8 / 32.0, // Small-Q5 CPU 16 线程约 2.8 分钟/32 分钟样片
            Self::TurboSpeed => 5.6 / 32.0, // Q5 降低内存带宽传输，提速约 30%
            Self::Precise    => 8.0 / 32.0,
        }
    }

    /// SenseVoice 的线程收益曲线（实测标定）。
    ///
    /// 16 核 AMD 机器、6 分钟样本实测：8 线程 32.6 秒，16 线程 41.6 秒 ——
    /// 非自回归 ONNX 推理在 8 线程附近已饱和，继续加线程反而因调度与内存带宽
    /// 争用而变慢。因此 8 线程以内按幂次收益递减，超过 8 线程按实测折损递增。
    fn sensevoice_thread_factor(threads: u32) -> f64 {
        let t = threads.max(1) as f64;
        if t <= 8.0 {
            (8.0 / t).powf(0.45)
        } else {
            1.0 + (t - 8.0) * 0.035
        }
    }

    /// Whisper.cpp 线程加速有收益递减：4→8 约 1.45×，8→16 约 1.39×。
    pub fn thread_time_factor(threads: u32) -> f64 {
        let t = threads.max(1) as f64;
        (8.0 / t).powf(0.45).clamp(0.55, 2.2)
    }

    /// 预估转写秒数。GPU 上线程收益更弱；润色额外加固定开销。
    pub fn estimate_seconds(
        self,
        duration_sec: f64,
        threads: u32,
        use_gpu: bool,
        enable_polish: bool,
    ) -> f64 {
        let media = duration_sec.max(1.0);
        let mut secs = media * self.cpu_realtime_factor();
        let thread_f = Self::thread_time_factor(threads);
        if self == Self::SenseVoice {
            secs *= Self::sensevoice_thread_factor(threads);
        } else if use_gpu {
            secs *= 0.28 * (0.72 + 0.28 * thread_f);
        } else {
            secs *= thread_f;
        }
        if enable_polish && self != Self::SenseVoice {
            secs += (media / 60.0) * 2.4;
        }
        secs.max(1.0)
    }

    pub fn format_eta(seconds: f64) -> String {
        if seconds < 60.0 {
            format!("约 {:.0} 秒", seconds.max(1.0))
        } else if seconds < 3600.0 {
            let mins = seconds / 60.0;
            if mins < 10.0 {
                format!("约 {:.1} 分钟", mins)
            } else {
                format!("约 {:.0} 分钟", mins)
            }
        } else {
            format!("约 {:.1} 小时", seconds / 3600.0)
        }
    }
}

/// 一次编辑前的界面快照，供撤销/重做还原。
///
/// 只装「编辑会动到、且用户看得见」的状态：字幕表本身，加上选中项、播放指针
/// 与文本编辑缓冲。像 `total_duration`、波形、代理视频这类派生态不进来——
/// 它们要么由字幕表推导，要么与本次编辑无关，装进来只会让快照失真。
#[derive(Clone)]
pub struct EditSnapshot {
    pub segments: Vec<Segment>,
    pub selected_segment_index: Option<usize>,
    pub current_time: f64,
    pub editing_text: String,
}

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub db: Database,
    pub pipeline: Arc<TaskPipeline>,

    pub selected_file: Option<PathBuf>,
    pub transcribe_file: Option<PathBuf>,
    pub transcribe_duration: f64,
    pub status: ProcessStatus,
    pub segments: Vec<Segment>,
    pub recent_tasks: Vec<TaskRecord>,

    // 实时流式转写数据与当前识别推进绝对时间 (秒)
    pub streaming_segments: Vec<Segment>,
    pub streaming_current_sec: f64,

    // 处理选项
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    pub polish_mode: PolishMode,
    pub whisper_threads: u32,
    pub whisper_model_tier: WhisperModelTier,

    /// 用户已点击「终止转写」，等待管线收尾事件（用于区分取消与真实失败）
    pub cancel_requested: bool,

    /// 字幕编辑产生的未落库脏标记（去抖：切句/跳转/播放/导出时统一写回）
    pub segments_dirty: bool,

    /// 当前编辑的工程对应的数据库主键（`tasks.id`）。
    ///
    /// 同一个 `file_path` 允许存在多条历史记录（每完成一次转写就插一行，
    /// 0 秒缓存命中按 `id DESC` 取最新一条），所以编辑结果只能按主键写回，
    /// 不能按路径——否则会连带覆盖同路径的其它记录。
    /// `None` 表示当前工程尚未落过库（没有可写回的行）。
    pub active_task_id: Option<i64>,

    /// 字幕内容的修订号：任何会改变 `segments` 内容或长度的操作都必须递增。
    /// 剪辑台的字幕清单搜索结果是缓存渲染的，靠这个号判断缓存是否失效，
    /// 避免每帧都对上千条字幕重跑一遍子串匹配。
    pub segments_revision: u64,

    /// 撤销栈：每次编辑前压入一份受影响状态的快照（见 [`EditSnapshot`]）。
    /// 快照式而非「反向操作」式：编辑动作有拆分/合并/删除/改时间五类，
    /// 逐类写逆操作既啰嗦又容易写错，整段克隆换来的是绝不会还原错。
    pub undo_stack: Vec<EditSnapshot>,
    /// 重做栈：撤销时把当前状态挪到这里，一旦发生新的编辑即清空。
    pub redo_stack: Vec<EditSnapshot>,
    /// 正在合并进同一层撤销记录的「文本编辑」目标片段序号。
    /// 同一句的连续输入只记一层撤销，换句或插入别的编辑即置 `None`。
    pub undo_text_coalesce: Option<usize>,

    // 字幕翻译相关状态
    pub is_translating: bool,
    pub translate_progress: f32,
    pub translate_status_msg: String,
    pub translate_target_lang: String,
    /// 翻译引擎档位：本地 Qwen 离线 / 在线 OpenAI 兼容 API。
    /// 与 `config.toml` 的 `[translate].mode` 双向同步，UI 切换后立即落盘。
    pub translate_mode: TranslateMode,

    // 硬件与模型资源监控
    pub metrics: ResourceMetrics,

    // 剪辑校对工作台状态 (主界面)
    pub active_tab: WorkspaceTab,
    pub current_time: f64,
    pub total_duration: f64,
    pub is_playing: bool,
    pub selected_segment_index: Option<usize>,
    pub preview_frame_path: Option<PathBuf>,
    pub timeline_zoom: f64,
    pub editing_text: String,

    // 视频帧缓存（优化拖动时间轴性能）
    pub frame_cache: Arc<FrameCache>,

    /// 当前视频真实分辨率（异步探测，驱动监视器画面等比适配与字幕定位）
    pub video_width: u32,
    pub video_height: u32,
    /// 已完成分辨率探测的视频路径（防止重复探测）
    pub dims_probed_for: Option<PathBuf>,

    // 实时内嵌视频播放引擎
    pub video_player: Arc<crate::engines::VideoPlayerEngine>,
    /// Runtime-selected rendering/decode policy shared by preview and seek.
    pub hardware: HardwareProfile,
    pub proxy_manager: Arc<ProxyManager>,
    pub preview_source: Option<PathBuf>,
    /// User-overridable: CPU machines force proxy on by default, but it can be turned off.
    pub proxy_enabled: bool,
    pub proxy_busy: bool,

    // 性能检测与 AI 推理决策
    pub hardware_info: crate::core::HardwareInfo,
    pub performance_level: crate::core::PerformanceLevel,
    pub user_strategy: crate::core::UserStrategy,
    pub recommended_profile: crate::core::InferenceProfile,
    pub benchmark_result: Option<crate::core::BenchmarkResult>,
    pub is_benchmarking: bool,
    /// 性能设置页配置说明卡的展开状态（右上角问号按钮切换）
    pub show_perf_help: bool,

    /// GPU 占用策略 ("full" 全速 | "balanced" 均衡 | "eco" 低占用 | "cpu" 纯 CPU)，
    /// 改动写入 config.toml 并重启生效
    pub gpu_mode: String,

    /// 当前工程的音频波形包络（时间轴波形轨）。`None` 表示尚未提取或提取失败。
    /// 用 `Arc` 是因为渲染每帧只读，且后台线程提取完成后一次性替换。
    pub waveform: Option<Arc<crate::engines::WaveformData>>,
    /// 正在后台提取波形的媒体路径。用于防止同一文件重复派发提取任务。
    pub waveform_busy_for: Option<PathBuf>,

    /// 批量转写队列 (F-012)。空队列表示处于单文件模式。
    pub batch_queue: Vec<QueueItem>,
    /// 是否正在连续跑完整个队列（决定单文件完成后是弹窗还是自动续跑下一个）
    pub batch_running: bool,
    /// 当前正在转写的队列项下标；单文件模式下为 `None`
    pub batch_active: Option<usize>,

    // ── 模型下载（首次使用引导）──
    /// 是否正在下载模型
    pub is_downloading: bool,
    /// 当前正在下载的条目 id（用于在列表里高亮）
    pub download_current: Option<String>,
    /// 已完成字节 / 总字节（总字节为 0 表示服务端未给 Content-Length）
    pub download_done_bytes: u64,
    pub download_total_bytes: u64,
    /// 最近一次下载的结果提示（成功或失败原因）
    pub download_status_msg: String,
    /// 最近一次字幕写库失败的原因（`None` = 一切正常）。
    ///
    /// 存在的意义：写库失败此前被 `let _ =` 完全吞掉，用户改完字幕、切页面、
    /// 关程序，重启后发现改动全没了，而界面上从未出现任何异常。现在把它
    /// 渲染成可见提示条，让「没保存上」这件事至少是可被发现的。
    pub db_write_error: Option<String>,

    /// 「0 秒智能缓存命中」的查询结果缓存：(文件路径, 命中记录)。
    ///
    /// 渲染路径要读它（转写页卡片显示「已命中缓存 N 句」），而底层查询会把
    /// 整份字幕 JSON 反序列化出来（实测约 0.79 ms/次，1000 句字幕）。
    ///
    /// 缓存里存 `Arc<TaskRecord>` 而不是 `TaskRecord`：渲染每帧都会取出这个
    /// 结果，若按值返回就等于每帧把上千条 `Segment`（含多个 String）深拷贝一遍，
    /// 把「省下的反序列化」原样换成「一样重的内存拷贝」。`Arc` 克隆只碰引用计数。
    /// 命中用 `Option<Arc<TaskRecord>>`，`None` 表示「查过库、确实没有缓存」。
    ///
    /// 用 `Arc<Mutex<…>>` 而非普通字段：渲染只有 `&self` 无法回填，
    /// 而 `AppState` 本身是 `Clone` 的（`Mutex` 不实现 `Clone`）。
    pub cached_transcription: Arc<std::sync::Mutex<Option<(String, Option<Arc<TaskRecord>>)>>>,
    /// 已就位条目的缓存快照：`is_present` 要 stat 磁盘，
    /// 每帧对 11 个路径做 stat 会拖慢渲染，因此只在启动/下载完成后刷新。
    pub model_present: std::collections::HashMap<String, bool>,
}

impl AppState {
    /// 本机逻辑核心数。设置页滑条量程与线程数收敛共用同一来源，
    /// 避免 UI 显示的上限与实际生效值不一致。
    pub fn logical_cores() -> u32 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8) as u32
    }

    pub fn new(config: AppConfig, db: Database, pipeline: Arc<TaskPipeline>) -> Self {
        Self::with_hardware(config, db, pipeline, HardwareProfile::detect())
    }

    pub fn with_hardware(
        config: AppConfig,
        db: Database,
        pipeline: Arc<TaskPipeline>,
        hardware: HardwareProfile,
    ) -> Self {
        let recent_tasks = db.list_recent_tasks(50).unwrap_or_default();
        let lang = config.pipeline.language.clone();
        let fmt = config.pipeline.output_format.clone();
        let polish = config.pipeline.enable_polish;
        let polish_mode = PolishMode::from_str(&config.pipeline.polish_mode);
        let translate_mode = TranslateMode::from_str(&config.translate.mode);
        // 线程数按本机逻辑核心数收敛：既保证与设置页滑条量程一致，
        // 也避免把小于 8 的用户设置强行抬到 8 导致滑条取值无法持久化。
        let threads = config
            .pipeline
            .whisper_threads
            .clamp(2, Self::logical_cores().max(2));
        let whisper_model_tier = WhisperModelTier::from_model_path(&config.paths.whisper_model)
            .unwrap_or_default();
        // 并行进程数交给管线（0 = 自动，按核数推导）
        pipeline.set_parallel_workers(config.pipeline.parallel_workers as usize);
        let ffmpeg_path = AppConfig::resolve_path(&config.paths.ffmpeg);
        let proxy_manager = Arc::new(ProxyManager::new(&ffmpeg_path));
        let proxy_enabled = hardware.force_proxy;
        // GPU 占用策略按硬件能力归一：无 GPU 适配器时无论配置如何实际都是纯 CPU
        let gpu_mode = if hardware.use_gpu_pipeline() {
            config.gpu.mode().to_string()
        } else {
            "cpu".to_string()
        };
        let video_player = Arc::new(crate::engines::VideoPlayerEngine::with_policy(
            ffmpeg_path,
            hardware.decode_policy(),
            hardware.use_gpu_pipeline(),
        ));

        // 硬件检测与决策评估
        let hardware_info = crate::core::HardwareInfo::detect_with_media_profile(&hardware);
        let performance_level = hardware_info.evaluate_performance();
        let user_strategy = crate::core::UserStrategy::Balanced;
        let recommended_profile = crate::core::InferenceProfile::decide(
            performance_level,
            user_strategy,
            &hardware_info,
        );

        // 目标语言在 config 被 move 进 state 之前取出（否则借用已移动的值）
        let translate_target_lang = config.translate.target_lang.clone();
        let mut state = Self {
            config,
            db,
            pipeline,
            selected_file: None,
            transcribe_file: None,
            transcribe_duration: 0.0,
            status: ProcessStatus::Idle,
            segments: Vec::new(),
            recent_tasks,
            streaming_segments: Vec::new(),
            streaming_current_sec: 0.0,
            language: lang,
            output_format: fmt,
            enable_polish: polish,
            polish_mode,
            whisper_threads: threads,
            whisper_model_tier,
            cancel_requested: false,
            segments_dirty: false,
            active_task_id: None,
            segments_revision: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_text_coalesce: None,
            is_translating: false,
            translate_progress: 0.0,
            translate_status_msg: String::new(),
            translate_target_lang,
            translate_mode,
            metrics: ResourceMetrics::default(),

            active_tab: WorkspaceTab::Editor, // 默认主界面为剪辑校对工作台
            current_time: 0.0,
            total_duration: 0.0,
            is_playing: false,
            selected_segment_index: None,
            preview_frame_path: None,
            timeline_zoom: 1.0,
            editing_text: String::new(),
            frame_cache: Arc::new(FrameCache::new(100)), // 缓存 100 帧（约占 5-10MB）
            video_width: crate::engines::PLAYER_WIDTH,
            video_height: crate::engines::PLAYER_HEIGHT,
            dims_probed_for: None,
            video_player,
            proxy_manager,
            hardware,
            preview_source: None,
            proxy_enabled,
            proxy_busy: false,

            hardware_info,
            performance_level,
            user_strategy,
            recommended_profile,
            benchmark_result: None,
            is_benchmarking: false,
            show_perf_help: false,
            gpu_mode,
            waveform: None,
            waveform_busy_for: None,
            batch_queue: Vec::new(),
            batch_running: false,
            batch_active: None,
            is_downloading: false,
            download_current: None,
            download_done_bytes: 0,
            download_total_bytes: 0,
            download_status_msg: String::new(),
            // 首次扫描放在 `with_hardware` 收尾处（那里能拿到 &mut self）
            cached_transcription: Arc::new(std::sync::Mutex::new(None)),
            db_write_error: None,
            model_present: std::collections::HashMap::new(),
        };

        // 如果存在历史记录，启动时自动加载最近一次的工程，避免开屏黑屏或空数据
        if let Some(recent) = state.recent_tasks.first().cloned() {
            state.load_task(&recent);
        }

        // 首次扫描模型就位情况：界面据此在缺必需组件时弹下载引导
        state.refresh_model_presence();
        state
    }

    /// 切换用户策略并重新计算推荐配置
    pub fn set_user_strategy(&mut self, strategy: crate::core::UserStrategy) {
        self.user_strategy = strategy;
        self.recommended_profile = crate::core::InferenceProfile::decide(
            self.performance_level,
            self.user_strategy,
            &self.hardware_info,
        );
    }

    /// 重新检测硬件并重新评估
    pub fn refresh_hardware_detection(&mut self) {
        self.hardware_info = crate::core::HardwareInfo::detect();
        self.performance_level = self.hardware_info.evaluate_performance();
        self.recommended_profile = crate::core::InferenceProfile::decide(
            self.performance_level,
            self.user_strategy,
            &self.hardware_info,
        );
    }

    /// 将推荐配置应用至当前系统状态与 Pipeline 配置
    pub fn apply_recommended_profile(&mut self) {
        self.whisper_model_tier = self.recommended_profile.whisper_tier;
        self.whisper_threads = self.recommended_profile.whisper_threads;
        self.config.pipeline.whisper_threads = self.recommended_profile.whisper_threads;
        self.config.pipeline.enable_vad = self.recommended_profile.enable_vad;
        self.config.pipeline.llm_threads = self.recommended_profile.llm_threads;
        self.config.pipeline.whisper_processors = self.recommended_profile.max_concurrency;
        // 并行进程数回归「自动」：由引擎按本机核数推导，推荐档位不再干预这一项
        self.config.pipeline.parallel_workers = 0;
        self.pipeline.set_parallel_workers(0);
        self.pipeline.set_llm_threads(self.config.pipeline.llm_threads);

        let rel_path = self.recommended_profile.whisper_tier.model_relative_path();
        if AppConfig::resolve_path(&rel_path).exists() {
            self.config.paths.whisper_model = rel_path;
        }

        let _ = self.config.save_to_file("config.toml");
    }

    /// 切换深浅主题并落盘。
    ///
    /// 颜色 token 是「渲染时读全局开关」的纯函数，所以这里只需翻转开关 + 写
    /// config.toml，下一帧界面即整体换色，无需重建任何视图状态。
    pub fn toggle_theme(&mut self) {
        self.config.ui = self.config.ui.toggled();
        crate::ui::theme::Theme::set_light(self.config.ui.is_light());
        let _ = self.config.save_to_file("config.toml");
    }

    /// 设置页 / 主界面调整字幕样式后落盘，避免重启丢失
    pub fn save_subtitle_style(&self) {
        let _ = self.config.save_to_file("config.toml");
    }

    /// 应用字幕样式预设：同时套用该预设的整组排版参数并落盘
    pub fn apply_subtitle_preset(&mut self, preset: &str) {
        self.config.subtitle_style.apply_preset(preset);
        self.save_subtitle_style();
    }

    /// 切换翻译引擎档位并落盘。在线/离线是长期偏好，不落盘会导致
    /// 用户每次重启都要重选，还会与 config.toml 里手写的 mode 打架。
    pub fn set_translate_mode(&mut self, mode: TranslateMode) {
        self.translate_mode = mode;
        self.config.translate.mode = mode.as_str().to_string();
        self.save_translate_config();
    }

    /// 翻译相关配置（模式 / 接口 / 密钥 / 模型）落盘
    pub fn save_translate_config(&self) {
        let _ = self.config.save_to_file("config.toml");
    }

    /// 组装在线翻译引擎所需的连接参数（密钥回退环境变量、地址容错拼接）
    pub fn online_translate_config(&self) -> crate::engines::OnlineApiConfig {
        crate::engines::OnlineApiConfig {
            endpoint: self.config.translate.chat_completions_url(),
            api_key: self.config.translate.effective_api_key(),
            model: self.config.translate.api_model.trim().to_string(),
            batch_size: self.config.translate.batch_size,
            timeout_secs: self.config.translate.timeout_secs,
        }
    }

    /// 已有多少条字幕带译文（不论目标语言）。
    ///
    /// 用于**导出模式判定**（有译文就默认走双语导出）——那个场景不该关心译文是哪国语言。
    /// 界面上的「已完成 N/M 句」请用 [`AppState::translated_count_for`]，
    /// 否则切换目标语言后会把旧语言的译文也算成完成，显示 100% 却整篇是别的语言。
    pub fn translated_count(&self) -> usize {
        self.segments
            .iter()
            .filter(|seg| seg.has_translation())
            .count()
    }

    /// 当前目标语言下已完成的句数（界面进度与「是否还需要翻译」以它为准）
    pub fn translated_count_for(&self, target_lang: &str) -> usize {
        self.segments
            .iter()
            .filter(|seg| seg.translation_matches(target_lang))
            .count()
    }
    /// 把翻译结果**按句合并**回当前字幕表，而不是整表覆盖。
    ///
    /// # 为什么不能直接 `self.segments = translated`
    ///
    /// 翻译是在后台跑几分钟的任务，而字幕编辑器在翻译期间**仍然可编辑**（用户
    /// 可以一边等一边改错别字、调时间、拆合句）。引擎拿到的是**开始时**克隆的一份
    /// 字幕快照，若收尾时直接 `self.segments = translated` 覆盖，就等于把用户在这
    /// 几分钟里做的所有编辑**静默回滚**——改了半天，翻译一结束全没了。
    ///
    /// # 为什么键是「起始时间」而不是 `index`
    ///
    /// 拆句 / 合句 / 删句都会调用 [`AppState::reindex_segments`] 把序号整体重排。
    /// 若按 `index` 合并，重排后的句子会拿到**别人**的译文——静默张冠李戴，比丢译文
    /// 严重得多（用户看到的是「译文内容对不上原句」却毫无提示）。
    ///
    /// 起始时间在三种结构编辑下都稳定：拆句时前半段保留原 `start`、合句时保留首段
    /// `start`、删句时其余句子不动。只有用户手动微调时间才会变，那种情况下宁可**漏认**
    /// （不合并，留待下次补译），也绝不错误合并。
    ///
    /// 只搬**非空**译文，不碰用户可能已改动的原文 / 时间 / 说话人。
    pub fn merge_translations(&mut self, translated: Vec<Segment>) {
        use std::collections::HashMap;
        // 毫秒取整做键：快照与当前表里的 start 是**逐字节复制**的同一 f64，
        // 取整后必然相等；同时规避 -0.0 / 0.0 之类的位级差异。
        let key = |start: f64| -> i64 { (start * 1000.0).round() as i64 };
        let mut by_start: HashMap<i64, Segment> = HashMap::with_capacity(translated.len());
        for seg in translated {
            by_start.insert(key(seg.start), seg);
        }
        for seg in self.segments.iter_mut() {
            if let Some(src) = by_start.get(&key(seg.start)) {
                // 引擎解析失败时可能回填空串，用它覆盖用户已有的好译文会让
                // 「翻译一结束译文变空白」，故只认非空译文。
                if src.has_translation() {
                    seg.translation = src.translation.clone();
                    seg.translation_lang = src.translation_lang.clone();
                }
            }
        }
        self.bump_segments_revision();
    }


    /// 切换目标语言并落盘。目标语言是长期偏好，必须持久化，
    /// 否则重启后又跳回默认语言，用户会以为设置没生效。
    pub fn set_translate_target_lang(&mut self, lang: &str) {
        self.translate_target_lang = lang.to_string();
        self.config.translate.target_lang = lang.to_string();
        self.save_translate_config();
    }
    pub fn set_selected_file(&mut self, path: PathBuf) {
        self.selected_file = Some(path);
    }

    pub fn refresh_recent_tasks(&mut self) {
        if let Ok(tasks) = self.db.list_recent_tasks(50) {
            self.recent_tasks = tasks;
        }
    }

    /// 载入历史任务并无缝切换至剪辑工作台
    /// 载入历史工程。`switch_tab = true` 时切到剪辑校对页（用户主动点击入口）；
    /// 删除记录后的自动补位传 false，留在当前页面不打断浏览。
    pub fn load_task_with(&mut self, task: &TaskRecord, switch_tab: bool) {
        // 换工程前先落库当前工程的编辑
        self.flush_segments_if_dirty();
        // 历史库列表查询不装载字幕正文，这里按 id 补齐后再使用
        let task = if task.segments_loaded {
            task.clone()
        } else {
            self.db.hydrate_task(task)
        };
        let task = &task;
        self.selected_file = Some(PathBuf::from(&task.file_path));
        self.active_task_id = Some(task.id);
        self.status = ProcessStatus::Idle;
        self.segments = task.segments.clone();
        // 换了工程就是换了一份文档：撤销栈里存的还是上一个工程的片段快照，
        // 留着的话用户按 Ctrl+Z 会把上一个工程的字幕灌回当前工程，
        // `restore_snapshot` 收尾的 `flush_segments_if_dirty` 还会按当前工程的 id
        // 把它落库，直接把刚打开的工程覆盖掉。必须在这里清干净。
        self.reset_edit_history();
        self.bump_segments_revision();
        crate::subtitle::optimize_segments(&mut self.segments);
        let dur = if task.duration > 0.0 {
            task.duration
        } else {
            task.segments.last().map(|s| s.end).unwrap_or(0.0)
        };
        self.total_duration = dur;
        if let Some(first) = self.segments.first() {
            self.select_segment(first.index);
        } else {
            self.current_time = 0.0;
            self.selected_segment_index = None;
            self.editing_text.clear();
        }
        if switch_tab {
            self.active_tab = WorkspaceTab::Editor;
        }
        self.preview_source = self.selected_file.clone();
        self.proxy_busy = false;
        if self.proxy_enabled {
            if let Some(src) = self.selected_file.as_ref() {
                let h = self.proxy_manager.preview_height(src, !self.hardware.use_gpu_pipeline());
                if let Some(existing) = self.proxy_manager.existing_proxy(src, h) {
                    self.preview_source = Some(existing);
                }
            }
        }
    }

    /// 载入历史工程并切到剪辑校对页（用户主动点击入口的默认行为）
    pub fn load_task(&mut self, task: &TaskRecord) {
        self.load_task_with(task, true);
    }

    /// 删除历史任务记录（若当前工作区正显示该工程，则静默补位至下一条或彻底清空，不切换页面）
    pub fn delete_task_record(&mut self, id: i64) {
        let deleted_task = self.recent_tasks.iter().find(|t| t.id == id).cloned();
        let _ = self.db.delete_task(id);
        self.refresh_recent_tasks();

        if let Some(task) = deleted_task {
            let is_current = self.selected_file.as_ref().map(|p| {
                p == &PathBuf::from(&task.file_path)
                    || p.to_string_lossy().replace('\\', "/") == task.file_path.replace('\\', "/")
            }).unwrap_or(false);

            if is_current {
                if let Some(next_task) = self.recent_tasks.first().cloned() {
                    // 静默补位：只换数据，不把用户拽离视频库页面
                    self.load_task_with(&next_task, false);
                } else {
                    self.clear_current_workspace();
                }
            }
        }
    }

    /// 彻底清空当前工作区工程状态（重置为空闲初始状态）
    pub fn clear_current_workspace(&mut self) {
        // 清空前先落库当前工程的编辑
        self.flush_segments_if_dirty();
        self.selected_file = None;
        self.active_task_id = None;
        self.status = ProcessStatus::Idle;
        self.segments.clear();
        // 清空工作区后撤销栈里还是刚被清掉那份工程的快照，留着会被 Ctrl+Z 复活：
        // 字幕表凭空多出一个「没有对应工程」的片段列表。
        self.reset_edit_history();
        self.bump_segments_revision();
        self.current_time = 0.0;
        self.total_duration = 0.0;
        self.selected_segment_index = None;
        self.preview_frame_path = None;
        self.editing_text.clear();
        self.is_playing = false;
        self.preview_source = None;
        self.proxy_busy = false;
        self.video_width = crate::engines::PLAYER_WIDTH;
        self.video_height = crate::engines::PLAYER_HEIGHT;
        self.dims_probed_for = None;
        self.clear_waveform();
        self.clear_streaming();
    }

    /// 实时流式转写窗口保留的句数上限。
    ///
    /// 界面只展示**最近 3 句**（见 `transcribe.rs` 的流式看板），但历史上这里
    /// 是把每一句都 push 进 `streaming_segments` 且从不裁剪。转写一个 32 分钟
    /// 的课程视频会累积上千条 Segment（每条还带 String），而这些数据在转写
    /// 结束前一直驻留内存——纯粹为了一个「只显示 3 行」的看板。
    ///
    /// 留 32 句而不是 3 句，是为了给「回看刚过去几句」这类交互留余量，
    /// 同时把内存占用钉死成常数。
    const STREAMING_WINDOW: usize = 32;

    /// 追加实时流式转写片段并推进时间戳。
    ///
    /// 只保留最近 [`Self::STREAMING_WINDOW`] 句：看板只显示 3 句，长视频下
    /// 无限增长只是白占内存。
    ///
    /// 每次超出时挪掉队首若干条（通常只有 1 条）。单次代价是移动窗口内剩余
    /// 元素（≤32），也就是常数级；相比「转写 32 分钟累积上千条 Segment 常驻
    /// 内存」，这个代价可以忽略。
    pub fn push_stream_segment(&mut self, seg: Segment) {
        if seg.end > self.streaming_current_sec {
            self.streaming_current_sec = seg.end;
        }
        self.streaming_segments.push(seg);
        if self.streaming_segments.len() > Self::STREAMING_WINDOW {
            let excess = self.streaming_segments.len() - Self::STREAMING_WINDOW;
            self.streaming_segments.drain(..excess);
        }
    }

    /// 清理重置实时流式转写状态
    pub fn clear_streaming(&mut self) {
        self.streaming_segments.clear();
        self.streaming_current_sec = 0.0;
    }

    /// 检查当前待转写文件是否已存在本地已完成解析记录 (用于 0 秒智能缓存命中)。
    ///
    /// # 为什么结果必须缓存
    ///
    /// 这个函数要在**渲染路径**里被调用（转写页的「已就绪待处理」卡片要显示
    /// 「已命中本地缓存 N 句」）。而底层 `find_cached_task` 会把整份字幕 JSON
    /// **反序列化**出来——实测一条 1000 句的字幕约 **0.79 ms**，跟着
    /// `cx.notify()` 每帧跑一次就是纯浪费。
    ///
    /// 缓存失效靠 [`Self::invalidate_cached_transcription`]：只在
    /// 「换文件」和「转写完成/写库」这两个真正会改变结果的时刻调用。
    /// 键用文件路径——同一路径的缓存记录内容变了（重转写）会由后者覆盖。
    ///
    /// 返回 `Option<Arc<TaskRecord>>` 而非按值 `TaskRecord`：调用点在渲染循环里，
    /// 按值返回等于每帧深拷贝整份字幕（与省掉的反序列化同一量级）。`Arc` 克隆
    /// 只加一次引用计数。命中为 `Some(Arc)`，确认无缓存为 `None`。
    pub fn get_cached_transcription(&self) -> Option<Arc<TaskRecord>> {
        let file = self.transcribe_file.as_ref()?;
        let path_str = file.to_string_lossy();
        // 已有缓存且路径一致 → 直接复用（Arc 克隆，不深拷贝字幕）
        if let Ok(slot) = self.cached_transcription.lock() {
            if let Some((cached_path, hit)) = slot.as_ref() {
                if cached_path == path_str.as_ref() {
                    return hit.clone();
                }
            }
        }
        // 未命中：查一次库并**回填**（用 Mutex 而非 `&mut self`，
        // 因为渲染路径只能拿到 `&self`）。每换一个文件只会走一次。
        let hit = self.db.find_cached_task(&path_str).ok().flatten().map(Arc::new);
        if let Ok(mut slot) = self.cached_transcription.lock() {
            *slot = Some((path_str.to_string(), hit.clone()));
        }
        hit
    }

    /// 让「0 秒命中」的缓存结果失效（换文件、转写完成写库后调用）。
    ///
    /// 只在结果**真的可能变了**的时刻调用；渲染路径不调用，否则等于没缓存。
    pub fn invalidate_cached_transcription(&self) {
        if let Ok(mut slot) = self.cached_transcription.lock() {
            *slot = None;
        }
    }
    /// 0 秒智能缓存命中载入：瞬间恢复已缓存的完整字幕片段与各项指标，彻底跳过重复计算
    pub fn load_from_cache(&mut self, cached: TaskRecord) {
        self.flush_segments_if_dirty();
        let file_path = PathBuf::from(&cached.file_path);
        let total_dur = if cached.duration > 0.0 {
            cached.duration
        } else if self.transcribe_duration > 0.0 {
            self.transcribe_duration
        } else {
            cached.segments.last().map(|s| s.end).unwrap_or(0.0)
        };

        self.selected_file = Some(file_path.clone());
        self.active_task_id = Some(cached.id);
        self.preview_source = Some(file_path);
        self.segments = cached.segments;
        // 缓存命中同样是「换了一份文档」，撤销栈必须清空（同 `load_task_with`）：
        // 否则 Ctrl+Z 会把上一个工程的字幕灌进这份缓存工程并落库覆盖。
        self.reset_edit_history();
        self.bump_segments_revision();
        // 旧缓存任务载入时统一再优化一遍：长句自动拆分、鬼影剔除等规则对历史数据同样生效
        crate::subtitle::optimize_segments(&mut self.segments);
        if let Some(first) = self.segments.first() {
            self.select_segment(first.index);
        }
        self.total_duration = total_dur;

        self.status = ProcessStatus::Idle;
        self.transcribe_file = None;
        self.transcribe_duration = 0.0;
        self.clear_streaming();
    }

    pub fn preview_media(&self) -> Option<&PathBuf> {
        self.preview_source.as_ref().or(self.selected_file.as_ref())
    }

    /// 时间轴波形是否已对应当前媒体。
    /// 必须比对来源路径：换片后旧包络的时间刻度完全对不上，直接复用会画出错误波形。
    pub fn waveform_ready_for(&self, media: &std::path::Path) -> bool {
        self.waveform
            .as_ref()
            .map(|w| w.source == media)
            .unwrap_or(false)
    }

    /// 丢弃当前波形（清空工程 / 换片时调用，避免时间轴残留上一支视频的包络）
    pub fn clear_waveform(&mut self) {
        self.waveform = None;
        self.waveform_busy_for = None;
    }

    /// 把文件加入批量转写队列，返回实际新增的数量。
    ///
    /// 重复入队（同一路径已在队列里）会被跳过：用户常把整批文件拖两次，
    /// 不去重就会白跑一遍。路径按小写归一后比较，规避 Windows 盘符与大小写差异。
    pub fn enqueue_files(&mut self, paths: Vec<PathBuf>) -> usize {
        let mut existing: std::collections::HashSet<String> = self
            .batch_queue
            .iter()
            .map(|i| i.path.to_string_lossy().to_lowercase())
            .collect();
        let mut added = 0;
        for path in paths {
            if !existing.insert(path.to_string_lossy().to_lowercase()) {
                continue;
            }
            self.batch_queue.push(QueueItem::new(path));
            added += 1;
        }
        added
    }

    /// 下一个还需要处理的队列下标。
    ///
    /// 只看「等待中」：已成功的不重复烧算力，失败的也不能在这里被自动重挑，
    /// 否则单个坏文件会让批量续跑卡在原地反复重试。
    pub fn queue_next_actionable(&self) -> Option<usize> {
        next_pending_index(&self.batch_queue)
    }

    /// 队列里某个路径的下标。按小写归一比较，与入队去重保持同一套判等规则，
    /// 避免「同一个文件因为盘符大小写差异被当成两条」。
    pub fn queue_index_of(&self, path: &std::path::Path) -> Option<usize> {
        let key = path.to_string_lossy().to_lowercase();
        self.batch_queue
            .iter()
            .position(|i| i.path.to_string_lossy().to_lowercase() == key)
    }

    /// 把某条队列项标记为「转写中」，让队列卡片立刻有反馈
    pub fn mark_queue_running(&mut self, idx: usize) {
        if let Some(item) = self.batch_queue.get_mut(idx) {
            item.state = QueueState::Running;
        }
    }

    /// 从队列里移除一条（等待中或已完成的条目都能删）。
    /// 删除会让后续下标整体前移，因此活动指针直接作废，不做位移修正。
    pub fn remove_queue_item(&mut self, idx: usize) -> bool {
        if idx >= self.batch_queue.len() {
            return false;
        }
        self.batch_queue.remove(idx);
        self.batch_active = None;
        true
    }

    /// 队列里还没跑完的条目数（等待中 + 失败待重试）。
    /// 失败项也算「待处理」——用户点「开始全部」时它们会被重新排队。
    pub fn queue_pending_count(&self) -> usize {
        self.batch_queue
            .iter()
            .filter(|i| i.state.is_actionable())
            .count()
    }

    /// 结算当前正在处理的队列项（成功后 `batch_active` 归位，避免重复结算）
    pub fn finish_active_queue_item(&mut self, outcome: Result<usize, String>) {
        if let Some(idx) = self.batch_active.take() {
            if let Some(item) = self.batch_queue.get_mut(idx) {
                item.state = match outcome {
                    Ok(segments) => QueueState::Done { segments },
                    Err(reason) => QueueState::Failed(reason),
                };
            }
        }
    }

    /// 队列汇总：`(已完成数, 失败数, 累计字幕句数)`
    pub fn queue_summary(&self) -> (usize, usize, usize) {
        let mut done = 0;
        let mut failed = 0;
        let mut segments = 0;
        for item in &self.batch_queue {
            match item.state {
                QueueState::Done { segments: n } => {
                    done += 1;
                    segments += n;
                }
                QueueState::Failed(_) => failed += 1,
                _ => {}
            }
        }
        (done, failed, segments)
    }

    /// 队列累计时长（秒），用于批量完成弹窗
    pub fn queue_total_duration(&self) -> f64 {
        self.batch_queue.iter().map(|i| i.duration).sum()
    }

    /// 队列里失败条目的文件名，用于批量完成弹窗直接点名需要重试的对象
    pub fn queue_failed_names(&self) -> Vec<String> {
        self.batch_queue
            .iter()
            .filter(|i| matches!(i.state, QueueState::Failed(_)))
            .map(|i| i.name.clone())
            .collect()
    }

    /// 清空队列并退出批量模式（已转写完成、已落库的工程不受影响）
    pub fn clear_batch_queue(&mut self) {
        self.batch_queue.clear();
        self.batch_running = false;
        self.batch_active = None;
    }

    /// 视频画面宽高比（探测未完成或失败时回退 16:9）
    pub fn video_aspect(&self) -> f32 {
        if self.video_width > 0 && self.video_height > 0 {
            self.video_width as f32 / self.video_height as f32
        } else {
            16.0 / 9.0
        }
    }

    pub fn whisper_eta_label(&self) -> String {
        let dur = if self.transcribe_duration > 0.0 {
            self.transcribe_duration
        } else if self.total_duration > 0.0 {
            self.total_duration
        } else {
            0.0
        };
        if dur <= 0.0 {
            return "--".to_string();
        }
        let secs = self.whisper_model_tier.estimate_seconds(
            dur,
            self.whisper_threads,
            self.hardware.use_gpu_pipeline(),
            self.enable_polish,
        );
        WhisperModelTier::format_eta(secs)
    }

    pub fn should_use_proxy(&self) -> bool {
        self.proxy_enabled
    }

    /// 获取当前播放时间对应的有效字幕片段
    /// 半开区间 [start, end)：边界时刻 (t == end == 下一句 start) 归下一句，
    /// 避免选中某句时画面字幕仍显示上一句
    pub fn get_active_segment(&self) -> Option<&Segment> {
        let t = self.current_time;
        self.segments.iter().find(|seg| t >= seg.start && t < seg.end)
    }

    /// 选中指定索引的字幕片段
    pub fn select_segment(&mut self, index: usize) {
        // 切句前先把上一句的编辑落库（去抖收口点）
        self.flush_segments_if_dirty();
        // 换了一句就是另一段编辑会话，之前那句的文本合并到此为止
        self.undo_text_coalesce = None;
        self.selected_segment_index = Some(index);
        if let Some(seg) = self.segments.iter().find(|s| s.index == index) {
            self.editing_text = seg.display_text().to_string();
            self.current_time = seg.start;
        }
    }

    /// 跳转播放指针时间
    pub fn seek_to(&mut self, time_sec: f64) {
        self.flush_segments_if_dirty();
        let max_dur = if self.total_duration > 0.0 {
            self.total_duration
        } else {
            self.segments.last().map(|s| s.end).unwrap_or(3600.0)
        };
        self.current_time = time_sec.clamp(0.0, max_dur);

        // 如果跳转到的时间落在某个字幕内，且当前没有选或者选的不同，自动联动
        if let Some(seg) = self.get_active_segment() {
            let seg_idx = seg.index;
            if self.selected_segment_index != Some(seg_idx) {
                self.selected_segment_index = Some(seg_idx);
                if let Some(s) = self.segments.iter().find(|s| s.index == seg_idx) {
                    self.editing_text = s.display_text().to_string();
                }
            }
        }
    }

    /// 保存当前选中字幕片段的修改文本（仅内存，去抖后统一落库）
    pub fn save_selected_text(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let new_text = self.editing_text.trim().to_string();
        // 文本编辑是逐键触发的，若每敲一个字都压一层撤销栈，用户按一次 Ctrl+Z
        // 只会退掉一个字符，栈也会被一次输入撑满。所以同一句的连续输入合并成
        // 一层：只有「换了一句」或「中间夹了别的编辑」时才另起一层。
        self.snapshot_for_text_edit(idx);
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            seg.text = new_text.clone();
            if !seg.polished.is_empty() {
                seg.polished = new_text;
            }
        }
        // 去抖：编辑逐键触发，若每次都全量序列化全部片段写 SQLite，长视频输入时是隐形 I/O 热点。
        // 仅标记脏，等切句/跳转/播放/导出等天然节点由 flush_segments_if_dirty 统一写回。
        self.segments_dirty = true;
        self.bump_segments_revision();
    }

    /// 将编辑产生的脏字幕落库（幂等，未脏时零开销）
    pub fn flush_segments_if_dirty(&mut self) {
        if self.segments_dirty {
            self.segments_dirty = false;
            // 写库失败不能静默：用户以为改动已保存，重启后却发现全丢了。
            // 记进 `db_write_error`，界面把它渲染成可见的提示条。
            match self.sync_segments_to_db() {
                Ok(()) => self.db_write_error = None,
                Err(err) => {
                    tracing::warn!(error = %err, "字幕写库失败，改动尚未持久化");
                    self.db_write_error = Some(format!("字幕保存失败：{err}"));
                }
            }
        }
    }

    /// 标记字幕内容已变化（驱动清单搜索缓存失效）
    pub fn bump_segments_revision(&mut self) {
        self.segments_revision = self.segments_revision.wrapping_add(1);
    }

    /// 撤销栈深度上限。按千行字幕、每段约 200 字节估，50 层也只有几 MB，
    /// 换来的是「一路点错也能退回去」，比为了省内存把深度压到个位数实用得多。
    const MAX_UNDO_DEPTH: usize = 50;

    fn current_snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            segments: self.segments.clone(),
            selected_segment_index: self.selected_segment_index,
            current_time: self.current_time,
            editing_text: self.editing_text.clone(),
        }
    }

    /// 清空撤销/重做历史与文本合并标记。
    ///
    /// **凡是整体换掉 `segments`（换工程、缓存命中、清空工作区、新一轮转写覆盖）的地方都必须调用。**
    /// 撤销栈里装的是某个工程的片段快照，换文档后若不清理，用户按一次 Ctrl+Z 就会把
    /// 上一个工程的整份字幕灌回当前文档；`restore_snapshot` 收尾的 `flush_segments_if_dirty`
    /// 还会按当前工程的 id 写库，等于用旧工程覆盖掉新工程的历史记录。
    pub fn reset_edit_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.undo_text_coalesce = None;
    }

    /// 在执行一次编辑前记录快照。必须由**所有**编辑入口调用，
    /// 漏掉一处的后果是那类操作撤销时会连同前一次编辑一起退掉。
    pub fn snapshot_for_undo(&mut self) {
        let snap = self.current_snapshot();
        self.undo_stack.push(snap);
        if self.undo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        // 非文本编辑打断文本合并：否则「改字 → 拆分 → 继续改字」会被并成一层，
        // 一次 Ctrl+Z 把拆分和两次改字一起退掉。
        self.undo_text_coalesce = None;
        // 新的编辑分支一旦产生，原先的重做链就失效了（与所有编辑器的行为一致）
        self.redo_stack.clear();
    }

    /// 文本编辑专用的快照入口：同一句的连续输入复用同一层撤销记录。
    fn snapshot_for_text_edit(&mut self, idx: usize) {
        if self.undo_text_coalesce == Some(idx) {
            return;
        }
        self.snapshot_for_undo();
        self.undo_text_coalesce = Some(idx);
    }

    /// 是否有可撤销的编辑（UI 据此决定撤销按钮是否可点）
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// 是否有可重做的编辑
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// 还原一份快照，并把界面依赖的状态一并刷新。
    ///
    /// 还原后必须递增 `segments_revision`：清单的搜索下标是按这个号缓存的，
    /// 不递增的话字幕表虽然换了，列表还会继续用旧下标渲染（表现为空白或错行）。
    fn restore_snapshot(&mut self, snap: EditSnapshot) {
        self.segments = snap.segments;
        self.selected_segment_index = snap.selected_segment_index;
        self.current_time = snap.current_time;
        self.editing_text = snap.editing_text;
        self.segments_dirty = true;
        // 还原之后若还留着「正在合并的文本编辑」标记，用户接着打字会被并进
        // 已经回退掉的那一层，撤销栈顺序就乱了。
        self.undo_text_coalesce = None;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
    }

    /// 撤销最近一次字幕编辑，成功返回 `true`
    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo_stack.pop() else { return false; };
        let current = self.current_snapshot();
        self.redo_stack.push(current);
        if self.redo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.redo_stack.remove(0);
        }
        self.restore_snapshot(snap);
        true
    }

    /// 重做最近一次被撤销的编辑，成功返回 `true`
    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo_stack.pop() else { return false; };
        let current = self.current_snapshot();
        self.undo_stack.push(current);
        if self.undo_stack.len() > Self::MAX_UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.restore_snapshot(snap);
        true
    }

    /// 微调选中字幕片段的起止时间
    ///
    /// 单纯加减时间会制造两类坏数据：区间交叉（后一句起点跑到前一句里面），
    /// 以及被压到近乎零长度。前者让「按播放时间取句」取错行，后者更狠——
    /// 下次载入工程时 [`crate::subtitle::optimize_segments`] 会把时长过短、
    /// 且与后句几乎同时开始的片段当成幻读鬼影**直接删掉**，用户看到的就是
    /// 「点几下微调，字幕没了，怎么改都回不来」。所以这里统一交给
    /// [`plan_time_edit`] 规划，宁可连带动一下邻居的边界，也不留下非法区间。
    pub fn adjust_selected_times(&mut self, delta_start: f64, delta_end: f64) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };

        let cur = self.segments[pos].clone();
        let prev = pos
            .checked_sub(1)
            .map(|p| (self.segments[p].start, self.segments[p].end));
        let next = self.segments.get(pos + 1).map(|s| (s.start, s.end));
        let edit = plan_time_edit(cur.start, cur.end, prev, next, delta_start, delta_end);

        self.snapshot_for_undo();

        self.segments[pos].start = edit.start;
        self.segments[pos].end = edit.end;
        // 邻居只有在被本次微调越界时才跟着让位，没越界时 `plan_time_edit`
        // 返回 `None`，这里也就不会平白改动别人的时间。
        if let Some(prev_end) = edit.prev_end {
            if let Some(p) = pos.checked_sub(1) {
                self.segments[p].end = prev_end;
            }
        }
        if let Some(next_start) = edit.next_start {
            if pos + 1 < self.segments.len() {
                self.segments[pos + 1].start = next_start;
            }
        }

        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
    }

    /// 拆分当前选中的字幕片段为前后两段。
    ///
    /// `split_at` 为按字符计的切分位置（来自编辑框光标）；`None` 时取文本正中。
    /// 切分时间点按字数的比例落在原时间区间内，而不是永远取中点，
    /// 否则在长句末尾断句时后半句会被拉到明显错误的时间位置。
    pub fn split_selected_segment(&mut self, split_at: Option<usize>) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        let orig = self.segments[pos].clone();

        let cur_text = orig.display_text().to_string();
        let char_count = cur_text.chars().count();
        if char_count <= 1 {
            return;
        }
        let split_pos = split_at
            .unwrap_or(char_count / 2)
            .clamp(1, char_count - 1);
        let ratio = split_pos as f64 / char_count as f64;
        // 时间切点跟着文字比例走，但必须保证拆出来的两半都够长：时长短于
        // [`MIN_EDIT_DUR`] 且与邻句起点相近的片段会在下次载入时被 `optimize_segments`
        // 当幻读鬼影删掉，用户看到的就是「拆完再打开，半句话凭空没了」。
        // 原文总跨度不足两倍下限时无法拆成两段可读字幕，直接放弃这次拆分。
        let span = orig.end - orig.start;
        if span < 2.0 * MIN_EDIT_DUR {
            return;
        }
        let raw_split = orig.start + span * ratio;
        let split_time = raw_split.clamp(orig.start + MIN_EDIT_DUR, orig.end - MIN_EDIT_DUR);
        let part1: String = cur_text.chars().take(split_pos).collect();
        let part2: String = cur_text.chars().skip(split_pos).collect();

        self.snapshot_for_undo();
        self.segments[pos].end = split_time;
        if !self.segments[pos].polished.is_empty() {
            self.segments[pos].polished = part1;
        } else {
            self.segments[pos].text = part1;
        }

        let new_seg = Segment {
            index: orig.index + 1,
            start: split_time,
            end: orig.end,
            text: part2.clone(),
            translation: orig.translation.clone(),
            translation_lang: orig.translation_lang.clone(),
            polished: if !orig.polished.is_empty() { part2 } else { String::new() },
            language: orig.language.clone(),
            confidence: orig.confidence,
            speaker: orig.speaker,
        };
        self.segments.insert(pos + 1, new_seg);

        // 重新规范化所有序号
        self.reindex_segments();
        self.bump_segments_revision();
        self.select_segment(idx + 1);
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    /// 将当前选中字幕与下一段字幕合并
    pub fn merge_selected_with_next(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        if pos + 1 >= self.segments.len() { return; }

        self.snapshot_for_undo();
        let next = self.segments.remove(pos + 1);
        let cur = &mut self.segments[pos];
        cur.end = next.end;
        let combined = format!("{}{}", cur.display_text(), next.display_text());
        if !cur.polished.is_empty() || !next.polished.is_empty() {
            cur.polished = combined;
        } else {
            cur.text = combined;
        }

        self.reindex_segments();
        self.bump_segments_revision();
        self.select_segment(idx);
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    /// 删除当前选中的字幕片段
    pub fn delete_selected_segment(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        self.snapshot_for_undo();
        self.segments.remove(pos);

        self.reindex_segments();
        self.bump_segments_revision();
        if !self.segments.is_empty() {
            let next_idx = self.segments[pos.min(self.segments.len() - 1)].index;
            self.select_segment(next_idx);
        } else {
            self.selected_segment_index = None;
            self.editing_text.clear();
        }
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    fn reindex_segments(&mut self) {
        for (i, seg) in self.segments.iter_mut().enumerate() {
            seg.index = i + 1;
        }
    }

    /// 同步当前字幕到 SQLite 数据库。
    ///
    /// # 为什么返回 `Result` 而不是吞掉
    ///
    /// 此前这里是 `let _ = ...`，写库失败被完全静默。后果是**用户以为保存了**：
    /// 在剪辑台改了半天字幕、切走页面（触发 flush）、关掉程序，重启后发现改动
    /// 全没了——而界面上从未出现任何异常。磁盘满、数据库被锁、文件只读都会走到
    /// 这条路径。
    ///
    /// 现在把失败交给调用方（`flush_segments_if_dirty` 会把它挂到界面的提示条上）。
    /// 注意：**不重试**。写失败通常是持久性原因（磁盘满 / 只读），立刻重试只会
    /// 在每次 flush 时再失败一次；提示用户才是有效的处置。
    pub fn sync_segments_to_db(&self) -> anyhow::Result<()> {
        // 按当前工程的数据库主键写回。没有主键（尚未落库）时什么都不做：
        // 退化成按路径更新会把同路径的历史记录一起改掉。
        match self.active_task_id {
            Some(id) => self.db.update_task_segments(id, &self.segments),
            None => Ok(()),
        }
    }

    // ─────────── 模型下载（首次使用引导）───────────

    /// 重新扫描全部可下载项的就位状态（启动时与每次下载完成后调用）。
    ///
    /// 结果缓存在 `model_present`：判定要 `stat` 磁盘，而渲染每帧都问，
    /// 11 个路径逐帧 stat 是白白的系统调用开销。
    pub fn refresh_model_presence(&mut self) {
        // 直接用**内存里的** config：AppState 自己就持有它，判定时再
        // `load_from_file` 一次纯属浪费（启动路径上会白读一遍 TOML + 解析）。
        // 顺带也解决了「11 个条目各读一遍」——统一用这一份配置。
        let ctx = crate::utils::model_download::PresenceContextRef::new(&self.config);
        self.model_present = crate::utils::ITEMS
            .iter()
            .map(|i| (i.id.to_string(), ctx.is_present(i)))
            .collect();
    }

    /// 某个条目是否已就位（查缓存快照，不碰磁盘）。
    pub fn model_is_present(&self, id: &str) -> bool {
        self.model_present.get(id).copied().unwrap_or(false)
    }

    /// 缺失的**必需**组件数（ffmpeg 与 Whisper 主模型）。
    /// 界面据此决定是否弹首次使用引导。
    pub fn missing_required_models(&self) -> usize {
        crate::utils::ITEMS
            .iter()
            .filter(|i| i.required && !self.model_is_present(i.id))
            .count()
    }

    /// 缺失条目总数（含可选）。
    pub fn missing_model_count(&self) -> usize {
        crate::utils::ITEMS
            .iter()
            .filter(|i| !self.model_is_present(i.id))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, WhisperModelTier};
    use crate::subtitle::Segment;

    /// 构造一个最小可用、不碰显卡/磁盘/子进程的 AppState：内存 SQLite + 假路径引擎，
    /// 只为验证「编辑 / 撤销」这类纯状态逻辑。
    ///
    /// 用 `HardwareProfile::cpu_only()` 而不是 `AppState::new`：后者会通过 wgpu 枚举
    /// 显卡，单测没必要为了一份硬件档案去初始化图形栈。
    fn test_state(segments: Vec<Segment>) -> AppState {
        use crate::engines::{FFmpegEngine, LLMEngine, WhisperEngine};
        use crate::storage::Database;
        use crate::utils::AppConfig;
        use std::sync::Arc;

        let db = Database::open(":memory:").expect("内存数据库应能打开");
        let pipeline = Arc::new(crate::core::TaskPipeline::new(
            Arc::new(FFmpegEngine::new("ffmpeg")),
            Arc::new(WhisperEngine::new("whisper-cli", "ggml-small.bin", 4, 1)),
            None,
            Arc::new(LLMEngine::new("llama-cli", "model.gguf", 2048, 4)),
            None,
        ));
        let mut state = AppState::with_hardware(
            AppConfig::default(),
            db,
            pipeline,
            crate::engines::HardwareProfile::cpu_only(),
        );
        state.segments = segments;
        state
    }

    #[test]
    fn model_tier_follows_configured_path() {
        assert_eq!(WhisperModelTier::default(), WhisperModelTier::Balanced);
        assert_eq!(
            WhisperModelTier::from_model_path("models/whisper/ggml-small.bin"),
            Some(WhisperModelTier::Balanced)
        );
        assert_eq!(
            WhisperModelTier::from_model_path("models/whisper/ggml-small-q5_0.bin"),
            Some(WhisperModelTier::Balanced)
        );
        assert_eq!(
            WhisperModelTier::from_model_path("models\\whisper\\ggml-large-v3-turbo-q5_0.bin"),
            Some(WhisperModelTier::TurboSpeed)
        );
        assert_eq!(
            WhisperModelTier::from_model_path("models/sensevoice/model.int8.onnx"),
            Some(WhisperModelTier::SenseVoice)
        );
    }

    #[test]
    fn more_threads_shortens_cpu_eta() {
        let d = 32.0 * 60.0;
        let t4 = WhisperModelTier::Balanced.estimate_seconds(d, 4, false, false);
        let t8 = WhisperModelTier::Balanced.estimate_seconds(d, 8, false, false);
        let t16 = WhisperModelTier::Balanced.estimate_seconds(d, 16, false, false);
        assert!(t4 > t8, "4 threads ({t4}) should be slower than 8 ({t8})");
        assert!(t8 > t16, "8 threads ({t8}) should be slower than 16 ({t16})");
        assert_ne!(
            WhisperModelTier::format_eta(t4),
            WhisperModelTier::format_eta(t16)
        );
    }

    #[test]
    fn gpu_eta_is_faster_than_cpu() {
        let d = 32.0 * 60.0;
        let cpu = WhisperModelTier::Precise.estimate_seconds(d, 8, false, false);
        let gpu = WhisperModelTier::Precise.estimate_seconds(d, 8, true, false);
        assert!(gpu < cpu * 0.5);
    }

    /// 失败项不能被续跑逻辑自动重挑，否则一个坏文件会让整批卡在死循环里。
    #[test]
    fn next_pending_skips_done_and_failed_items() {
        use super::{next_pending_index, QueueItem, QueueState};
        use std::path::PathBuf;

        let mut queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
        ];
        queue[0].state = QueueState::Done { segments: 12 };
        queue[1].state = QueueState::Failed("坏文件".to_string());

        assert_eq!(next_pending_index(&queue), Some(2));

        queue[2].state = QueueState::Running;
        assert_eq!(next_pending_index(&queue), None);

        // 只有「开始全部」显式重新排队之后，失败项才重新可跑
        queue[1].state = QueueState::Pending;
        assert_eq!(next_pending_index(&queue), Some(1));
    }

    /// 失败项在「待处理」计数里要算数（用户点「开始全部」时它们会被重试），
    /// 但不等于「可续跑」——两者用不同判定，别混。
    #[test]
    fn failed_items_count_as_pending_but_are_not_runnable() {
        use super::QueueState;

        let failed = QueueState::Failed("超时".to_string());
        assert!(failed.is_actionable());
        assert!(failed.is_failed());
        assert!(!failed.is_pending());

        let done = QueueState::Done { segments: 3 };
        assert!(!done.is_actionable());
        assert!(!done.is_pending());
        assert!(!done.is_failed());

        assert!(QueueState::Pending.is_pending());
    }

    /// 反复点「起点 +0.5」不能把当前句推过下一句：交叉区间会被
    /// `optimize_segments` 在下次载入时按「排序 + 截断重叠 + 剔鬼影」重写甚至删句，
    /// 用户看到的就是「字幕没了，怎么改都回不来」。
    #[test]
    fn adjust_times_keeps_timeline_valid_and_survives_optimize() {
        use crate::subtitle::{optimize_segments, MIN_SEGMENT_DUR};

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
            Segment::new(3, 4.0, 6.0, "第三句"),
        ]);
        state.selected_segment_index = Some(1);
        for _ in 0..8 {
            state.adjust_selected_times(0.5, 0.0);
        }

        // (a) 时间轴仍然单调不重叠
        for w in state.segments.windows(2) {
            assert!(
                w[0].end <= w[1].start + 1e-9,
                "第 {} 段与下一段交叉：{:?}",
                w[0].index,
                (w[0].end, w[1].start)
            );
        }
        // (b) 没有片段的时长小于最短时长
        for seg in &state.segments {
            assert!(
                seg.duration() >= MIN_SEGMENT_DUR - 1e-9,
                "第 {} 段时长过短：{}",
                seg.index,
                seg.duration()
            );
        }
        // (c) 对调完的结果跑一遍载入时的规范化，片段数量不变（没有被当鬼影删掉）
        let before = state.segments.len();
        let mut after = state.segments.clone();
        optimize_segments(&mut after);
        assert_eq!(
            after.len(),
            before,
            "用户手工调过的时间被 optimize_segments 当鬼影删掉了"
        );
    }

    /// 撤销/重做：快照在编辑之前压栈，撤销回到编辑前，重做回到编辑后。
    #[test]
    fn undo_and_redo_restore_edit_state() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.selected_segment_index = Some(2);

        let before: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
        state.adjust_selected_times(0.0, 0.5);
        let after: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
        assert_ne!(before, after, "微调应当真的改动了时间");

        assert!(state.can_undo(), "编辑之后应当有可撤销的步骤");
        assert!(state.undo(), "撤销应当成功");
        let undone: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(undone, before, "撤销后应回到编辑前的时间");

        assert!(state.can_redo(), "撤销之后应当有可重做的步骤");
        assert!(state.redo(), "重做应当成功");
        let redone: Vec<(f64, f64)> = state.segments.iter().map(|s| (s.start, s.end)).collect();
        assert_eq!(redone, after, "重做后应回到编辑后的时间");
    }

    /// 删除也要能撤销（快照在 `remove` 之前压栈），否则用户删错一句只能重跑转写。
    #[test]
    fn undo_restores_deleted_segment() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.selected_segment_index = Some(1);
        state.delete_selected_segment();
        assert_eq!(state.segments.len(), 1);

        assert!(state.undo());
        assert_eq!(state.segments.len(), 2, "撤销删除应把被删片段放回去");
        assert_eq!(state.segments[1].text, "第二句");
    }

    /// 换工程必须清空撤销栈。修前：在工程 A 里做一次编辑，再打开工程 B，
    /// 按 Ctrl+Z 会把 A 的整份字幕灌回当前文档；`restore_snapshot` 收尾的
    /// `flush_segments_if_dirty` 还会按 B 的 id 落库，等于用 A 覆盖掉 B 的历史记录。
    #[test]
    fn switching_projects_clears_undo_history() {
        use crate::storage::TaskRecord;

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "甲工程第一句"),
            Segment::new(2, 2.0, 4.0, "甲工程第二句"),
        ]);
        state.selected_segment_index = Some(1);
        state.adjust_selected_times(0.0, 0.5);
        assert!(state.can_undo(), "前置条件：甲工程里应当攒下了一次可撤销的编辑");

        let other = TaskRecord {
            id: 2,
            file_path: "D:/video/b.mp4".to_string(),
            file_name: "b.mp4".to_string(),
            duration: 4.0,
            status: "completed".to_string(),
            segments: vec![Segment::new(1, 0.0, 2.0, "乙工程第一句")],
            segment_count: 1,
            sample_text: "乙工程第一句".to_string(),
            segments_loaded: true,
            created_at: String::new(),
            metrics: None,
        };
        state.load_task_with(&other, true);

        assert_eq!(state.segments.len(), 1);
        assert_eq!(state.segments[0].text, "乙工程第一句");
        assert!(!state.can_undo(), "换工程后不应还留着上一个工程的撤销点");
        assert!(!state.can_redo(), "重做栈同样必须清空");
        assert!(!state.undo(), "撤销应当无事可做");
        assert_eq!(
            state.segments[0].text, "乙工程第一句",
            "撤销把上一个工程的字幕灌回了当前工程"
        );
    }

    /// 清空工作区也要清撤销栈：否则 Ctrl+Z 会让字幕表凭空复活一份
    /// 「没有对应工程」的片段列表。
    #[test]
    fn clearing_workspace_clears_undo_history() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "某工程第一句")]);
        state.selected_segment_index = Some(1);
        state.editing_text = "改过的句子".to_string();
        state.save_selected_text();
        assert!(state.can_undo(), "前置条件：应当攒下了一次可撤销的文本编辑");

        state.clear_current_workspace();
        assert!(state.segments.is_empty());
        assert!(!state.can_undo(), "清空工作区后不应还有可撤销的步骤");
        assert!(!state.undo());
        assert!(
            state.segments.is_empty(),
            "撤销把已经清空的工程片段复活了"
        );
    }

    /// 手工把一句拆成前后两段时，时间切点也必须有下限：否则短的那半会落进
    /// `optimize_segments` 的幻读鬼影判据（时长 < 0.25s 且与后句几乎同时开始），
    /// 下次载入工程时被直接删掉——用户看到的是「拆分之后重新打开，半句话没了，
    /// 怎么改都回不来」。
    #[test]
    fn split_segment_halves_survive_reload() {
        use crate::subtitle::{optimize_segments, MIN_EDIT_DUR};

        // 15 字 / 2.0 秒：光标停在第 1 个字上拆分，按字数比例算出的切点约 0.13s，
        // 短于下限，旧实现会被当鬼影删掉。
        let mut state = test_state(vec![Segment::new(
            1,
            0.0,
            2.0,
            "前半句内容，后半句内容在这里呢",
        )]);
        state.selected_segment_index = Some(1);
        let before_len: usize = state.segments.iter().map(|s| s.display_text().chars().count()).sum();
        state.split_selected_segment(Some(1));
        assert_eq!(state.segments.len(), 2, "应当拆成两段");
        assert_eq!(
            state.segments.iter().map(|s| s.display_text().chars().count()).sum::<usize>(),
            before_len,
            "拆分不该丢字"
        );
        for w in state.segments.windows(2) {
            assert!(
                w[0].duration() >= MIN_EDIT_DUR - 1e-9,
                "拆出的前半段过短：{:?}",
                w[0]
            );
        }

        // 模拟下次载入工程时的规范化：两段都必须存活
        let mut after = state.segments.clone();
        optimize_segments(&mut after);
        assert_eq!(after.len(), 2, "拆出的片段被 optimize_segments 当鬼影删掉了");
        let joined: String = after.iter().map(|s| s.display_text().to_string()).collect();
        assert!(
            joined.contains('前') && joined.contains('后'),
            "复原后内容丢失：{joined}"
        );
    }

    /// 清单过滤下标必须始终落在片段表内：虚拟列表取不到行就量不出行高
    /// （`item_height = 0`），整片清单会塌成空白，而且缓存键不变就一直空着——
    /// 这正是用户说的「字幕没了」。
    #[test]
    fn filter_indices_cover_segments_after_edit() {
        use crate::subtitle::{indices_cover_segments, matched_indices};

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
            Segment::new(3, 4.0, 6.0, "第三句"),
        ]);
        // 编辑前缓存下来的「全部片段」下标，删掉一段之后必然越界
        let stale = matched_indices(&state.segments, "");
        assert!(indices_cover_segments(&stale, state.segments.len()));

        state.selected_segment_index = Some(2);
        state.delete_selected_segment();
        assert!(
            !indices_cover_segments(&stale, state.segments.len()),
            "旧下标应当已越界——所以刷新时必须重新校验，不能只认缓存键"
        );

        // 重算之后必须重新覆盖全部片段
        let fresh = matched_indices(&state.segments, "");
        assert!(indices_cover_segments(&fresh, state.segments.len()));
        assert_eq!(fresh.len(), state.segments.len());

        // 过滤态（有关键字）只要求不越界
        let hit = matched_indices(&state.segments, "第三句");
        assert!(indices_cover_segments(&hit, state.segments.len()));
    }
    /// 流式转写窗口必须有界：长视频会 push 上千句，而看板只显示 3 句。
    /// 这里锁住「内存占用是常数」这个不变量，防止将来有人把裁剪去掉。
    #[test]
    fn streaming_window_stays_bounded() {
        let mut state = test_state(Vec::new());
        for i in 1..=500 {
            state.push_stream_segment(Segment::new(i, (i as f64) * 2.0, (i as f64) * 2.0 + 1.0, "句子"));
        }
        assert!(
            state.streaming_segments.len() <= AppState::STREAMING_WINDOW,
            "窗口必须钉死在上限内，实际 {}",
            state.streaming_segments.len()
        );
        // 保留的是**最近**的若干句，不是最早的
        let last = state.streaming_segments.last().expect("应有内容");
        assert_eq!(last.index, 500, "窗口里应保留最后 push 的那句");
        // 时间戳仍应推进到最新（进度条依赖它）
        assert!((state.streaming_current_sec - 1001.0).abs() < 0.001);
    }

    /// 窗口内句序必须保持 push 顺序（看板按顺序展示最近 3 句）。
    #[test]
    fn streaming_window_keeps_order() {
        let mut state = test_state(Vec::new());
        for i in 1..=100 {
            state.push_stream_segment(Segment::new(i, i as f64, i as f64 + 0.5, "x"));
        }
        let idxs: Vec<usize> = state.streaming_segments.iter().map(|s| s.index).collect();
        let mut sorted = idxs.clone();
        sorted.sort_unstable();
        assert_eq!(idxs, sorted, "窗口内句序应保持递增");
        assert_eq!(*idxs.last().unwrap(), 100);
    }

    /// 清理必须把窗口与计时都归零（换任务时复用同一个 AppState）。
    #[test]
    fn clear_streaming_resets_window() {
        let mut state = test_state(Vec::new());
        for i in 1..=10 {
            state.push_stream_segment(Segment::new(i, i as f64, i as f64 + 0.5, "x"));
        }
        state.clear_streaming();
        assert!(state.streaming_segments.is_empty());
        assert_eq!(state.streaming_current_sec, 0.0);
    }

    /// 渲染路径每帧都会调 `get_cached_transcription()`，而底层 `find_cached_task`
    /// 会把整份字幕 JSON 反序列化出来（实测约 0.79 ms/次）。这个测试锁死缓存语义：
    /// 同一路径在**未显式失效**前不会重新查库——证据是库里中途新插了一条同路径记录，
    /// 但返回值仍是首次那份「无命中」，直到 `invalidate_cached_transcription` 才更新。
    #[test]
    fn cached_transcription_reuses_result_until_invalidated() {
        use crate::subtitle::Segment;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));

        // 首次调用：库里还没有 → 返回 None 并回填缓存
        assert!(state.get_cached_transcription().is_none());

        // 写库一条同路径的已完成记录（模拟后台刚转写完）
        let id = state
            .db
            .insert_task(
                "D:/video/a.mp4",
                "a.mp4",
                4.0,
                "completed",
                &[Segment::new(1, 0.0, 2.0, "第一句")],
                None,
            )
            .expect("插入应成功");

        // 缓存已回填「无命中」，未失效前不应重新查库
        assert!(
            state.get_cached_transcription().is_none(),
            "同一路径在失效前不应重新查库（否则渲染路径每帧都反序列化字幕）"
        );

        // 显式失效后才应看到新记录
        state.invalidate_cached_transcription();
        let hit = state.get_cached_transcription().expect("失效后应能查到");
        assert_eq!(hit.id, id);
        assert_eq!(hit.segments.len(), 1);

        // 再次调用走缓存，返回同一份
        assert_eq!(state.get_cached_transcription().unwrap().id, id);
    }

    /// 换文件（`transcribe_file` 换成另一个路径）必须立刻反映到结果，
    /// 不能把上一个文件「无缓存」的结论错误地套到新文件上。
    #[test]
    fn cached_transcription_follows_file_change() {
        use crate::subtitle::Segment;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());

        state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));
        assert!(state.get_cached_transcription().is_none());

        let id = state
            .db
            .insert_task(
                "D:/video/b.mp4",
                "b.mp4",
                6.0,
                "completed",
                &[Segment::new(1, 0.0, 3.0, "B 文件第一句")],
                None,
            )
            .expect("插入应成功");

        // 换成 b.mp4：路径不一致，缓存必须重新查库
        state.transcribe_file = Some(PathBuf::from("D:/video/b.mp4"));
        let hit = state
            .get_cached_transcription()
            .expect("换文件后应查到 B 的缓存");
        assert_eq!(hit.id, id);
        assert_eq!(hit.segments[0].text, "B 文件第一句");
    }

    /// 缓存命中必须返回**同一个 Arc**（指针相等），而不是每帧深拷贝一份新记录。
    ///
    /// 这是「渲染路径零拷贝」的核心不变量：若哪天有人把返回类型换回
    /// `Option<TaskRecord>`，这个断言会立刻失败——否则每帧都会悄悄复制
    /// 上千条 Segment 的 String，把省下的反序列化又原样花回去。
    #[test]
    fn cached_transcription_hit_shares_one_arc() {
        use crate::subtitle::Segment;
        use std::path::PathBuf;
        use std::sync::Arc;

        let mut state = test_state(Vec::new());
        state.transcribe_file = Some(PathBuf::from("D:/video/a.mp4"));
        state
            .db
            .insert_task(
                "D:/video/a.mp4",
                "a.mp4",
                4.0,
                "completed",
                &[
                    Segment::new(1, 0.0, 2.0, "第一句"),
                    Segment::new(2, 2.0, 4.0, "第二句"),
                ],
                None,
            )
            .expect("插入应成功");

        let first: Arc<_> = state.get_cached_transcription().expect("首次应命中");
        let second: Arc<_> = state.get_cached_transcription().expect("再次应命中");
        assert!(
            Arc::ptr_eq(&first, &second),
            "缓存命中应复用同一份 Arc，而不是每帧深拷贝整份字幕"
        );
        assert_eq!(first.segments.len(), 2);
    }
    /// 回归：翻译收尾必须**按句合并**，不能整表覆盖。
    ///
    /// 翻译在后台跑几分钟，期间用户仍可编辑字幕。修前收尾是
    /// `self.segments = translated`（开始时的快照），会把用户这几分钟的编辑
    /// 静默回滚。这里模拟「翻译期间用户改了原文」，断言：译文合并进来了，
    /// 但用户的原文改动**没有被回滚**。
    #[test]
    fn merge_translations_keeps_concurrent_user_edits() {
        use crate::subtitle::Segment;

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "原始第一句"),
            Segment::new(2, 2.0, 4.0, "原始第二句"),
        ]);

        // 引擎开始时的快照（只带译文，原文是旧的）
        let mut translated = state.segments.clone();
        translated[0].translation = Some("First sentence.".to_string());
        translated[0].translation_lang = Some("English".to_string());
        translated[1].translation = Some("Second sentence.".to_string());
        translated[1].translation_lang = Some("English".to_string());

        // 翻译期间用户改了第 1 句原文、并删掉了第 2 句
        state.segments[0].text = "用户改过的第一句".to_string();
        state.segments.remove(1);

        state.merge_translations(translated);

        assert_eq!(state.segments.len(), 1, "用户删掉的句子不该被翻译结果复活");
        assert_eq!(
            state.segments[0].text, "用户改过的第一句",
            "用户的原文改动不该被翻译收尾回滚"
        );
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("First sentence."),
            "译文应正确合并进来"
        );
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English")
        );
    }

    /// 回归：引擎回填空串译文时，`merge_translations` 不能覆盖用户已有的好译文。
    ///
    /// 引擎在「本句没译出」时理论上只保留旧值，但解析失败路径可能写入空串。
    /// 若收尾无条件 `seg.translation = src.translation`，用户先前译好的句子会在
    /// 「再点一次翻译」后**变空白**。这里断言空串译文被忽略。
    #[test]
    fn merge_translations_ignores_blank_translation() {
        use crate::subtitle::Segment;

        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
        // 用户已有一句好译文
        state.segments[0].translation = Some("Hello".to_string());
        state.segments[0].translation_lang = Some("English".to_string());

        // 引擎快照里这句变成空串译文（模拟解析失败回填）
        let mut translated = state.segments.clone();
        translated[0].translation = Some("   ".to_string());
        translated[0].translation_lang = Some("English".to_string());

        state.merge_translations(translated);

        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("Hello"),
            "空串译文不该覆盖用户已有的好译文"
        );
    }

    /// 回归：结构编辑（拆/合/删）会重排序号，合并键必须是**起始时间**而非 index。
    ///
    /// 这里删掉第一句，剩余那句的 index 从 2 变成 1。若按 index 合并，它会拿到
    /// 原本属于第 1 句（start 0.0）的译文——张冠李戴。按 start 合并则正确拿到
    /// 它自己（start 2.0）的译文。
    #[test]
    fn merge_translations_survives_index_shift_from_delete() {
        use crate::subtitle::Segment;

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        // 引擎快照：两句都带各自的译文
        let mut translated = state.segments.clone();
        translated[0].translation = Some("First.".to_string());
        translated[0].translation_lang = Some("English".to_string());
        translated[1].translation = Some("Second.".to_string());
        translated[1].translation_lang = Some("English".to_string());

        // 翻译期间用户删掉了第一句，并重新编号（reindex_segments 的行为）
        state.segments.remove(0);
        state.reindex_segments();
        assert_eq!(state.segments.len(), 1);
        assert_eq!(state.segments[0].index, 1, "删句后序号被重排");
        assert_eq!(state.segments[0].start, 2.0);

        state.merge_translations(translated);

        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("Second."),
            "剩余句（start 2.0）必须拿到自己的译文，而不是被重排后的 index 撞上的第一句译文"
        );
    }
}
