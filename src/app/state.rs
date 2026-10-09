//! 应用程序全局状态

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::TaskPipeline;
use crate::engines::TranslateMode;
use crate::engines::{HardwareProfile, ProxyManager};
use crate::storage::{Database, TaskRecord};
use crate::subtitle::{plan_time_edit, Segment, MIN_EDIT_DUR};
use crate::utils::{AppConfig, FrameCache};

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
    Processing {
        stage: String,
        progress: f64,
        detail: String,
    },
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
    pub sys_cpu: f32,       // 系统总 CPU (0.0 ~ 100.0)
    pub sys_mem_used: u64,  // 系统已用内存 (bytes)
    pub sys_mem_total: u64, // 系统总内存 (bytes)

    pub proc_name: String,      // 进程标识 (如 "Qwen2.5 LLM" 或 "Voice2Word")
    pub proc_cpu: f32,          // 进程 CPU (0.0 ~ 100.0)
    pub proc_mem: u64,          // 进程占用内存 (bytes)
    pub is_model_running: bool, // 是否有模型正在工作
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
    Editor,      // 剪辑校对工作台 (主界面，默认)
    Generate,    // 智能转写生成 (轻量化无卡顿进度)
    Library,     // 历史解析视频库 (视频资产库)
    Performance, // 性能与推理设置 (硬件检测、性能评估、AI 推理决策)
}

/// 模型档位：控制 Whisper 模型大小与量化精度，平衡速度与抗口音能力
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhisperModelTier {
    SenseVoice, // 阿里 SenseVoice 极速 — model.int8.onnx (非自回归单次出字，极致提速 5~8 倍)
    Fast,       // 极速 Base — ggml-base.bin (39M 参数)
    #[default]
    Balanced, // 均衡 Small — ggml-small.bin (244M 参数)
    TurboSpeed, // 极速 Turbo — ggml-large-v3-turbo-q5_0.bin (Q5 破带宽版，提速 25%~30%)
    Precise,    // 高精 Turbo — ggml-large-v3-turbo-q8_0.bin (Q8 旗舰版，抗口音吞音)
}

/// 润色模式：CT-Punc 极速标点恢复 vs Qwen 大模型深度润色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolishMode {
    #[default]
    PuncFast, // 极速标点 (CT-Punc · 仅数秒)
    QwenDeep, // 深度润色 (Qwen · 较慢)
}

impl PolishMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolishMode::PuncFast => "punc",
            PolishMode::QwenDeep => "qwen",
        }
    }

    pub fn parse_label(s: &str) -> Self {
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

    /// Whisper 四档（不含 SenseVoice）：档位下拉菜单的遍历来源。
    ///
    /// 顺序即界面顺序：从最省资源到最准，用户从上往下读就是「越来越准、越来越慢」。
    pub const WHISPER_TIERS: [Self; 4] =
        [Self::Fast, Self::Balanced, Self::TurboSpeed, Self::Precise];

    /// 该档位对应的**可下载组件 id**（`utils::model_download::ITEMS` 里的 id）。
    ///
    /// 档位下拉里的「下载 / 删除」要落到具体的 `DownloadItem` 上，而 id 是那份清单的
    /// 唯一键。SenseVoice 不在此列：它由 `sensevoice-*` 三条目（模型 / 词表 / VAD）
    /// 共同构成，不是「一个档位 = 一个文件」，因此返回 `None`——调用方据此不渲染
    /// 单文件的下载/删除按钮，避免只删掉模型却留下悬空的词表引用。
    pub fn download_item_id(self) -> Option<&'static str> {
        match self {
            Self::SenseVoice => None,
            Self::Fast => Some("whisper-base"),
            Self::Balanced => Some("whisper-small"),
            Self::TurboSpeed => Some("whisper-turbo-q5"),
            Self::Precise => Some("whisper-turbo-q8"),
        }
    }

    /// 该档位**现在可用**的模型文件绝对路径；文件不在则 `None`。
    ///
    /// 语义刻意与 [`Self::model_relative_path`] 对齐（直接复用它）：界面上的
    /// 「已就位 / 未下载」必须与「点开始转写后管线真正会加载的那个文件」同结论，
    /// 否则会出现最气人的那种不一致——界面说未下载、其实跑得起来，或界面说已就位、
    /// 一点却报「模型加载失败」。因此档位内的量化回退（Small 的 q5_0→ggml-small、
    /// Turbo 的 q5→q8）在这里同样生效。
    ///
    /// **不**看 `paths.whisper_model`：那个字段表达的是「管线默认加载谁」，与
    /// 「这一档下没下过」无关——若跟着它走，用户把配置指到 Turbo Q8 后，四个档位
    /// 会一起显示「已就位」。
    pub fn installed_path(self) -> Option<std::path::PathBuf> {
        // 直接复用 `model_relative_path()`（含档位内量化回退），保证界面的
        // 「已就位」与管线真正会加载的文件同结论；**不**看 `paths.whisper_model`
        // ——那个字段表达「管线默认加载谁」，与「这一档下没下过」无关。
        let p = AppConfig::resolve_path(&self.model_relative_path());
        p.is_file().then_some(p)
    }

    /// 档位显示名（下拉触发器与菜单项共用，保证两处永远一致）。
    pub fn label(self) -> &'static str {
        match self {
            Self::SenseVoice => "SenseVoice 极速",
            Self::Fast => "Whisper Base",
            Self::Balanced => "Whisper Small-Q5",
            Self::TurboSpeed => "Whisper Turbo Q5",
            Self::Precise => "Whisper Turbo Q8",
        }
    }

    /// 返回对应的模型文件名（相对于 models/whisper/ 或 models/sensevoice/ 目录）
    pub fn model_filename(self) -> &'static str {
        match self {
            Self::SenseVoice => "model.int8.onnx",
            Self::Fast => "ggml-base.bin",
            Self::Balanced => "ggml-small-q5_0.bin",
            Self::TurboSpeed => "ggml-large-v3-turbo-q5_0.bin",
            Self::Precise => "ggml-large-v3-turbo-q8_0.bin",
        }
    }

    /// 返回相对路径（如果 TurboSpeed 本地未下载完成，自动平滑回退至 Q8）
    pub fn model_relative_path(self) -> String {
        match self {
            Self::SenseVoice => "models/sensevoice/model.int8.onnx".to_string(),
            Self::Fast => "models/whisper/ggml-base.bin".to_string(),
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
            Self::Precise => "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string(),
        }
    }

    /// CPU 下相对视频时长的转写耗时系数（校准自 32 分钟样片）。
    fn cpu_realtime_factor(self) -> f64 {
        match self {
            // SenseVoice 实测：32 分钟样片约 134 秒（8 线程）。
            // 旧值 0.4/32 来自早期预估，实际偏低 5.5 倍，会把「预计耗时」
            // 显示成 24 秒而用户要等 2 分多钟，属于误导，故按实测重标。
            Self::SenseVoice => 2.4 / 32.0,
            Self::Fast => 1.5 / 32.0,
            Self::Balanced => 3.8 / 32.0, // Small-Q5 CPU 16 线程约 2.8 分钟/32 分钟样片
            Self::TurboSpeed => 5.6 / 32.0, // Q5 降低内存带宽传输，提速约 30%
            Self::Precise => 8.0 / 32.0,
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

    /// GPU 相对 CPU 的**实时倍率比**（GPU 耗时 ÷ CPU 耗时），按档位标定。
    ///
    /// # 为什么必须分档，而不能用一个统一系数
    ///
    /// 旧实现对所有档位统一乘 `0.28`——那是「独显级」速度的想当然值。实测本机
    /// AMD **核显**（Radeon 780M 级，Vulkan 后端）发现：GPU 的收益高度依赖模型
    /// 大小。小模型本就跑得快、加载与调度占比高，GPU 优势小；大模型算力吃紧，
    /// GPU 才显著拉开差距。实测（本机 300 s 样片、8 线程、`-fa`）：
    ///
    /// | 档位 | CPU 耗时 | GPU 耗时 | GPU/CPU |
    /// | --- | --- | --- | --- |
    /// | Fast (Base) | 15.33 s | 8.97 s | 0.585 |
    /// | Balanced (Small-Q5) | 38.01 s | 15.64 s | 0.412 |
    /// | Turbo (Turbo-Q5) | 124.73 s | 24.84 s | 0.199 |
    ///
    /// 若继续用 0.28：Turbo 会被高估到「实际只要 25 s、界面却说 35 s」（低估速度），
    /// Base 会被低估到「实际 9 s、界面说 4 s」（高估速度）——两头都误导。
    /// 与代码里 SenseVoice 误标 5.5 倍是同一类问题，故按档位分别标定。
    ///
    /// 说明：这是**核显**实测值；独显（如 RTX）GPU 更快，此系数会偏保守
    /// （界面预估比实际慢），属于「宁可说慢」的安全方向。后续可按
    /// `HardwareProfile::is_discrete` 分流，届时补独显档位数据即可。
    fn gpu_speedup_ratio(self) -> f64 {
        match self {
            // SenseVoice 是 ONNX 引擎、不走此分支；给个中性值避免误导。
            Self::SenseVoice => 1.0,
            Self::Fast => 0.585,
            Self::Balanced => 0.412,
            Self::TurboSpeed => 0.199,
            // 高精 Turbo Q8 未单独实测，与 Q5 同为大模型，沿用同一比值。
            Self::Precise => 0.199,
        }
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
            // GPU 路径：按档位实测比值折算 CPU 基线，再叠加**弱化后**的线程因子
            // （GPU 推理几乎不吃 CPU 线程，多线程对 GPU 耗时的边际收益很小）。
            secs *= self.gpu_speedup_ratio() * (0.9 + 0.1 * thread_f);
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

/// 「0 秒智能缓存命中」查询结果的共享槽位：(文件路径, 命中记录)。
/// `None` 表示「查过库、确实没有缓存」。抽成别名只为给这个嵌套类型起个名字，
/// 暴露出的形状与语义和原字段完全一致。
pub type CachedTranscription = Arc<std::sync::Mutex<Option<(String, Option<Arc<TaskRecord>>)>>>;

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
    /// 自本次转写开始累计的已流式生成句数。
    ///
    /// 为什么不能直接用 `streaming_segments.len()`：那个缓冲区被裁剪到
    /// [`Self::STREAMING_WINDOW`]（32 句），长视频下计数会永远卡在 32——
    /// 界面上「已流式生成 N 句」看起来像卡死了。这里单独计一个
    /// 单调计数器，与缓冲区裁剪解耦。
    pub streaming_segment_count: usize,

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
    /// 编辑审计日志（有界，见 `subtitle::audit`）。
    ///
    /// 与撤销栈是**两件事**：撤销栈是「怎么退回去」（有上限、换文档即清空、退出即丢），
    /// 审计是「改过什么」（用户要能事后查看、导出、交给别人）。共用一份会导致
    /// 「撤销栈一满，审计也跟着丢」。
    pub edit_log: crate::subtitle::audit::EditLog,
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
    /// 性能页「音频预处理与救场」卡是否展开。
    ///
    /// 默认收起：出厂默认值已是实测最优，误改会变慢或变差；但功能必须能被找到——
    /// 此前这些开关**完全没有界面入口**，只有翻开 `config.toml` 才知道它们存在。
    pub show_audio_advanced: bool,
    /// 更新检查的结果：`None` = 还没查过；`Some((是否新版, 文案, 下载页))`。
    ///
    /// 缓存在会话里（不落盘）：版本不会在几分钟内变化，而用户可能反复点「检查更新」；
    /// 每次点都发一次 GitHub API 请求既慢又容易撞上未认证的速率限制。
    pub update_check_result: Option<(bool, String, String)>,
    /// 是否正在检查更新（防连点）
    pub update_check_busy: bool,

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
    pub cached_transcription: CachedTranscription,
    /// 已就位条目的缓存快照：`is_present` 要 stat 磁盘，
    /// 每帧对全部条目路径做 stat 会拖慢渲染，因此只在启动/下载完成后刷新。
    pub model_present: std::collections::HashMap<String, bool>,
    /// 命中的是否是用户自编译的自包含构建（如手编 Vulkan whisper-cli）。
    /// 与 `model_present` 一样按帧查缓存，避免渲染时反复 stat 磁盘。
    pub model_custom_build: std::collections::HashMap<String, bool>,
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
        let polish_mode = PolishMode::parse_label(&config.pipeline.polish_mode);
        let translate_mode = TranslateMode::parse_mode(&config.translate.mode);
        // 线程数按本机逻辑核心数收敛：既保证与设置页滑条量程一致，
        // 也避免把小于 8 的用户设置强行抬到 8 导致滑条取值无法持久化。
        let threads = config
            .pipeline
            .whisper_threads
            .clamp(2, Self::logical_cores().max(2));
        let whisper_model_tier =
            WhisperModelTier::from_model_path(&config.paths.whisper_model).unwrap_or_default();
        // 并行进程数交给管线（0 = 自动，按核数推导）
        pipeline.set_parallel_workers(config.pipeline.parallel_workers as usize);
        // 自动导出用的字幕样式交给管线：转写收尾的落盘拿不到 config，
        // 只能由这里在构造时把首份快照写进去（后续变更见 `save_subtitle_style`）。
        pipeline.set_subtitle_style(config.subtitle_style.clone());
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
        let recommended_profile =
            crate::core::InferenceProfile::decide(performance_level, user_strategy, &hardware_info);

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
            streaming_segment_count: 0,
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
            edit_log: crate::subtitle::audit::EditLog::new(),
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
            show_audio_advanced: false,
            update_check_result: None,
            update_check_busy: false,
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
            model_custom_build: std::collections::HashMap::new(),
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

    /// 切换 Whisper 模型档位并**落盘**（性能页档位下拉的唯一入口）。
    ///
    /// 为什么必须落盘：`whisper_model_tier` 只是运行期状态，启动时由
    /// [`WhisperModelTier::from_model_path`] 从 `config.paths.whisper_model` 反推。
    /// 此前性能页的档位胶囊只改内存、不写配置，于是「选了 Turbo Q5 → 重启又变回
    /// Small」——用户会认为选择没生效。这里把档位对应的相对路径写进配置，选择才
    /// 真正持久。
    ///
    /// **无条件**写进配置，哪怕该档位还没下载：用户选了哪一档就该记住哪一档。
    /// `from_model_path` 只按文件名认档位、不要求文件存在，所以下次启动会正确
    /// 还原成同一个档位，界面显示「未下载」并给下载入口——这比「重启后悄悄跳回
    /// 上一个档位」诚实得多（后者正是「选了不生效」的观感来源）。
    ///
    /// 未下载档位的文件缺失由开工守卫（`MainWindow::missing_engine_reason`）
    /// 在点「开始转写」时明确拦住并给出可操作提示。
    pub fn set_whisper_model_tier(&mut self, tier: WhisperModelTier) {
        self.whisper_model_tier = tier;
        self.config.paths.whisper_model = tier.model_relative_path();
        let _ = self.config.save_to_file("config.toml");
        // 档位换了，「模型与组件」卡里 whisper 各档的就位判定要跟着重算
        // （`configured_path_for` 只认「配置指向的那个档位」）。
        self.refresh_model_presence();
    }

    /// 切换「标点与润色」总开关与引擎档位，并**落盘**。
    ///
    /// # 为什么必须收成一个 setter
    ///
    /// 这两个字段（`enable_polish` / `polish_mode`）此前**任何界面入口都没写回配置**：
    /// 性能页的总开关与档位胶囊、转写页侧栏的「标点与润色」都只改内存里的
    /// `AppState`，而启动时 [`AppState::new`] 又是从 `config.pipeline` 读回来的——
    /// 于是用户把润色打开、或从「极速标点」换成「Qwen 深度润色」，**重启即失效**，
    /// 界面还会显示成配置里的旧值，看起来像选择没被记住。
    ///
    /// 更隐蔽的是「关掉润色」这一路：`enable_polish=false` 只改内存，配置里若原本
    /// 是 `true`，重启后润色又自己开了回来——用户会以为程序擅自改了他的设置。
    ///
    /// 两个字段必须一起写：`polish_mode` 单独落盘而 `enable_polish` 不落盘，
    /// 会出现「配置说开润色、但模式是用户上次没启用的那个」这类半套状态。
    pub fn set_polish(&mut self, enabled: bool, mode: PolishMode) {
        self.enable_polish = enabled;
        self.polish_mode = mode;
        self.config.pipeline.enable_polish = enabled;
        self.config.pipeline.polish_mode = mode.as_str().to_string();
        let _ = self.config.save_to_file("config.toml");
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
        self.pipeline
            .set_llm_threads(self.config.pipeline.llm_threads);

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
        // 同步一份给管线：转写收尾的自动导出没有别的途径拿到样式，
        // 而这里正是「样式刚被改过」的唯一收口（滑条 / 预设 / 预览框宽都走它）。
        self.pipeline
            .set_subtitle_style(self.config.subtitle_style.clone());
        let _ = self.config.save_to_file("config.toml");
    }

    /// 导出内容模式（原文 / 仅译文 / 双语）的落盘字符串 ↔ 枚举转换。
    ///
    /// 存在 `config.ui.export_mode` 而不是 `subtitle` 里：`utils` 不该反过来依赖
    /// `subtitle` 的枚举，用稳定的字符串做持久化边界，映射放在这层。
    pub fn export_mode_from_config(&self) -> crate::subtitle::ExportMode {
        match self
            .config
            .ui
            .export_mode
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "raw" | "raw_only" | "rawonly" => crate::subtitle::ExportMode::RawOnly,
            "translation" | "translation_only" | "translationonly" => {
                crate::subtitle::ExportMode::TranslationOnly
            }
            _ => crate::subtitle::ExportMode::Bilingual,
        }
    }

    /// 把界面上的导出内容模式写回配置并落盘（幂等）。
    pub fn set_export_mode(&mut self, mode: crate::subtitle::ExportMode) {
        let s = match mode {
            crate::subtitle::ExportMode::RawOnly => "raw",
            crate::subtitle::ExportMode::TranslationOnly => "translation",
            crate::subtitle::ExportMode::Bilingual => "bilingual",
        };
        if self.config.ui.export_mode != s {
            self.config.ui.export_mode = s.to_string();
            let _ = self.config.save_to_file("config.toml");
        }
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
            glossary_hint: self.config.translate.glossary_prompt(),
        }
    }

    /// 术语表提示（离线链路用；在线链路已随 `OnlineApiConfig` 一起传递）。
    pub fn translate_glossary_hint(&self) -> String {
        self.config.translate.glossary_prompt()
    }

    /// 术语表原始文本（供编辑界面显示条数等）。
    pub fn glossary_text(&self) -> String {
        self.config.translate.glossary.clone()
    }

    /// 术语表条目（解析后的 `(原文, 译文)` 列表）。
    pub fn glossary_entries(&self) -> Vec<(String, String)> {
        self.config.translate.glossary_entries()
    }

    /// 疑似未遵守术语表的字幕下标（供界面标注）。空术语表时为零开销。
    pub fn glossary_violations(&self) -> Vec<usize> {
        let entries = self.config.translate.glossary_entries();
        if entries.is_empty() {
            return Vec::new();
        }
        crate::subtitle::glossary_violations(&self.segments, &entries)
    }

    /// 覆盖术语表原文并落盘（编辑器关闭回填时调用）。
    pub fn set_glossary_text(&mut self, text: String) {
        self.config.translate.glossary = text;
        self.save_translate_config();
    }

    /// 把术语表写到临时文件并返回路径，供外部编辑器（记事本）打开。
    ///
    /// 用临时文件而非「对话框逐行输入」：术语表天然是多行文本，自绘单行输入框
    /// 既装不下也没法用输入法舒服地编辑；写文件让用户用顺手的编辑器改，回来再读。
    pub fn write_glossary_temp_file(&self) -> std::io::Result<std::path::PathBuf> {
        let path = std::env::temp_dir().join("voice2word_glossary.txt");
        let header = "# Voice2Word 术语表：每行一条「原文=译文」（也支持 -> / → / : 分隔）\n\
                     # 以 # 开头的是注释。保存后回到程序点「应用术语表」。\n\n";
        std::fs::write(&path, format!("{header}{}", self.config.translate.glossary))?;
        Ok(path)
    }

    /// 从临时文件读回术语表（跳过自动写入的注释头），落盘并返回生效条数。
    pub fn reload_glossary_from_temp_file(&mut self) -> std::io::Result<usize> {
        let path = std::env::temp_dir().join("voice2word_glossary.txt");
        let text = std::fs::read_to_string(&path)?;
        self.config.translate.glossary = text;
        self.save_translate_config();
        Ok(self.config.translate.glossary_entries().len())
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

    /// 是否正在转写（`ProcessStatus::Processing`）。
    ///
    /// `status` 是**全局单一**的进程态：转写进度、导出/预览失败横幅都复用它。因此任何
    /// 与转写无关的长任务（典型是翻译）在写失败态之前，都必须先问这一句。
    pub fn is_processing(&self) -> bool {
        matches!(self.status, ProcessStatus::Processing { .. })
    }

    /// 翻译失败的统一收口：写翻译面板文案，**仅在没有转写在跑时**才写全局 `status`。
    ///
    /// # 为什么不能无条件写全局 `status`
    ///
    /// `status` 只有一个格子。转写通常要跑几分钟，而这段时间里用户完全可以另起一个
    /// 翻译（翻译是纯后处理，入口只挡 `is_translating` / 空字幕表，不挡转写）。翻译失败
    /// 若在这里无条件 `status = Failed`，用户会看到「翻译失败」横幅、而状态栏里的转写
    /// 进度**凭空消失**——转写其实还在跑，界面却没有任何地方显示它。反过来，只在
    /// `translate_status_msg` 里记一句又会让「剪辑台上点翻译失败」这种没有翻译面板的
    /// 场景静默无声，所以按「有没有转写在跑」分流：没有转写才占用全局横幅。
    pub fn note_translate_failure(&mut self, msg: String) {
        self.translate_progress = 0.0;
        self.translate_status_msg = msg.clone();
        if self.is_processing() {
            // 转写在跑：只记日志 + 翻译面板，绝不碰转写进度态
            tracing::warn!("转写进行中，翻译失败只记录在翻译面板，不覆盖转写进度态: {msg}");
        } else {
            self.status = ProcessStatus::Failed(msg);
        }
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
                let h = self
                    .proxy_manager
                    .preview_height(src, !self.hardware.use_gpu_pipeline());
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
            let is_current = self
                .selected_file
                .as_ref()
                .map(|p| {
                    p == &PathBuf::from(&task.file_path)
                        || p.to_string_lossy().replace('\\', "/")
                            == task.file_path.replace('\\', "/")
                })
                .unwrap_or(false);

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
        self.streaming_segment_count += 1;
        if self.streaming_segments.len() > Self::STREAMING_WINDOW {
            let excess = self.streaming_segments.len() - Self::STREAMING_WINDOW;
            self.streaming_segments.drain(..excess);
        }
    }

    /// 清理重置实时流式转写状态
    pub fn clear_streaming(&mut self) {
        self.streaming_segments.clear();
        self.streaming_current_sec = 0.0;
        self.streaming_segment_count = 0;
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
        let hit = self
            .db
            .find_cached_task(&path_str)
            .ok()
            .flatten()
            .map(Arc::new);
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
    ///
    /// 删除会让后续下标整体前移，所以活动指针必须跟着修正。
    ///
    /// 唯一允许被删的「正在跑的那条」是其**尚在排队**（`batch_active` 尚未指向它）
    /// 的条目：真正 `Running` 的项在界面上不给删（见 `render_batch_queue_panel`）。
    /// 因此被删条目不可能恰是 `batch_active` 指向的那条，指针只需处理「前移」情形。
    pub fn remove_queue_item(&mut self, idx: usize) -> bool {
        if idx >= self.batch_queue.len() {
            return false;
        }
        self.batch_queue.remove(idx);
        self.batch_active = match self.batch_active {
            // 被删的正是活动项（理论上不会发生，防御性处理）：没有归属了，作废
            Some(active) if active == idx => None,
            // 删的是它前面的条目：整条队列前移一格，指针跟着前移
            Some(active) if active > idx => Some(active - 1),
            other => other,
        };
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
    ///
    /// 结算完必须把 `ProcessStatus` 收回 `Idle`：调用点都在「一条转写已经结束」
    /// 之后，此刻没有任何东西在跑。批量**失败**路径若漏掉这一步，状态会一直停在
    /// `Processing`，而续跑入口 `start_processing_for` 见到 `Processing` 会直接
    /// return——一个坏文件就能让整批卡死在「转写中」，后面的文件永远不开跑
    /// （队列看着在跑、进度条也不动，用户只能手动「终止批量」才出得来）。
    pub fn finish_active_queue_item(&mut self, outcome: Result<usize, String>) {
        if let Some(idx) = self.batch_active.take() {
            if let Some(item) = self.batch_queue.get_mut(idx) {
                item.state = match outcome {
                    Ok(segments) => QueueState::Done { segments },
                    Err(reason) => QueueState::Failed(reason),
                };
            }
        }
        // 无论有没有结算到具体条目（用户可能把正在跑的那条从队列里删了，
        // 于是 `batch_active` 已作废），这一条转写都结束了，状态都必须归位。
        // 只收「还在处理中」的状态：调用点可能已经先写入了更具体的失败原因
        // （如「未识别出任何有效字幕」），那类提示要留给用户看，不能被抹掉。
        if matches!(self.status, ProcessStatus::Processing { .. }) {
            self.status = ProcessStatus::Idle;
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
        self.segments
            .iter()
            .find(|seg| t >= seg.start && t < seg.end)
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
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let new_text = self.editing_text.trim().to_string();
        // 文本编辑是逐键触发的，若每敲一个字都压一层撤销栈，用户按一次 Ctrl+Z
        // 只会退掉一个字符，栈也会被一次输入撑满。所以同一句的连续输入合并成
        // 一层：只有「换了一句」或「中间夹了别的编辑」时才另起一层。
        self.snapshot_for_text_edit(idx);
        // 审计也逐键追加（日志层自带「连续同句同类型合并」的 `coalesced`，
        // 展示时再折叠——这里若也做合并，就得把合并状态塞进 AppState，
        // 与日志的职责重叠）。
        let before_text = self
            .segments
            .iter()
            .find(|s| s.index == idx)
            .map(|s| s.display_text().to_string())
            .unwrap_or_default();
        self.audit(
            crate::subtitle::audit::EditKind::Text,
            idx,
            before_text,
            new_text.clone(),
        );
        // P0-2：改原文必须让**旧译文失效**。`translation_matches` 只看「译文非空 +
        // 语言标记匹配」，而 `Segment` 里没有源文本哈希，所以只改原文不动标记时，
        // 这条会一直算「已完成」：表格里长期并列显示「新原文 + 旧译文」，点「开始翻译」
        // 还回一句「已全部翻译」——译文与原文已经对不上，却没有任何机制纠正。
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            // 只有原文**真的变了**才动标记。本函数逐键触发，若「点进来又点出去」
            // 这种没改字的调用也清标记，用户会莫名其妙丢掉完成状态。
            // 用 `display_text()` 比较：界面与翻译实际取用的就是它（优先 `polished`），
            // 与下面赋值分支的口径一致（有 `polished` 时只改 `polished`）。
            let text_changed = seg.display_text() != new_text.as_str();
            seg.text = new_text.clone();
            if !seg.polished.is_empty() {
                seg.polished = new_text;
            }
            // 方案 A（最小侵入）：只清语言标记、**保留译文文本**——用户仍能看到旧译文
            // 作参考（界面不显示「—」），但 `translation_matches` 立刻失效，
            // 下一次翻译会把这一句重译。没有译文（含只有空白）时不动标记：
            // 没有「陈旧译文」可言，也就无需失效。
            // 残留风险：`has_translation()` 仍为 true，双语导出仍会带出这条陈旧译文；
            // 彻底解决需要给 `Segment` 加「译文是否对应当前原文」的标记或源文本哈希，
            // 但 `src/subtitle/segment.rs` 不在本次可改清单内。
            if text_changed && seg.has_translation() {
                seg.translation_lang = None;
            }
        }
        // 去抖：编辑逐键触发，若每次都全量序列化全部片段写 SQLite，长视频输入时是隐形 I/O 热点。
        // 仅标记脏，等切句/跳转/播放/导出等天然节点由 flush_segments_if_dirty 统一写回。
        self.segments_dirty = true;
        self.bump_segments_revision();
    }

    /// 直接改写当前选中片段的**译文**（用户在对照表里手工订正机翻）。
    ///
    /// 与原文编辑走同一条脏标记 + 去抖落库路径；译文本身不参与 `merge_translations`
    /// 的「只搬非空译文」覆盖，用户改完不会被下一次翻译任务回滚（那套合并按 start 匹配，
    /// 且只在引擎给出非空译文时才覆盖，见 `merge_translations`）。
    ///
    /// 传入空白串视为「清除译文」，回到无译文状态（对照表显示「—」）。
    pub fn set_selected_translation(&mut self, text: &str) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        self.snapshot_for_undo();
        let cleaned = text.trim().to_string();
        let before_translation = self
            .segments
            .iter()
            .find(|s| s.index == idx)
            .and_then(|s| s.translation.clone())
            .unwrap_or_default();
        self.audit(
            crate::subtitle::audit::EditKind::Translation,
            idx,
            before_translation,
            cleaned.clone(),
        );
        // P0-1：用户此刻是在「当前目标语言」下手工写这条译文，这个动作表达的就是
        // 「这条译文属于当前目标语言」，所以下面非空分支要**无条件**用当前目标语言
        // 覆盖标记。绝不能保留旧标记——那正是本缺陷的根因：English 译文 → 切到 日本語
        // → 手工改成日文，若标记仍是 "English"，`translation_matches("日本語")` 返回
        // false，这句被当**待译**，下一次翻译收尾就用机翻结果覆盖掉用户手写的日文；
        // 而全仓库没有任何 UI 写 `translation_lang`，用户没有「把这条标成日语」的入口，
        // 界面层无法自救，只能在这里落定。
        //
        // 取值来源与有效性：`translate_target_lang` 构造时取自
        // `config.translate.target_lang`（默认 "简体中文"，见
        // `default_translate_target_lang`），唯一写入方 `set_translate_target_lang`
        // 只接受 `TRANSLATE_TARGET_LANGS` 里的显示名，因此正常路径下必是非空显示名。
        // 防御：真出现空串时回落到配置值；两者都空则**不动标记**——写个空标记同样
        // 匹配不上任何语言，只会让这句反复被重译。
        let target_lang = if self.translate_target_lang.trim().is_empty() {
            self.config.translate.target_lang.clone()
        } else {
            self.translate_target_lang.clone()
        };
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            if cleaned.is_empty() {
                seg.translation = None;
                seg.translation_lang = None;
            } else {
                seg.translation = Some(cleaned);
                // 无条件覆盖标记（理由见函数开头的 P0-1 说明）。修前这里是
                // `if seg.translation_lang.is_none()`：只补「原先没有标记」的情况，
                // 于是「已有别的语言标记」的句子改完仍带着旧标记，下次翻译必然重译并覆盖。
                if !target_lang.trim().is_empty() {
                    seg.translation_lang = Some(target_lang.clone());
                }
            }
        }
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

    /// 记一条审计（内部便利方法，省掉调用点重复写 `DateTime::now()`）。
    ///
    /// 直接用 `Local::now()` 而不是让调用方传时间：审计要的是「真实发生时刻」，
    /// 而纯函数层（`audit.rs`）把时间作为参数收进来，正是为了**它可以被单测**。
    /// 两个目标不冲突：这里读钟、那边不读。
    fn audit(
        &mut self,
        kind: crate::subtitle::audit::EditKind,
        index: usize,
        before: String,
        after: String,
    ) {
        self.edit_log.push(crate::subtitle::audit::EditRecord {
            index,
            kind,
            before,
            after,
            at: chrono::Local::now(),
        });
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
        let Some(snap) = self.undo_stack.pop() else {
            return false;
        };
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
        let Some(snap) = self.redo_stack.pop() else {
            return false;
        };
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
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };

        let cur = self.segments[pos].clone();
        let prev = pos
            .checked_sub(1)
            .map(|p| (self.segments[p].start, self.segments[p].end));
        let next = self.segments.get(pos + 1).map(|s| (s.start, s.end));
        let edit = plan_time_edit(cur.start, cur.end, prev, next, delta_start, delta_end);

        self.snapshot_for_undo();
        // 审计记的是**规划后**的实际结果，不是用户点的那个 ±0.1——微调会被邻居
        // 边界挡住（`plan_time_edit` 可能收短/后挪），日志里若写「用户点了 +0.1」
        // 就与实际发生的事不符，事后对照会误导。
        self.audit(
            crate::subtitle::audit::EditKind::Timing,
            idx,
            format!("{:.3}-{:.3}", cur.start, cur.end),
            format!("{:.3}-{:.3}", edit.start, edit.end),
        );

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
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        let orig = self.segments[pos].clone();

        let cur_text = orig.display_text().to_string();
        let char_count = cur_text.chars().count();
        if char_count <= 1 {
            return;
        }
        let split_pos = split_at.unwrap_or(char_count / 2).clamp(1, char_count - 1);
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

        // P0-3：译文必须**按同一个 ratio 切开**，左右各拿一半，绝不能整条复制给两半
        // （修前是 `translation: orig.translation.clone()`）：复制会让双语导出出现两个
        // cue 显示同一条完整译文（原文却已正确切开），而且本函数收尾立刻 flush 落库，
        // 重复译文会被持久化。切分策略与自动拆分（`subtitle::segment` 里的
        // `split_following`，目前是私有函数、无法直接复用）逐条一致：先按标点就近切、
        // 找不到标点再按比例硬切，**绝不让后半段落空**。
        let (trans_left, trans_right) = match orig.translation.as_deref() {
            Some(t) if !t.trim().is_empty() => {
                let (l, r) = split_text_by_ratio(t, ratio);
                (Some(l), r)
            }
            // 没有译文（或只有空白）时两半都不带译文：不能凭空造出 `Some("")`，
            // 那会让双语导出多出一行空白。
            _ => (None, None),
        };
        let trans_lang = if trans_left.is_some() {
            orig.translation_lang.clone()
        } else {
            None
        };

        self.snapshot_for_undo();
        self.segments[pos].end = split_time;
        if !self.segments[pos].polished.is_empty() {
            self.segments[pos].polished = part1;
        } else {
            self.segments[pos].text = part1;
        }
        // 左半段也要换成切分后的译文；原样保留（修前行为）等于把整条译文复制一份。
        self.segments[pos].translation = trans_left;
        self.segments[pos].translation_lang = trans_lang.clone();

        let new_seg = Segment {
            index: orig.index + 1,
            start: split_time,
            end: orig.end,
            text: part2.clone(),
            translation: trans_right,
            translation_lang: trans_lang,
            polished: if !orig.polished.is_empty() {
                part2
            } else {
                String::new()
            },
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
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        if pos + 1 >= self.segments.len() {
            return;
        }

        self.snapshot_for_undo();
        // 合并同样是破坏性的（少了一个 cue）。记下合并前的两句原文。
        let before_merge = format!(
            "{} | {}",
            self.segments[pos].display_text(),
            self.segments[pos + 1].display_text()
        );
        self.audit(
            crate::subtitle::audit::EditKind::Merge,
            idx,
            before_merge,
            String::new(),
        );
        let next = self.segments.remove(pos + 1);
        // P0-4：译文不能丢。`next` 被 remove 之后它的译文就没了，而原文是「拼接」的，
        // 合并后的 cue 理应对应两段译文的拼接。修前这里完全没碰译文：首句无译文、
        // 次句有译文时，合并后译文直接消失（双语导出少一行），且立刻落库。
        // 必须在 `cur` 被可变借用之前把两边的译文取出来。
        let (merged_translation, merged_translation_lang) = merge_concat_translations(
            self.segments[pos].translation.as_deref(),
            self.segments[pos].translation_lang.as_deref(),
            next.translation.as_deref(),
            next.translation_lang.as_deref(),
        );
        let cur = &mut self.segments[pos];
        cur.end = next.end;
        let combined = format!("{}{}", cur.display_text(), next.display_text());
        if !cur.polished.is_empty() || !next.polished.is_empty() {
            cur.polished = combined;
        } else {
            cur.text = combined;
        }
        cur.translation = merged_translation;
        cur.translation_lang = merged_translation_lang;

        self.reindex_segments();
        self.bump_segments_revision();
        self.select_segment(idx);
        self.segments_dirty = true;
        self.flush_segments_if_dirty();
    }

    /// 删除当前选中的字幕片段
    pub fn delete_selected_segment(&mut self) {
        let Some(idx) = self.selected_segment_index else {
            return;
        };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else {
            return;
        };
        self.snapshot_for_undo();
        // 删除是破坏性操作：审计必须留下**被删掉的内容**，否则事后无从追查
        // 「原来那句是什么」（撤销栈只有 50 层，且换文档即清空）。
        let removed_text = self.segments[pos].display_text().to_string();
        self.audit(
            crate::subtitle::audit::EditKind::Delete,
            idx,
            removed_text,
            String::new(),
        );
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
    /// 全部条目路径逐帧 stat 是白白的系统调用开销。
    pub fn refresh_model_presence(&mut self) {
        // 直接用**内存里的** config：AppState 自己就持有它，判定时再
        // `load_from_file` 一次纯属浪费（启动路径上会白读一遍 TOML + 解析）。
        // 顺带也解决了「每个条目各读一遍」——统一用这一份配置。
        let ctx = crate::utils::model_download::PresenceContextRef::new(&self.config);
        self.model_present = crate::utils::ITEMS
            .iter()
            .map(|i| (i.id.to_string(), ctx.is_present(i)))
            .collect();
        // 同时缓存「自编译构建」判定：同样要 stat 磁盘，不能每帧现算。
        self.model_custom_build = crate::utils::ITEMS
            .iter()
            .map(|i| (i.id.to_string(), ctx.is_custom_build(i)))
            .collect();
    }

    /// 某个条目是否已就位（查缓存快照，不碰磁盘）。
    pub fn model_is_present(&self, id: &str) -> bool {
        self.model_present.get(id).copied().unwrap_or(false)
    }

    /// 某个条目命中的是否是**用户自编译的自包含构建**（查缓存快照，不碰磁盘）。
    ///
    /// 界面据此把徽标从「已就位」换成「自编译」，让用户一眼看出这是自己编的版本
    /// （例如手编的 Vulkan whisper-cli），而不是本项目下载展开的官方包——
    /// 后者被覆盖升级无所谓，前者被覆盖会丢 GPU 支持。
    pub fn model_is_custom_build(&self, id: &str) -> bool {
        self.model_custom_build.get(id).copied().unwrap_or(false)
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

/// 按 `ratio` 把文本切成前后两半，供手工拆分时译文跟随切点。
///
/// 必须与 `crate::subtitle::segment` 里自动拆分用的 `split_following` **保持同一套
/// 策略**：优先在标点处切（切点标点归前段），找不到可用标点再按比例硬切，
/// **绝不让后半段落空**——否则右半段会退化成「没有译文」，双语导出时整句译文
/// 全挂在左行，右行只剩原文。两份实现分家会重新长出「同一句话自动拆和手工拆得到
/// 不同的译文切点」这类不一致；`split_following` 目前是 `subtitle` 模块的私有函数，
/// 无法直接复用（建议把它提升为 `pub` 供这里调用，见任务报告）。
///
/// 文本长度不足 2 无法切分时返回 `(原文, None)`，由调用方按「不切」处理。
fn split_text_by_ratio(text: &str, ratio: f64) -> (String, Option<String>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= 1 {
        return (text.to_string(), None);
    }
    let cut = split_index_at_ratio(text, ratio).unwrap_or_else(|| {
        let c = (n as f64 * ratio.clamp(0.0, 1.0)).round() as usize;
        c.clamp(1, n - 1)
    });
    let left: String = chars[..cut].iter().collect();
    let right: String = chars[cut..].iter().collect();
    (left, Some(right))
}

/// 可作为切分点的中英文标点（与 `subtitle::segment::is_split_punct` 同集合）
fn is_split_punct(c: char) -> bool {
    matches!(
        c,
        '，' | '。' | '！' | '？' | '；' | '、' | '：' | '…' | ',' | '.' | '!' | '?' | ';' | ':'
    )
}

/// 在 `text` 中寻找最接近 `ratio` 位置的标点切点（切点标点归前段）。
/// 与 `subtitle::segment::split_index_at_ratio` 逐条同规则：切点强制落在 25%~75%
/// 区间、两侧各留至少 2 字符；无合理切点返回 `None`。
fn split_index_at_ratio(text: &str, ratio: f64) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n < 4 {
        return None;
    }
    let target = (n as f64 * ratio.clamp(0.0, 1.0)) as usize;
    let lo = ((n as f64) * 0.25) as usize;
    let hi = ((n as f64) * 0.75).ceil() as usize;
    let mut best: Option<(usize, i64)> = None;
    for (i, &c) in chars.iter().enumerate() {
        if !is_split_punct(c) {
            continue;
        }
        let left_len = i + 1;
        if left_len < 2 || n - left_len < 2 || left_len < lo || left_len > hi {
            continue;
        }
        let dist = (left_len as i64 - target as i64).abs();
        if best.is_none_or(|(_, best_dist)| dist < best_dist) {
            best = Some((left_len, dist));
        }
    }
    best.map(|(cut, _)| cut)
}

/// 「合并下句」时把首句与次句的译文拼成一条。
///
/// 三种组合：
/// - 两边都没有（含只有空白）→ `(None, None)`，不造 `Some("")`；
/// - 只有一边有 → 原样保留那一边的译文与它的语言标记；
/// - 两边都有 → 用换行拼接（双语导出本来就是「译文在上、原文在下」，
///   译文里多一行是自洽的，SRT/ASS 都会原样保留换行）。
///
/// 语言标记：两边相同 → 保留（拼接后的 cue 仍是该语言的译文）；不同 → 置 `None`。
/// 理由：一条 cue 里混着两种目标语言的译文，任何单一标记都是谎报——留下任一边的标记，
/// `translation_matches` 会宣称整条都已是那个语言、永远不会重译；置 `None` 则让这条
/// 在下一次翻译里被当待译重译，同时译文文本仍在（界面不显示「—」）。
fn merge_concat_translations(
    cur: Option<&str>,
    cur_lang: Option<&str>,
    next: Option<&str>,
    next_lang: Option<&str>,
) -> (Option<String>, Option<String>) {
    let cur_text = cur.filter(|t| !t.trim().is_empty());
    let next_text = next.filter(|t| !t.trim().is_empty());
    let cur_lang = cur_lang.filter(|l| !l.trim().is_empty());
    let next_lang = next_lang.filter(|l| !l.trim().is_empty());
    match (cur_text, next_text) {
        (None, None) => (None, None),
        (Some(t), None) => (Some(t.to_string()), cur_lang.map(str::to_string)),
        (None, Some(t)) => (Some(t.to_string()), next_lang.map(str::to_string)),
        (Some(a), Some(b)) => {
            let lang = match (cur_lang, next_lang) {
                (Some(l), Some(r)) if l == r => Some(l.to_string()),
                _ => None,
            };
            (Some(format!("{a}\n{b}")), lang)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, PolishMode, ProcessStatus, WhisperModelTier};
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

    /// 润色「档位 → 配置字符串 → 档位」必须往返一致，且「关闭」只关总开关。
    ///
    /// 回归护栏：`enable_polish` / `polish_mode` 此前**没有任何界面入口写回配置**，
    /// 用户关掉润色、重启又被配置里的 `true` 打开（看起来像程序擅自改设置）。
    /// 现在所有入口都经 `AppState::set_polish`，这里锁住它的配置侧契约：
    /// ① 档位字符串与 `parse_label` 往返一致（启动回读的就是这个映射）；
    /// ② 关闭只写 `enable_polish=false`，不顺手改写已选档位（否则关一下再开，
    ///    引擎会悄悄从 Qwen 变回 CT-Punc）。
    #[test]
    fn polish_config_roundtrips_mode_and_keeps_mode_when_disabled() {
        for mode in [PolishMode::PuncFast, PolishMode::QwenDeep] {
            let s = mode.as_str();
            assert_eq!(
                PolishMode::parse_label(s),
                mode,
                "档位字符串 {s} 必须能被 parse_label 复原（启动回读走的就是它）"
            );
        }
        // 关闭：沿用当前档位，仅总开关置假
        let mut pipeline = crate::utils::AppConfig::default().pipeline;
        pipeline.enable_polish = true;
        pipeline.polish_mode = PolishMode::QwenDeep.as_str().to_string();
        // 模拟 set_polish(false, 当前档位)
        pipeline.enable_polish = false;
        let text = toml::to_string_pretty(&pipeline).expect("serialize");
        let back: crate::utils::config::PipelineConfig =
            toml::from_str(&text).expect("deserialize");
        assert!(
            !back.enable_polish,
            "关闭润色必须持久化，否则重启自己开回来"
        );
        assert_eq!(
            PolishMode::parse_label(&back.polish_mode),
            PolishMode::QwenDeep,
            "关闭不应改写已选档位"
        );
    }

    /// 档位 → 可下载组件 id 的映射必须与 `ITEMS` 对得上，且四档都不是压缩包
    /// （压缩包组件不显示删除按钮）。这条锁住的是「界面上的下载/删除按钮」与
    /// 「下载层真正能删的东西」之间的接线——一旦有人改了 id 或把某档换成 zip，
    /// 这里先红。
    #[test]
    fn whisper_tier_download_ids_exist_and_are_deletable() {
        for tier in WhisperModelTier::WHISPER_TIERS {
            let id = tier
                .download_item_id()
                .unwrap_or_else(|| panic!("{tier:?} 应映射到可下载组件"));
            assert!(
                crate::utils::model_download::item_by_id(id).is_some(),
                "{tier:?} 的组件 id {id} 不在 ITEMS 里"
            );
            assert!(
                crate::utils::model_download::item_is_deletable(id),
                "{tier:?} 的组件 {id} 应可删除（界面据此渲染删除按钮）"
            );
        }
        // SenseVoice 不是「一个档位 = 一个文件」，必须返回 None
        assert!(WhisperModelTier::SenseVoice.download_item_id().is_none());
    }

    /// 档位显示名四档各异且非空——下拉触发器与菜单项共用它，重复会让用户分不清。
    #[test]
    fn whisper_tier_labels_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for tier in WhisperModelTier::WHISPER_TIERS {
            let label = tier.label();
            assert!(!label.is_empty());
            assert!(seen.insert(label), "档位显示名重复: {label}");
        }
        assert!(!WhisperModelTier::SenseVoice.label().is_empty());
    }

    /// `installed_path` 只认该档位自己的默认文件：配置指向别的档位时，
    /// 没下过的档位不能跟着显示「已就位」。
    #[test]
    fn installed_path_does_not_follow_configured_other_tier() {
        let mut cfg = crate::utils::AppConfig::default();
        cfg.paths.whisper_model = "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string();
        // 本测试断言的是**存在性**语义，不依赖磁盘：
        // 非首选量化的 Balanced 只在 filesystem 真有 ggml-small-q5_0.bin 时才 Some，
        // 与配置无关。
        // installed_path 必须与 model_relative_path 同结论（界面「已就位」与管线
        // 实际加载的文件一致），且**不受** config 指向影响。
        for tier in WhisperModelTier::WHISPER_TIERS {
            let rel = crate::utils::AppConfig::resolve_path(&tier.model_relative_path());
            assert_eq!(
                tier.installed_path().is_some(),
                rel.is_file(),
                "{tier:?} 的 installed_path 应与 model_relative_path 一致"
            );
        }
        // 配置指向 turbo-q8 时，Balanced 是否可用只取决于小模型自己（含 q5_0→
        // ggml-small 的档位内回退），不会因为配置指向 turbo 就跟着变 true。
        let small_q5 = crate::utils::AppConfig::resolve_path("models/whisper/ggml-small-q5_0.bin");
        let small_plain = crate::utils::AppConfig::resolve_path("models/whisper/ggml-small.bin");
        assert_eq!(
            WhisperModelTier::Balanced.installed_path().is_some(),
            small_q5.is_file() || small_plain.is_file(),
            "Balanced 不应跟着 config 指向的 turbo-q8 一起变已就位"
        );
    }

    /// 编辑必须留下审计记录，且**破坏性操作要能追回被删的内容**。
    ///
    /// 这条锁住的是「审计接线」：`audit()` 的调用点分散在 4 个编辑入口，漏掉任何一个
    /// 都会让用户事后查不到那次改动。撤销栈救不了这个——它只有 50 层、换文档即清空、
    /// 退出即丢。
    #[test]
    fn destructive_edits_are_recorded_with_their_old_content() {
        use crate::subtitle::audit::EditKind;
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 1.0, "第一句"),
            Segment::new(2, 1.0, 2.0, "第二句"),
        ]);

        // 删除第 2 句：审计里必须留下「被删掉的那句原文」
        state.select_segment(2);
        state.delete_selected_segment();
        let deletes: Vec<_> = state.edit_log.of_kind(EditKind::Delete);
        assert_eq!(deletes.len(), 1, "删除必须留一条审计");
        assert_eq!(deletes[0].index, 2);
        assert_eq!(deletes[0].before, "第二句", "必须记下被删的内容");
        assert!(deletes[0].after.is_empty());

        // 合并：记录合并前的两句
        state.select_segment(1);
        // 只剩一句时合并是空操作（pos+1 越界），先补一句回来构造可合并的状态
        state.segments.push(Segment::new(2, 1.0, 2.0, "第二句"));
        state.merge_selected_with_next();
        let merges: Vec<_> = state.edit_log.of_kind(EditKind::Merge);
        assert_eq!(merges.len(), 1, "合并必须留一条审计");
        assert!(
            merges[0].before.contains("第一句") && merges[0].before.contains("第二句"),
            "合并记录应含合并前的两句: {}",
            merges[0].before
        );
    }

    /// 时间微调的审计必须记**规划后的实际结果**，而不是用户点的那一下。
    ///
    /// 微调会被邻居边界挡住（`plan_time_edit` 可能收短/后挪），若日志里写「用户点了
    /// +0.1」就与实际发生的事不符，事后对照会误导。
    #[test]
    fn timing_audit_records_planned_result_not_requested_delta() {
        use crate::subtitle::audit::EditKind;
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 1.0, "a"),
            Segment::new(2, 1.0, 2.0, "b"),
        ]);
        state.select_segment(2);
        // 起点往前推 0.5 秒：会越界压到第 1 句，`plan_time_edit` 会收短它。
        state.adjust_selected_times(-0.5, 0.0);
        let records: Vec<_> = state.edit_log.of_kind(EditKind::Timing);
        assert_eq!(records.len(), 1);
        // before 是原区间，after 必须是规划结果（与当前实际时间一致）
        assert_eq!(records[0].before, "1.000-2.000");
        let seg = state
            .segments
            .iter()
            .find(|s| s.index == 2)
            .expect("第 2 句应还在");
        assert_eq!(
            records[0].after,
            format!("{:.3}-{:.3}", seg.start, seg.end),
            "审计记录必须与落定后的真实时间一致"
        );
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
        assert!(
            t8 > t16,
            "8 threads ({t8}) should be slower than 16 ({t16})"
        );
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

    /// P0-A5：GPU ETA 按档位标定——大模型的 GPU 加速比要显著大于小模型，
    /// 否则 Turbo 档会被严重高估（界面说 35 s、实际 25 s）。
    #[test]
    fn gpu_speedup_is_tier_dependent() {
        let d = 32.0 * 60.0;
        let ratio = |tier: WhisperModelTier| {
            let cpu = tier.estimate_seconds(d, 8, false, false);
            let gpu = tier.estimate_seconds(d, 8, true, false);
            gpu / cpu
        };
        let fast = ratio(WhisperModelTier::Fast);
        let balanced = ratio(WhisperModelTier::Balanced);
        let turbo = ratio(WhisperModelTier::TurboSpeed);
        assert!(
            fast > balanced && balanced > turbo,
            "GPU 加速比应随模型变大而增大：fast={fast:.3} balanced={balanced:.3} turbo={turbo:.3}"
        );
        // 实测核显：Turbo 的 GPU/CPU 约 0.20，留出余量取 0.28 上限。
        assert!(
            turbo < 0.28,
            "Turbo GPU ETA 不应再被 0.28 系数高估：{turbo:.3}"
        );
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

    /// 批量续跑的关键点：`finish_active_queue_item` 必须把 `ProcessStatus` 收回 `Idle`。
    ///
    /// 修前失败路径只改了队列卡片，状态仍停在 `Processing`；而续跑入口
    /// `start_processing_for` 一见 `Processing` 就 `return`——一个坏文件足以让整批
    /// 卡死在「转写中」，后面的文件永远不开跑，用户只能手动「终止批量」才出得来。
    #[test]
    fn finishing_active_item_clears_processing_status() {
        use super::{ProcessStatus, QueueItem, QueueState};
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        state.batch_active = Some(0);
        state.batch_queue[0].state = QueueState::Running;
        state.status = ProcessStatus::Processing {
            stage: "转写中".to_string(),
            progress: 0.3,
            detail: String::new(),
        };

        state.finish_active_queue_item(Err("坏文件".to_string()));

        assert!(state.batch_queue[0].state.is_failed());
        assert!(state.batch_active.is_none());
        assert_eq!(
            state.status,
            ProcessStatus::Idle,
            "批量失败后状态必须归位，否则续跑入口会把整批卡死"
        );

        // 调用点可能已经先写入了更具体的失败原因（如「未识别出有效字幕」），
        // 那类提示要留给用户看，不能被归位成 Idle 抹掉。
        state.status = ProcessStatus::Failed("未识别出有效字幕".to_string());
        state.batch_active = Some(0);
        state.batch_queue[0].state = QueueState::Running;
        state.finish_active_queue_item(Err("未识别出有效字幕".to_string()));
        assert_eq!(
            state.status,
            ProcessStatus::Failed("未识别出有效字幕".to_string()),
            "更具体的失败原因必须保留"
        );
    }

    /// 从队列里删条目时，活动指针必须跟着位移：删掉正在跑那条**之前**的条目后，
    /// 指针若不作废就会指向别人——`finish_active_queue_item` 会把完成/失败记到
    /// 另一条卡片上，而真正在跑的那条永远停在「转写中」。
    #[test]
    fn removing_queue_item_keeps_active_pointer_on_the_running_item() {
        use super::{QueueItem, QueueState};
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
        ];
        // b.mp4 正在跑（下标 1）
        state.batch_active = Some(1);
        state.batch_queue[1].state = QueueState::Running;

        // 删掉它前面的 a.mp4：指针必须前移到 0，仍指向 b.mp4
        assert!(state.remove_queue_item(0));
        assert_eq!(state.batch_active, Some(0));
        assert_eq!(state.batch_queue[0].name, "b.mp4");

        state.finish_active_queue_item(Ok(7));
        assert_eq!(
            state.batch_queue[0].state,
            QueueState::Done { segments: 7 },
            "结算必须落在真正在跑的那条上"
        );

        // 删掉正在跑的那条：指针作废，不再指到别人头上
        state.batch_active = Some(1);
        assert!(state.remove_queue_item(1));
        assert_eq!(state.batch_active, None);

        // 越界删除是安全空操作，且不动指针
        state.batch_active = Some(0);
        assert!(!state.remove_queue_item(9));
        assert_eq!(state.batch_active, Some(0));
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
        assert!(
            state.can_undo(),
            "前置条件：甲工程里应当攒下了一次可撤销的编辑"
        );

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
            content_hash: None,
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
        assert!(state.segments.is_empty(), "撤销把已经清空的工程片段复活了");
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
        let before_len: usize = state
            .segments
            .iter()
            .map(|s| s.display_text().chars().count())
            .sum();
        state.split_selected_segment(Some(1));
        assert_eq!(state.segments.len(), 2, "应当拆成两段");
        assert_eq!(
            state
                .segments
                .iter()
                .map(|s| s.display_text().chars().count())
                .sum::<usize>(),
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
        assert_eq!(
            after.len(),
            2,
            "拆出的片段被 optimize_segments 当鬼影删掉了"
        );
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
            state.push_stream_segment(Segment::new(
                i,
                (i as f64) * 2.0,
                (i as f64) * 2.0 + 1.0,
                "句子",
            ));
        }
        assert!(
            state.streaming_segments.len() <= AppState::STREAMING_WINDOW,
            "窗口必须钉死在上限内，实际 {}",
            state.streaming_segments.len()
        );
        // 计数器不受窗口裁剪影响：已流式生成句数应等于实际 push 的总数
        assert_eq!(
            state.streaming_segment_count, 500,
            "计数器必须是单调累计值，不能被缓冲区裁剪截断"
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
        assert_eq!(state.streaming_segment_count, 0, "清理必须把计数器也归零");
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

    /// 用户手工订正译文：写入非空译文、空串清除、以及自动补目标语言标记。
    #[test]
    fn set_selected_translation_edits_and_clears() {
        use crate::subtitle::Segment;

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "你好"),
            Segment::new(2, 2.0, 4.0, "世界"),
        ]);
        state.translate_target_lang = "English".to_string();

        // 选中第一句，手工写入译文
        state.select_segment(1);
        state.set_selected_translation("  Hello  ");
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("Hello"),
            "应写入去除首尾空白的译文"
        );
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English"),
            "凭空补的译文应带上当前目标语言标记，否则会被判为未完成"
        );
        assert!(state.segments[0].translation_matches("English"));

        // 空白串 = 清除译文
        state.set_selected_translation("   ");
        assert!(!state.segments[0].has_translation(), "空白应清除译文");
        assert!(state.segments[0].translation.is_none());

        // 未选中任何片段时是安全空操作
        state.selected_segment_index = None;
        state.set_selected_translation("Whatever");
        assert!(!state.segments[1].has_translation());
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

    // ───────────── P0-1：手工订正译文必须表达「属于当前目标语言」─────────────

    /// English 译文 → 切到 日本語 → 手工改译文，标记必须变成 日本語。
    ///
    /// 修前只补「原先没有标记」的情况，已有 "English" 的句子改完仍带旧标记，
    /// 于是这句会被下一次翻译当**待译**，收尾时用机翻结果覆盖掉用户手写的日文。
    #[test]
    fn manual_translation_retags_to_current_target_lang() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
        // 先按 English 译过一遍
        state.segments[0].translation = Some("Hello".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        // 用户把目标语言切到 日本語（直接改字段，避免单测落盘 config.toml）
        state.translate_target_lang = "日本語".to_string();

        state.select_segment(1);
        state.set_selected_translation("こんにちは");

        assert_eq!(state.segments[0].translation.as_deref(), Some("こんにちは"));
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("日本語"),
            "手工订正发生在当前目标语言下，标记必须无条件改成当前目标语言"
        );
        assert!(
            !state.segments[0].translation_matches("English"),
            "旧语言标记必须被覆盖，否则这句会被判成「已是 English」"
        );
        assert!(state.segments[0].translation_matches("日本語"));
    }

    /// 核心防回归断言：手工订正后这句**不会**再被当待译，因此下一次翻译不会覆盖它。
    ///
    /// 这里复刻两条翻译链路共用的增量判据
    /// （`!translate_source().trim().is_empty() && !translation_matches(target)`，
    /// 见 `engines::llm` 与 `engines::translate`），并走一遍 `merge_translations` 收尾。
    #[test]
    fn manual_translation_is_not_pending_on_next_translate() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
        state.segments[0].translation = Some("Hello".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        state.translate_target_lang = "日本語".to_string();

        state.select_segment(1);
        state.set_selected_translation("こんにちは");

        let target = "日本語";
        let pending: Vec<usize> = state
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.translate_source().trim().is_empty() && !s.translation_matches(target)
            })
            .map(|(pos, _)| pos)
            .collect();
        assert!(
            pending.is_empty(),
            "手写的日文译文仍被判为待译，下一次翻译会把它覆盖掉：{pending:?}"
        );
        // 「已完成 N/M」同样按当前目标语言判定（UI 用它决定按钮是否可点）
        assert_eq!(state.translated_count_for(target), 1);

        // 端到端：引擎在「无待译」时原样返回快照，合并收尾不能改动用户的译文
        let engine_snapshot = state.segments.clone();
        state.merge_translations(engine_snapshot);
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("こんにちは"),
            "用户手写的日文被翻译收尾覆盖了"
        );
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("日本語")
        );

        // 反向对照：若标记仍是 English（修前行为），这句必然落进 pending
        state.segments[0].translation_lang = Some("English".to_string());
        assert!(
            !state.segments[0].translation_matches(target),
            "对照：旧标记下这句应被判为待译（这正是修前的缺陷链）"
        );
    }

    /// 空串仍表示「清除译文」：translation 与 translation_lang 双双置 None（现有行为不能破坏）。
    #[test]
    fn clearing_manual_translation_nulls_both_fields() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "你好")]);
        state.translate_target_lang = "日本語".to_string();
        state.select_segment(1);
        state.set_selected_translation("こんにちは");
        assert!(state.segments[0].has_translation());

        state.set_selected_translation("   ");
        assert!(state.segments[0].translation.is_none());
        assert!(state.segments[0].translation_lang.is_none());
        assert!(!state.segments[0].has_translation());
    }

    // ───────────── P0-2：改原文必须让旧译文失效 ─────────────

    /// 有译文的句子改原文 → `translation_matches(原目标语言)` 变 false，译文文本仍在。
    #[test]
    fn editing_source_invalidates_stale_translation() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "旧原文")]);
        state.segments[0].translation = Some("Old translation".to_string());
        state.segments[0].translation_lang = Some("English".to_string());

        state.select_segment(1);
        state.editing_text = "新原文".to_string();
        state.save_selected_text();

        assert_eq!(state.segments[0].text, "新原文");
        assert!(
            !state.segments[0].translation_matches("English"),
            "原文已改，旧译文不该再算「已完成」，否则下一次翻译会直接跳过这句"
        );
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("Old translation"),
            "只清语言标记、保留译文文本，用户仍能看到旧译文作参考"
        );
        // 残留风险（已知）：has_translation() 仍为 true，双语导出仍会带出这条陈旧译文。
        assert!(state.segments[0].has_translation());
    }

    /// 原文未变（同一文本重复保存）→ 标记不动。
    #[test]
    fn saving_unchanged_source_keeps_translation_lang() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原文没动")]);
        state.segments[0].translation = Some("Untouched".to_string());
        state.segments[0].translation_lang = Some("English".to_string());

        state.select_segment(1);
        // 选中时 editing_text 已被填成 display_text；再保存一次属于「没改字」
        state.save_selected_text();
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English"),
            "原文没变时不该清标记，否则点进来又点出去就丢掉完成状态"
        );
        assert!(state.segments[0].translation_matches("English"));
    }

    /// 无译文的句子改原文 → 不 panic、无副作用（不该凭空写出标记或译文）。
    #[test]
    fn editing_source_without_translation_is_noop() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "还没翻译")]);
        state.select_segment(1);
        state.editing_text = "改过但还没翻译".to_string();
        state.save_selected_text();

        assert_eq!(state.segments[0].text, "改过但还没翻译");
        assert!(state.segments[0].translation.is_none());
        assert!(state.segments[0].translation_lang.is_none());
        assert!(!state.segments[0].has_translation());
    }

    /// 展示文本取自 `polished` 时，改 `polished` 同样要判为「原文变了」。
    #[test]
    fn editing_polished_source_invalidates_translation() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "原始识别")]);
        state.segments[0].polished = "润色后的原文".to_string();
        state.segments[0].translation = Some("Stale".to_string());
        state.segments[0].translation_lang = Some("English".to_string());

        state.select_segment(1);
        assert_eq!(state.editing_text, "润色后的原文");
        state.editing_text = "又改过的润色原文".to_string();
        state.save_selected_text();

        assert!(!state.segments[0].translation_matches("English"));
        assert_eq!(state.segments[0].polished, "又改过的润色原文");
    }

    // ───────────── P0-3：手工拆分必须按 ratio 切开译文 ─────────────

    /// 有译文的一句在中间拆分 → 左右各拿一半，两半不相同、拼起来等于原译文。
    #[test]
    fn manual_split_splits_translation_by_ratio() {
        let mut state = test_state(vec![Segment::new(
            1,
            0.0,
            10.0,
            "前面这半句讲的是背景，后面这半句讲的是结论。",
        )]);
        let original = "The first half is background, the second half is the conclusion.";
        state.segments[0].translation = Some(original.to_string());
        state.segments[0].translation_lang = Some("English".to_string());

        state.select_segment(1);
        state.editing_text = state.segments[0].display_text().to_string();
        state.split_selected_segment(None);

        assert_eq!(state.segments.len(), 2, "应拆成两段");
        let left = state.segments[0]
            .translation
            .clone()
            .expect("左半段应带译文");
        let right = state.segments[1]
            .translation
            .clone()
            .expect("右半段应带译文");
        assert_ne!(left, right, "两半译文不该是同一条（修前就是整条复制）");
        assert_eq!(
            format!("{left}{right}"),
            original,
            "两半译文拼起来应等于原译文"
        );
        // 两半都保留目标语言标记，否则拆完会被判「未完成」而重复请求翻译
        assert!(state.segments[0].translation_matches("English"));
        assert!(state.segments[1].translation_matches("English"));
        assert!(state.segments[0].has_translation());
        assert!(state.segments[1].has_translation());
    }

    /// 无译文的一句拆分 → 两半都无译文（不能凭空造出译文）。
    #[test]
    fn manual_split_without_translation_stays_none() {
        let mut state = test_state(vec![Segment::new(
            1,
            0.0,
            10.0,
            "前面这半句讲的是背景，后面这半句讲的是结论。",
        )]);
        state.select_segment(1);
        state.editing_text = state.segments[0].display_text().to_string();
        state.split_selected_segment(None);

        assert_eq!(state.segments.len(), 2);
        for seg in &state.segments {
            assert!(seg.translation.is_none(), "不该凭空造出译文");
            assert!(seg.translation_lang.is_none());
            assert!(!seg.has_translation());
        }
    }

    /// 译文只有空白 → 拆分后两半都不带 `Some("")`（双语导出会多出一行空白）。
    #[test]
    fn manual_split_blank_translation_stays_none() {
        let mut state = test_state(vec![Segment::new(
            1,
            0.0,
            10.0,
            "前面这半句讲的是背景，后面这半句讲的是结论。",
        )]);
        state.segments[0].translation = Some("   ".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        state.select_segment(1);
        state.editing_text = state.segments[0].display_text().to_string();
        state.split_selected_segment(None);

        assert_eq!(state.segments.len(), 2);
        for seg in &state.segments {
            assert!(seg.translation.is_none());
            assert!(seg.translation_lang.is_none());
        }
    }

    // ───────────── P0-4：合并下句不能丢译文 ─────────────

    /// 首句无译文、次句有译文 → 合并后保留次句译文（修前直接丢）。
    #[test]
    fn merge_keeps_next_translation_when_cur_has_none() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.segments[1].translation = Some("Second.".to_string());
        state.segments[1].translation_lang = Some("English".to_string());
        state.selected_segment_index = Some(1);

        state.merge_selected_with_next();

        assert_eq!(state.segments.len(), 1);
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("Second."),
            "次句译文不该随被删片段一起消失"
        );
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English")
        );
    }

    /// 首句有译文、次句无 → 保留首句译文与标记。
    #[test]
    fn merge_keeps_cur_translation_when_next_has_none() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.segments[0].translation = Some("First.".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        state.selected_segment_index = Some(1);

        state.merge_selected_with_next();

        assert_eq!(state.segments.len(), 1);
        assert_eq!(state.segments[0].translation.as_deref(), Some("First."));
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English")
        );
    }

    /// 两边都有、语言相同 → 译文拼接（换行）、语言标记保留。
    #[test]
    fn merge_concats_translations_with_same_lang() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.segments[0].translation = Some("First.".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        state.segments[1].translation = Some("Second.".to_string());
        state.segments[1].translation_lang = Some("English".to_string());
        state.selected_segment_index = Some(1);

        state.merge_selected_with_next();

        assert_eq!(state.segments.len(), 1);
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("First.\nSecond."),
            "两边都有译文时应拼接，而不是只留一边"
        );
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("English"),
            "两半语言相同，拼接后仍是该语言的译文"
        );
    }

    /// 两边都有、语言不同 → 拼接译文但**清掉语言标记**，让下一次翻译重译这条混合语言 cue。
    #[test]
    fn merge_drops_lang_when_languages_differ() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.segments[0].translation = Some("First.".to_string());
        state.segments[0].translation_lang = Some("English".to_string());
        state.segments[1].translation = Some("二番目。".to_string());
        state.segments[1].translation_lang = Some("日本語".to_string());
        state.selected_segment_index = Some(1);

        state.merge_selected_with_next();

        assert_eq!(state.segments.len(), 1);
        assert_eq!(
            state.segments[0].translation.as_deref(),
            Some("First.\n二番目。")
        );
        assert!(
            state.segments[0].translation_lang.is_none(),
            "一条 cue 混了两种语言，任何单一标记都是谎报，应置 None 迫使重译"
        );
        assert!(!state.segments[0].translation_matches("English"));
        assert!(!state.segments[0].translation_matches("日本語"));
        // 译文文本仍在：界面不显示「—」，用户还能看到内容
        assert!(state.segments[0].has_translation());
    }

    /// 两边都没译文 → 合并后仍无译文（不造 `Some("")`）。
    #[test]
    fn merge_without_translations_stays_none() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.selected_segment_index = Some(1);

        state.merge_selected_with_next();

        assert_eq!(state.segments.len(), 1);
        assert!(state.segments[0].translation.is_none());
        assert!(state.segments[0].translation_lang.is_none());
        assert!(!state.segments[0].has_translation());
    }
    /// 转写进行中（Processing）时翻译失败**不得**覆盖全局 `status`：否则状态栏里的
    /// 转写进度会凭空消失。失败信息只落在翻译面板自己的 `translate_status_msg`。
    #[test]
    fn translate_failure_does_not_clobber_processing_status() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "第一句")]);
        state.status = ProcessStatus::Processing {
            stage: "识别中".to_string(),
            progress: 0.42,
            detail: "whisper 正在解码".to_string(),
        };
        assert!(state.is_processing());

        state.note_translate_failure("翻译失败：接口超时".to_string());

        assert!(
            state.is_processing(),
            "翻译失败不能把转写进度态冲掉: {:?}",
            state.status
        );
        assert_eq!(state.translate_progress, 0.0);
        assert_eq!(state.translate_status_msg, "翻译失败：接口超时");
    }

    /// 没有转写在跑时照旧写全局 `status`（剪辑台 / 视频库这些没有翻译面板的地方，
    /// 仍要靠跨页错误横幅把失败告诉用户）。
    #[test]
    fn translate_failure_sets_status_when_not_processing() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "第一句")]);
        assert!(!state.is_processing());

        state.note_translate_failure("翻译失败：密钥无效".to_string());

        assert_eq!(
            state.status,
            ProcessStatus::Failed("翻译失败：密钥无效".to_string())
        );
        assert_eq!(state.translate_status_msg, "翻译失败：密钥无效");
    }
}
