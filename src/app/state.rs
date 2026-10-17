//! 应用程序全局状态

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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
    /// 本条自己的转写进度，单位是**百分比**（0.0 ~ 100.0）。
    ///
    /// 与 `ProcessStatus::Processing::progress`（0.0 ~ 1.0 的比率）刻意不同单位：
    /// 面板要显示的是整数百分比，直接用比率就得在每个渲染点乘 100 并处理四舍五入；
    /// 收尾判定（「满格 = 不再画进度」）也就跟着散在多处。这里存百分比，
    /// 卡片侧只做取整。
    ///
    /// 为什么不并进 [`QueueState`]：批量面板要按「运行中 / 已完成 / 失败」三套配色
    /// 渲染卡片，状态每多一个进度字段，`match item.state` 就得在四五个调用点同时
    /// 跟着改；而进度是**瞬态数据**，每条重跑都要清零，跟状态的生命周期不是一回事。
    /// 分开存还让「失败」保住出事那一刻的进度——把进度塞进状态枚举就做不到这点
    /// （失败分支持的是 `String` 原因）。
    pub progress: f64,
    /// 本条正在跑的阶段名（沿用管线 `PipelineEvent::Progress` 给的字符串，
    /// 只改标点不改措辞）。条目未开跑 / 已结算后为 `None`，否则收尾时会留下
    /// 上一阶段的残影，用户以为还卡在那个阶段。
    pub stage: Option<String>,
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
            progress: 0.0,
            stage: None,
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
    Tiny,       // 尝鲜 Tiny — ggml-tiny.bin (75MB 超轻量)
    Fast,       // 极速 Base — ggml-base.bin (148MB)
    #[default]
    Balanced,   // 均衡 Small — ggml-small.bin / ggml-small-q5_0.bin (181MB)
    Medium,     // 进阶 Medium — ggml-medium.bin (1.5GB)
    TurboSpeed, // 极速 Turbo — ggml-large-v3-turbo-q5_0.bin (547MB)
    Precise,    // 高精 Turbo — ggml-large-v3-turbo-q8_0.bin (833MB)
    LargeV3,    // 全尺寸旗舰 — ggml-large-v3.bin (3.1GB 完整版)
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
            "ggml-tiny.bin" => Some(Self::Tiny),
            "ggml-base.bin" => Some(Self::Fast),
            "ggml-small.bin" | "ggml-small-q5_0.bin" => Some(Self::Balanced),
            "ggml-medium.bin" => Some(Self::Medium),
            "ggml-large-v3-turbo-q5_0.bin" => Some(Self::TurboSpeed),
            "ggml-large-v3-turbo-q8_0.bin" => Some(Self::Precise),
            "ggml-large-v3.bin" => Some(Self::LargeV3),
            _ => None,
        }
    }

    /// Whisper 纯 Whisper 档位（不含 SenseVoice）：按速度与精度排列。
    pub const WHISPER_TIERS: [Self; 7] = [
        Self::Tiny,
        Self::Fast,
        Self::Balanced,
        Self::Medium,
        Self::TurboSpeed,
        Self::Precise,
        Self::LargeV3,
    ];

    // 注：这里曾有 `ALL_TIERS` 与 `available_installed_tiers()`——「在 8 个预设枚举里
    // 筛出已下载档位」的实现。它已被 `utils::local_models` 的目录扫描取代：
    // 枚举只能表达内置预设，扫不出用户自备的模型。枚举现在只提供「友好名 / 估算系数」
    // 这类元数据，不再充当可用性判据（见 `local_models` 模块头的说明）。

    /// 该档位对应的**可下载组件 id**（`utils::model_download::ITEMS` 里的 id）。
    pub fn download_item_id(self) -> Option<&'static str> {
        match self {
            Self::SenseVoice => None,
            Self::Tiny => Some("whisper-tiny"),
            Self::Fast => Some("whisper-base"),
            Self::Balanced => Some("whisper-small"),
            Self::Medium => Some("whisper-medium"),
            Self::TurboSpeed => Some("whisper-turbo-q5"),
            Self::Precise => Some("whisper-turbo-q8"),
            Self::LargeV3 => Some("whisper-large-v3"),
        }
    }

    /// 该档位**现在可用**的模型文件绝对路径；文件不在则 `None`。
    pub fn installed_path(self) -> Option<std::path::PathBuf> {
        let p = AppConfig::resolve_path(&self.model_relative_path());
        p.is_file().then_some(p)
    }

    /// 档位显示名（下拉触发器与菜单项共用，保证两处永远一致）。
    pub fn label(self) -> &'static str {
        match self {
            Self::SenseVoice => "SenseVoice 极速",
            Self::Tiny => "Whisper Tiny",
            Self::Fast => "Whisper Base",
            Self::Balanced => "Whisper Small-Q5",
            Self::Medium => "Whisper Medium",
            Self::TurboSpeed => "Whisper Turbo Q5",
            Self::Precise => "Whisper Turbo Q8",
            Self::LargeV3 => "Whisper Large-V3",
        }
    }

    /// 返回对应的模型文件名（相对于 models/whisper/ 或 models/sensevoice/ 目录）
    pub fn model_filename(self) -> &'static str {
        match self {
            Self::SenseVoice => "model.int8.onnx",
            Self::Tiny => "ggml-tiny.bin",
            Self::Fast => "ggml-base.bin",
            Self::Balanced => "ggml-small-q5_0.bin",
            Self::Medium => "ggml-medium.bin",
            Self::TurboSpeed => "ggml-large-v3-turbo-q5_0.bin",
            Self::Precise => "ggml-large-v3-turbo-q8_0.bin",
            Self::LargeV3 => "ggml-large-v3.bin",
        }
    }

    /// 返回相对路径（如果 TurboSpeed 本地未下载完成，自动平滑回退至 Q8 或 Large-V3）
    pub fn model_relative_path(self) -> String {
        match self {
            Self::SenseVoice => "models/sensevoice/model.int8.onnx".to_string(),
            Self::Tiny => "models/whisper/ggml-tiny.bin".to_string(),
            Self::Fast => "models/whisper/ggml-base.bin".to_string(),
            Self::Balanced => {
                let preferred = "models/whisper/ggml-small-q5_0.bin".to_string();
                if crate::utils::AppConfig::resolve_path(&preferred).exists() {
                    preferred
                } else {
                    "models/whisper/ggml-small.bin".to_string()
                }
            }
            Self::Medium => "models/whisper/ggml-medium.bin".to_string(),
            Self::TurboSpeed => {
                let preferred = "models/whisper/ggml-large-v3-turbo-q5_0.bin".to_string();
                if !crate::utils::AppConfig::resolve_path(&preferred).exists() {
                    if crate::utils::AppConfig::resolve_path("models/whisper/ggml-large-v3-turbo-q8_0.bin").exists() {
                        "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string()
                    } else if crate::utils::AppConfig::resolve_path("models/whisper/ggml-large-v3.bin").exists() {
                        "models/whisper/ggml-large-v3.bin".to_string()
                    } else {
                        preferred
                    }
                } else {
                    preferred
                }
            }
            Self::Precise => "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string(),
            Self::LargeV3 => "models/whisper/ggml-large-v3.bin".to_string(),
        }
    }


    /// CPU 下相对视频时长的转写耗时系数（校准自 32 分钟样片）。
    fn cpu_realtime_factor(self) -> f64 {
        match self {
            Self::SenseVoice => 2.4 / 32.0,
            Self::Tiny => 0.8 / 32.0,
            Self::Fast => 1.5 / 32.0,
            Self::Balanced => 3.8 / 32.0,
            Self::Medium => 6.5 / 32.0,
            Self::TurboSpeed => 5.6 / 32.0,
            Self::Precise => 8.0 / 32.0,
            Self::LargeV3 => 11.0 / 32.0,
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
            Self::SenseVoice => 1.0,
            Self::Tiny => 0.70,
            Self::Fast => 0.585,
            Self::Balanced => 0.412,
            Self::Medium => 0.30,
            Self::TurboSpeed => 0.199,
            Self::Precise => 0.199,
            Self::LargeV3 => 0.180,
        }
    }

    /// 预估的 Qwen 深度润色附加开销（秒/分钟素材）。
    ///
    /// 与 CT-Punc（毫秒级，可忽略）不同，Qwen 是逐批 LLM 解码，耗时与素材量成正比，
    /// 必须计入预估，否则「预计 2 分钟」实际要等十几分钟。
    const QWEN_POLISH_SEC_PER_MIN: f64 = 2.4;

    /// 预估转写秒数。GPU 上线程收益更弱；润色按**档位**计附加开销。
    ///
    /// # 为什么必须区分润色档位
    ///
    /// 旧签名只有一个 `enable_polish: bool`，于是无法表达三档截然不同的代价：
    /// - SenseVoice + `punc`：管线**整段跳过**标点（模型原生自带 ITN），开销 0；
    /// - 其它引擎 + `punc`：CT-Punc 毫秒级，开销可忽略；
    /// - 任意引擎 + `qwen`：逐批 LLM 解码，32 分钟素材实测约 +1.3 分钟。
    ///
    /// 而 SenseVoice 恰是出厂默认引擎、`punc` 又是默认档位——不区分就只能二选一：
    /// 要么把 Qwen 的开销算给误开了它的 SenseVoice 用户（虚高），要么一律不算
    /// （选 Qwen 的人被严重低估）。按档位分别计，两种误导同时消掉。
    pub fn estimate_seconds(
        self,
        duration_sec: f64,
        threads: u32,
        use_gpu: bool,
        polish: Option<PolishMode>,
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
        // 只有 Qwen 档才计附加开销：CT-Punc 毫秒级，SenseVoice 的 `punc` 更是整段跳过。
        if matches!(polish, Some(PolishMode::QwenDeep)) {
            secs += (media / 60.0) * Self::QWEN_POLISH_SEC_PER_MIN;
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

/// 脏字幕的落库去抖窗口：用户停手这么久之后，才把整份工程写回历史库。
///
/// # 为什么要去抖
///
/// 落库 = 把整份 `segments` 序列化成 JSON + 一条 sqlite UPDATE。5000 句的工程大约
/// 10~30ms。而拖时间轴的定位请求每 40ms 就来一次（见 `seek_by_mouse_x` 的节流），
/// 切句 / 暂停播放同样是高频动作——逐次写库等于让磁盘写把交互本身拖慢，
/// 用户看到的就是「拖一下卡一下」。
///
/// 停手 1.2s 后写一次：连续操作自动合成一次写盘。真崩了最多丢这 1.2s 内的编辑，
/// 而换工程 / 导出 / 关窗守卫走的都是**立刻写入**的那条路，不会丢。
pub const SEGMENTS_FLUSH_IDLE: std::time::Duration = std::time::Duration::from_millis(1200);

/// 单次「同步落库」等待后台写线程的最长时间。
///
/// 超过这个时间就不再干等，直接自己写（见 `flush_segments_if_dirty`）。宁可让当前这一份
/// 最新快照先落盘（即便可能越过那个还没写完的旧快照），也不拖住关闭窗口 / 导出 / 换工程
/// 这些「写完才能继续」的路径：那些路径的语义是「返回时数据已在库里」。
const SEGMENTS_SYNC_FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// 待落库的一份一致快照（序列化好的整份字幕 + 目标行 + 修订号）。
///
/// 单独成型是为了让「取快照」与「写到哪一行」分开：前者必须留在渲染线程（要读正在编辑
/// 的 `segments`），后者挪到后台。
struct SegmentWriteSnapshot {
    /// 目标行：派发那一刻的工程主键。
    task_id: i64,
    /// 派发那一刻的字幕修订号，用于判断回报是否已经过期。
    revision: u64,
    /// 序列化好的整份字幕（`segments_json` 参数原样带上）。
    ///
    /// # 为什么把序列化留在渲染线程，而不是把整份 Vec 交给后台
    ///
    /// `Database` 里给后台写库准备的入口是 `update_task_segments_json(id, &str)`：
    /// 它接收已经序列化好的 JSON，后台只解析一次算派生列（样例文本 / 句数 / 有无译文）。
    /// 于是后台线程不需要 `Vec<Segment>` 的所有权，快照可以是**纯 String**——
    /// 但代价是序列化发生在渲染线程上，这与本任务「把写库挪走」的目标并不冲突：
    /// 任务要求允许「序列化在当前线程做（要拿一致快照）」，真正必须挪走的是 sqlite 写。
    /// 另一个好处是 `Segment` 类型不必跨线程克隆，派发时只移动一段字符串。
    json: String,
}

/// 一次「把字幕快照写进库」的结局（见 [`AppState::dispatch_segment_write`]）。
///
/// # 为什么需要区分「在飞」与「已保存」
///
/// 去抖路径派发成功后会**保留** `segments_dirty`，直到后台回报确认落库——脏标记的语义
/// 是「当前内容还没落库」，而派发只是把这份快照交出去，还没写完。关窗守卫读的就是它：
/// 若派发那一刻就清脏，用户「改完 → 去抖派发 → 立刻关窗」这条链上，守卫会以为没有未保存
/// 内容而放行，此时在飞写一旦失败，改动就**静默丢失**（失败回报要等下一次 poll 才被看到，
/// 而窗口已经关了）。
///
/// 但后台线程起不来、退回同步写的那条兜底也可能**当场写完**——那种结局必须直接清脏，
/// 否则「没有任何后台回报」会让脏标记永远挂着。所以调用方需要三态，而不是一个布尔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentDispatch {
    /// 已交给后台写线程：内容「在飞」，落库与否要等回报。
    Dispatched,
    /// 已经落库（同工程水位判定「无需再写」或同步兜底成功）。
    Saved,
    /// 没写进库（后台线程起不来且同步兜底也失败）：错误已挂上提示条。
    Failed,
}

/// 后台写线程回报给自己派发方的一条「写完了」信号。
///
/// # 为什么用通道，而不是「后台线程直接改 AppState」
///
/// 渲染线程按帧借用 `AppState`（`&mut`），后台线程若直接持有它就得共享可变引用——那等于
/// 把整个界面状态变成全局锁。通道把「后台只想说的一件事」抽成值：序号 + 修订号 + 结局。
/// 派发侧在渲染线程上收（`poll_background_writes`），于是**只有渲染线程写
/// `db_write_error` / `segments_dirty`**，不需要给 AppState 加锁，也不会出现
/// 「后台线程把一个已经换掉的工程标脏」。
///
/// # 错误怎么回到界面（本任务的难点）
///
/// 后台线程只把 `Result::Err` 的原样字符串塞进 `result` 投回来；`drain_segment_writes`
/// 在渲染线程上把它拼成「字幕保存失败：…」挂进 `db_write_error`，并（在快照仍是当前
/// 修订时）把 `segments_dirty` 置回触发重试。界面读的是原有字段，所以提示条与重试逻辑
/// 一行都不用改。
#[derive(Debug, Clone)]
struct SegmentWriteDone {
    /// 派发时领到的单调写序号（见 `BackgroundWriteTracker::next_seq`）。
    seq: u64,
    /// 快照对应的工程主键（原样带回，后台不查状态）。
    ///
    /// 只用于诊断日志与未来「按 task 归并失败」的扩展，回报判定不用它（判定看 seq +
    /// revision + error_at_dispatch 三件套）。留 `#[allow(dead_code)]` 而不是删掉：
    /// 删了之后 `tracing` 里那条「写过哪一行」的信息就再也补不回来了。
    #[allow(dead_code)]
    task_id: i64,
    /// 落库时字幕内容的修订号：用来判断「这份快照是否已经被更新的编辑取代」。
    revision: u64,
    /// 派发时 `db_write_error` 的值：只有它没被后续更晚的失败改过，才允许按本次结局处理。
    error_at_dispatch: Option<String>,
    /// 写成功 / 失败原因（失败原因直接来自 sqlite 与它的前置校验）。
    result: Result<(), String>,
}

/// 后台写线程的共享账本：单调写序号 + 按工程的落库水位。
///
/// 这两样东西共同实现「防回退」——旧快照的结局不许覆盖新状态：
///
/// - `next_seq`：派发序号只增不减，后台结果自带「我是哪一代」；同步写也从同一
///   序列领号，因此同步写与后台写在序号上可以直接比较先后；
/// - `applied`：**每个工程已经真正落库**的最大写序号。同步写与后台写都在写入闸
///   （[`BackgroundWrites::lock_writer`]）里先查它再写，于是更旧的快照不可能在
///   更新的内容之后落地——不依赖线程调度的先后（[`write_snapshot_under_gate`]）；
///   结果通道（`submit` / `drain_results`）也按它判断一条回报是不是已经被同工程
///   更新的写覆盖，因此不在这里囤失败队列。
///
/// `applied` 按**工程**分开记而不是记一个全局水位：全局水位会让「B 工程的新写」
/// 把「A 工程还没落地的最新快照」也一起吞掉，那是最难排查的一种丢写。
#[derive(Debug)]
struct BackgroundWriteTracker {
    /// 下一个要发放的写序号。从 **1** 开始：0 专门表示「还没写过」，
    /// 于是「同工程水位 >= 待发序号」这条判据不会被第一次派发误命中。
    next_seq: AtomicU64,
    /// 每个工程已经真正落库的最大写序号（`tasks.id` → seq）。
    ///
    /// 只在写入闸里读写，所以用普通 `Mutex` 而不是原子类型；条目数 = 本次运行
    /// 写过库的工程数，量级很小，不需要清理。
    applied: Mutex<HashMap<i64, u64>>,
}

impl Default for BackgroundWriteTracker {
    fn default() -> Self {
        Self {
            // 见字段注释：序号从 1 起，0 是「还没写过」的哨兵。
            next_seq: AtomicU64::new(1),
            applied: Mutex::new(HashMap::new()),
        }
    }
}

impl BackgroundWriteTracker {
    /// 某个工程已落库的最大写序号（0 = 还没写过）。
    fn applied_seq(&self, task_id: i64) -> u64 {
        self.applied
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&task_id)
            .copied()
            .unwrap_or(0)
    }

    /// 记下一次**真正落库**的写。只增不减：并发下后到的更小序号绝不能把水位拉回去，
    /// 否则防回退就白做了。
    fn note_applied(&self, task_id: i64, seq: u64) {
        let mut applied = self.applied.lock().unwrap_or_else(|e| e.into_inner());
        let entry = applied.entry(task_id).or_insert(0);
        if *entry < seq {
            *entry = seq;
        }
    }
}

/// 后台写入子系统（在 `AppState` 里以 `Arc` 持有一份）。
///
/// # 同一 task_id 的写入为什么能按序号顺序落地
///
/// 两道闸：
/// 1. **发放侧串行**：派发时 `writer` 这把锁把「查同工程水位 -> 领写序号 -> 起写线程」
///    做成对同步写与其它派发原子的临界区；
/// 2. **写入侧串行 + 按序号收口**：每个后台线程拿到快照后**先取同一把锁再写库**
///    （`write_guard`），同步写同样持锁写完；锁里同时完成「查同工程水位 -> 写 -> 记水位」
///    （[`write_snapshot_under_gate`]）。
///
/// 只靠「串行」是不够的：普通 `Mutex` 不保证先到先得，两个线程谁先拿到锁由调度决定，
/// 更晚派发（更新）的快照完全可能先写、再被更旧的快照盖掉。加上按工程的水位之后，
/// **更新的写一旦落库，更旧的写在锁里就会看到水位比自己新而直接放弃**——不依赖运气。
///
/// 再加上 `Database` 内部那条连接锁，写入本身也已经串行——这里额外加锁是为了**顺序**：
/// 光靠连接锁只能保证不交错，不保证先后。
///
/// # 为什么天然不会积压
///
/// 去抖路径停手 1.2s 才派发一次，而一次写只要 10~30ms，正常情况永远只有一个任务在飞。
/// 满负荷下（磁盘卡住 + 用户狂停手）最多也只是每 1.2s 多一个线程，线程写完即退，不占
/// 常驻资源；`in_flight_writes` 是「已派发还没收尾」的诊断计数器（派发时 +1、
/// 写线程退出时 -1），真正的等待判据是 `segments_write_pending`。
#[derive(Debug)]
struct BackgroundWrites {
    writer: Mutex<()>,
    tracker: Arc<BackgroundWriteTracker>,
    submit: std::sync::mpsc::Sender<SegmentWriteDone>,
    results: Mutex<std::sync::mpsc::Receiver<SegmentWriteDone>>,
    in_flight_writes: AtomicUsize,
}

impl Default for BackgroundWrites {
    fn default() -> Self {
        let (submit, results) = std::sync::mpsc::channel();
        Self {
            writer: Mutex::new(()),
            tracker: Arc::new(BackgroundWriteTracker::default()),
            submit,
            results: Mutex::new(results),
            in_flight_writes: AtomicUsize::new(0),
        }
    }
}

/// 「这一份后台写还没收尾」的凭证：**派发侧**领序号时 +1，任务收尾时 -1。
///
/// # 为什么 +1 在派发侧，而不是线程体里
///
/// 线程体里 +1 的写法有一个致命窗口：从派发返回到线程真正开跑之间，`pending` 还是 0。
/// 同步落库（关窗 / 换工程 / 导出）等的就是它——被这个窗口骗到就会「以为没有在飞的写」
/// 直接往下写，随后那条更旧的后台快照才落地，把新内容盖回去（旧快照的写序号比水位旧，
/// 本该被拦住）。计数点放在派发侧之后，`pending == 0` 才真的等于
/// 「没有已派发未收尾的写」。
///
/// 计数在**结果投递成功之后**才归零（`disarm`），所以 `pending == 0` 的含义是
/// 「结果已经在通道里」。剩下那些「没走到投递」的结局（提前放弃、panic、发送失败）
/// 由 `Drop` 兜底 -1，免得每个 return 分支都得记得递减（漏一处，计数就永远回不了零）。
///
/// 派发失败（线程都起不来）时没有线程来持有这份凭证，由派发侧自己 -1——见
/// `dispatch_segment_write` 的失败分支。
struct InFlightGuard(Arc<AtomicUsize>);

impl InFlightGuard {
    /// 结果已经交给通道：可以安全地报告「这一份不在飞了」。
    fn disarm(self) {}
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl BackgroundWrites {
    /// 取 `writer` 锁，**容忍 Mutex 中毒**（与 `Database::lock_conn` 同策略）。
    ///
    /// 中毒只说明「上次有人在持锁时 panic 了」，守卫本身要保护的是**写入顺序**，不是数据：
    /// panic 的那个线程要么还没写、要么已经写完，锁里没有半截状态。若这里 `unwrap()`，
    /// 一次后台 panic 会让此后每一次派发与同步落库都连环 panic，整程序直接不能保存字幕
    /// ——比继续用这把锁危险得多。
    fn lock_writer(&self) -> MutexGuard<'_, ()> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 排空后台结果通道：只留「还没有结论」的那些回报。
    ///
    /// # 这里丢掉的是什么（防回退的一部分）
    ///
    /// **同一个工程**已经有更新的写落库（`applied[task] > seq`）：这条迟到的回报描述的
    /// 是一个**已经被覆盖**的快照，再挂提示只会让用户以为「刚存好的东西又坏了」。留日志即可。
    ///
    /// 判据必须按工程比水位、不能拿全局最大写序号：写序号是跨工程共用的，但两个工程的
    /// 内容互不覆盖——拿 A 的新序号压 B 的迟到失败，会让 B 的失败既不提示、也不置脏，
    /// 而 B 当前修订其实还没落库（静默丢写，正是这条通道最不该发生的事）。
    ///
    /// 提示条的竞价（`error_at_dispatch` 与排空前快照不一致就整条不回写）**不在这里做**，
    /// 而是留给 [`drain_segment_writes`]：那里除了提示条，还要处理「这条失败描述的内容
    /// 到底还该不该重试」。判定只留一处，免得两处漂移成两个答案。
    ///
    /// 与 `lock_writer` 同策略容忍中毒：`results` 里装的是一次性判定用的值，没有半截状态；
    /// 而这里一旦 panic，界面就再也收不到写库失败的提示（错误被静默）。
    fn drain_results(&self) -> Vec<SegmentWriteDone> {
        let receiver = self.results.lock().unwrap_or_else(|e| e.into_inner());
        let mut kept = Vec::new();
        for done in receiver.try_iter() {
            if self.tracker.applied_seq(done.task_id) > done.seq {
                if let Err(err) = &done.result {
                    tracing::debug!(
                        seq = done.seq,
                        task_id = done.task_id,
                        error = %err,
                        "同工程已有更新的写落库，丢弃过期的后台写失败"
                    );
                }
                continue;
            }
            kept.push(done);
        }
        kept
    }
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
    /// 自本次转写开始累计的已流式生成句数。
    ///
    /// 为什么不能直接用 `streaming_segments.len()`：那个缓冲区被裁剪到
    /// [`Self::STREAMING_WINDOW`]（32 句），长视频下计数会永远卡在 32——
    /// 界面上「已流式生成 N 句」看起来像卡死了。这里单独计一个
    /// 单调计数器，与缓冲区裁剪解耦。
    pub streaming_segment_count: usize,
    /// 转写任务启动的绝对时刻，用于实时推算真实剩余耗时（ETA）
    pub transcribe_started_at: Option<std::time::Instant>,

    // 处理选项
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    pub polish_mode: PolishMode,
    pub whisper_threads: u32,
    /// 当前选中的**模型文件绝对路径**——「用哪个模型」的唯一真相。
    ///
    /// 为什么不再让 `whisper_model_tier` 当真相：枚举只能表达 8 个内置预设，
    /// 用户自备的模型（自行量化的 q5_1、改名后的权重）在枚举里根本没有对应值，
    /// 于是「看不到、选不了、也不会被加载」。改成路径后，磁盘上有什么就能用什么。
    ///
    /// 与 `config.paths.whisper_model` 的关系：这里存解析后的绝对路径，
    /// 配置里存可直接落盘的形式（项目内相对路径 / 目录外绝对路径）。
    pub selected_model_path: PathBuf,
    /// 当前选中模型命中的预设档位；认不出预设时按体积启发式给一个估算档位
    /// （仅供友好名与耗时估算，不再充当可用性判据）。
    pub whisper_model_tier: WhisperModelTier,

    /// 用户已点击「终止转写」，等待管线收尾事件（用于区分取消与真实失败）
    pub cancel_requested: bool,

    /// 字幕编辑产生的未落库脏标记（去抖：切句/跳转/播放/导出时统一写回）
    pub segments_dirty: bool,

    /// 「待落库」登记：高频交互节点（跳转 / 切句 / 暂停播放 / 就地改一句）不再立刻
    /// 写库，而是把时刻记在这里，由界面侧的定时器在停手满 [`SEGMENTS_FLUSH_IDLE`]
    /// 后统一 flush（见 [`AppState::flush_segments_if_due`]）。
    pub segments_flush_due: Option<std::time::Instant>,

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

    /// 后台写线程的共享账本 + 串行闸（见 `BackgroundWrites`）：去抖路径
    /// （`flush_segments_if_dirty_async`）派发的写在这里排队，写库本身不占渲染线程。
    segments_writes: Arc<BackgroundWrites>,
    /// 给后台线程用的过闸入口（转写收尾回写上一个工程的脏字幕）。
    ///
    /// 单独存一份而不是让调用点现拼：它必须与派发 / 同步写共用同一个
    /// `BackgroundWrites`（同一把写入闸、同一份按工程水位），否则「收尾写的旧内容
    /// 盖掉在飞的新内容」这条又会回来。
    segments_write_gate: SegmentWriteGate,

    /// 「已派发但还没落库」的字幕快照数（含正在写与排队中的）。
    ///
    /// # 为什么要有它
    ///
    /// 关窗守卫靠 `segments_dirty` 判断「还有没有没写的东西」。异步派发之后只清脏标记是
    /// 不够的：那份快照**已经在后台了**，但还没落库。若此刻正赶上退出，重建窗口 / 进程
    /// 收尾都可能抢在写线程之前跑完，改动就丢了。所以派发路径保留脏标记语义的同时，用这个
    /// 计数暴露「后台还有几份在飞」——`flush_segments_if_dirty` 会先把它们全部等回来
    /// （带上限）再自己写，于是「写完才返回」的旧语义对调用点仍然成立。
    segments_write_pending: Arc<AtomicUsize>,

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

    /// 「修复常见错误」勾选的规则（转写页质检卡片用）。
    ///
    /// 只活在会话里、不写进 `config.toml`：默认值就是四类全开，用户下次开程序看到
    /// 的仍是同一套默认；而把一次临时试勾落进配置，会让它在下一次转写里悄悄生效
    /// ——用户会看到「嗯」莫名其妙少了一个，却找不到是谁干的。
    pub cleanup_rules: crate::subtitle::cleanup::CleanupRules,
    /// 上一次修复的结果提示（「已修复 12 处」）。改勾选或下一次修复时刷新。
    pub cleanup_note: Option<String>,

    // 字幕翻译相关状态
    pub is_translating: bool,
    pub translate_progress: f32,
    pub translate_status_msg: String,
    pub translate_target_lang: String,
    /// 翻译引擎档位：本地 Qwen 离线 / 在线 OpenAI 兼容 API。
    /// 与 `config.toml` 的 `[translate].mode` 双向同步，UI 切换后立即落盘。
    pub translate_mode: TranslateMode,
    /// 「选句翻译」模式：`None` = 关闭（整篇翻译）；`Some(set)` = 开启，`set` 是
    /// 用户在下方的字幕清单里勾选的句子 `index`。
    ///
    /// 为什么要它：整片几百句而只想重译其中某几句时（机翻只错了那几句、或只想
    /// 先试译一段看模型效果），此前只有「全部翻译」一条路——在线接口按 token
    /// 计费，重译一遍整片既慢又烧钱。勾选后点「开始翻译」只发送这几句。
    pub translate_pick: Option<std::collections::BTreeSet<i64>>,

    // 硬件与模型资源监控
    pub metrics: ResourceMetrics,

    // 剪辑校对工作台状态 (主界面)
    pub active_tab: WorkspaceTab,
    pub current_time: f64,
    pub total_duration: f64,
    pub is_playing: bool,
    pub selected_segment_index: Option<usize>,
    pub preview_frame_path: Option<PathBuf>,
    /// 当前定位帧**自己解码**好的图像（BGRA，可直接当 GPU 纹理上传）。
    ///
    /// 与 `preview_frame_path` 并存：解码成功就走这一张（渲染时零解码、零 GPUI 资产缓存），
    /// 解码失败回退到按路径交给 GPUI 加载。解码在后台抽帧线程里完成，不占渲染线程。
    /// 为什么不直接用 `img(路径)`：见 `ui::editor::render_monitor_picture` 的分支说明。
    pub preview_frame_image: Option<Arc<gpui::RenderImage>>,
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
    /// 最新版本的完整 Release 元数据（版本、更新日志、大小、直链等）
    pub update_release_info: Option<crate::utils::update_check::LatestRelease>,
    /// 是否正在检查更新（防连点）
    pub update_check_busy: bool,
    /// 更新包直接下载目标：(download_url, asset_name, size)
    pub update_download_target: Option<(String, String, u64)>,
    /// 是否正在下载更新安装包
    pub update_download_busy: bool,
    /// 更新下载进度 0.0 ~ 1.0
    pub update_download_progress: f32,
    /// 更新下载状态文案或提示
    pub update_download_status: Option<String>,
    /// 已下载的新版本文件路径
    pub update_downloaded_path: Option<PathBuf>,

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

    /// 最近一次配置落盘失败的原因（`None` = 一切正常）。
    ///
    /// 与 [`Self::db_write_error`] 同型：设置页 / 性能页 / 字幕样式滑条的每一次
    /// 改动都会写 `config.toml`，而此前这些写盘一律 `let _ = ...` 吞错。保存失败
    /// （磁盘满、目录只读、`config.toml` 被别的程序占用）完全静默——用户改完设置、
    /// 重启、发现设置又变回去了，却没有任何线索。这里把失败挂到可见提示条上。
    pub config_write_error: Option<String>,

    /// 拖动类控件（滑条轨道 / 调色轮）的落盘闸门。
    ///
    /// 这类控件的 `on_mouse_move` 会在拖动时连续回调，若每次都走 [`Self::persist_config`]
    /// 就是一次同步磁盘写——一次拖动能在 UI 线程上打出几十上百次写，既卡顿又刷盘，
    /// 而中间值用户根本用不到。约定：按下/开始拖动 [`Self::begin_config_drag`] 打开闸门，
    /// 拖动中 `persist_config` 只把改动留在内存，松手 [`Self::end_config_drag`] 一次性落盘。
    ///
    /// 注意闸门**只**影响 `persist_config`，不影响各方法对内存 / 管线的即时更新
    /// （拖动中的实时预览照常）。
    config_drag_active: bool,

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

/// 从字幕内容判断「当前视频语言」，返回目标语言清单里的显示名。
///
/// 判定顺序刻意如此：先问 [`crate::subtitle::is_source_chinese`]（它还带假名/谚文
/// 反查，能挡住「转写模型把日语误标成 zh」），再按字符特征找日语/韩语，最后才
/// 落到拉丁字母（英语）。认不出来时返回 `None`——调用方保持当前目标语言不变，
/// 绝不瞎猜一个语言把用户带偏。
pub fn source_language_label(segments: &[Segment]) -> Option<&'static str> {
    if segments.is_empty() {
        return None;
    }
    if crate::subtitle::is_source_chinese(segments) {
        return Some("简体中文");
    }
    let sample: String = segments
        .iter()
        .take(40)
        .map(|seg| seg.translate_source())
        .collect();
    if crate::subtitle::contains_japanese_kana(&sample) {
        Some("日本語")
    } else if crate::subtitle::contains_korean_hangul(&sample) {
        Some("한국어")
    } else if sample.chars().any(|c| c.is_ascii_alphabetic()) {
        Some("English")
    } else {
        None
    }
}

impl AppState {
    /// 本机逻辑核心数。设置页滑条量程与线程数收敛共用同一来源，
    /// 避免 UI 显示的上限与实际生效值不一致。
    pub fn logical_cores() -> u32 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8) as u32
    }

    /// 把「转写线程数」收敛到 `[2, cores]`：设置页滑条量程与引擎都以这个区间为准，
    /// 收敛口径必须只此一处，避免界面与实际生效值打架。
    ///
    /// 用 `cores.max(2)` 兜底：单核机器/容器上 `available_parallelism()` 会返回 1，
    /// 若直接写 `requested.clamp(2, cores)` 就是 `clamp(2, 1)`——`min <= max` 断言
    /// 立即 panic（CPU 档切换曾因此崩在这一行）。
    pub fn clamp_threads_to_cores(requested: u32, cores: u32) -> u32 {
        requested.clamp(2, cores.max(2))
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
        // 读失败（库损坏自愈之后重建、外部工具占用等）不该带崩启动，但必须留日志：
        // 这里的空列表直接决定首屏历史库内容，静默吞错会让「库读不出来」看起来
        // 像「一条记录都没有」。
        let recent_tasks = db.list_recent_tasks(50).unwrap_or_else(|err| {
            tracing::warn!(error = %err, "启动时读取历史库列表失败，先以空列表启动");
            Vec::new()
        });
        let lang = config.pipeline.language.clone();
        let fmt = config.pipeline.output_format.clone();
        let polish = config.pipeline.enable_polish;
        let polish_mode = PolishMode::parse_label(&config.pipeline.polish_mode);
        let translate_mode = TranslateMode::parse_mode(&config.translate.mode);
        // 线程数按本机逻辑核心数收敛：既保证与设置页滑条量程一致，
        // 也避免把小于 8 的用户设置强行抬到 8 导致滑条取值无法持久化。
        let threads =
            Self::clamp_threads_to_cores(config.pipeline.whisper_threads, Self::logical_cores());
        // 选中模型以**配置里的路径**为准（不再「按文件名反推档位、认不出就静默
        // 回落 Balanced」——那会让配置里的自定义模型被悄悄换成内置小模型，
        // 界面与命令行也会因此跑不同的模型）。
        let selected_model_path = AppConfig::resolve_path(&config.paths.whisper_model);
        // 档位是路径的**派生元数据**（友好名 + 耗时估算），不是可用性判据。
        let whisper_model_tier = WhisperModelTier::from_model_path(&config.paths.whisper_model)
            .unwrap_or_else(|| {
                let size = std::fs::metadata(&selected_model_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                crate::utils::local_models::tier_for_unknown_size(size)
            });
        // 并行进程数交给管线（0 = 自动，按核数推导）
        pipeline.set_parallel_workers(config.pipeline.parallel_workers as usize);
        // 自动导出用的字幕样式交给管线：转写收尾的落盘拿不到 config，
        // 只能由这里在构造时把首份快照写进去（后续变更见 `save_subtitle_style`）。
        pipeline.set_subtitle_style(config.subtitle_style.clone());
        let ffmpeg_path = AppConfig::resolve_path(&config.paths.ffmpeg);
        // 硬解后端必须在这里接进代理链路：`with_profile` 会按厂商把
        // `hardware.hwaccel_backend()` 的结果固化进 `ProxyManager`；若用 `new`
        // 则代理永远纯软解，设置页的 `gpu.hwaccel_decode` 无法生效。
        let proxy_manager = Arc::new(ProxyManager::with_profile(&ffmpeg_path, &hardware));
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
        // 后台写线程与渲染线程之间的结果通道：发送端交给后台写线程，接收端装进
        // `BackgroundWrites`（见 `BackgroundWrites::drain_results`）。
        let (segments_write_tx, segments_write_results) = std::sync::mpsc::channel();
        // 后台写入子系统的唯一一份实例：派发 / 同步落库 / 后台过闸入口（转写收尾）
        // 三方共用，水位与串行闸才不会各算一套。
        let writes_arc = Arc::new(BackgroundWrites {
            submit: segments_write_tx,
            results: Mutex::new(segments_write_results),
            ..BackgroundWrites::default()
        });
        // 过闸入口自带一份库句柄（`Database` 内部是 `Arc<Mutex<Connection>>`，克隆只是
        // 多一个引用）：后台线程只拿它，不拿整个 `AppState`。
        let segments_write_gate = SegmentWriteGate {
            db: db.clone(),
            writes: Arc::clone(&writes_arc),
        };
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
            transcribe_started_at: None,
            language: lang,
            output_format: fmt,
            enable_polish: polish,
            polish_mode,
            whisper_threads: threads,
            selected_model_path,
            whisper_model_tier,
            cancel_requested: false,
            segments_dirty: false,
            segments_flush_due: None,
            active_task_id: None,
            segments_revision: 0,
            segments_writes: writes_arc.clone(),
            // 过闸入口与派发 / 同步写共用**同一个** `BackgroundWrites`（同一把写入闸、
            // 同一份按工程水位）：另起一份账本等于给它单开一条不受水位约束的写库通道，
            // 正是这次要堵的那类「旧内容覆盖新内容」。
            segments_write_gate,
            segments_write_pending: Arc::new(AtomicUsize::new(0)),
            undo_stack: Vec::new(),
            edit_log: crate::subtitle::audit::EditLog::new(),
            redo_stack: Vec::new(),
            undo_text_coalesce: None,
            cleanup_rules: crate::subtitle::cleanup::CleanupRules::default(),
            cleanup_note: None,
            is_translating: false,
            translate_progress: 0.0,
            translate_status_msg: String::new(),
            translate_target_lang,
            translate_mode,
            translate_pick: None,
            metrics: ResourceMetrics::default(),

            active_tab: WorkspaceTab::Editor, // 默认主界面为剪辑校对工作台
            current_time: 0.0,
            total_duration: 0.0,
            is_playing: false,
            selected_segment_index: None,
            preview_frame_path: None,
            preview_frame_image: None,
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
            update_release_info: None,
            update_check_busy: false,
            update_download_target: None,
            update_download_busy: false,
            update_download_progress: 0.0,
            update_download_status: None,
            update_downloaded_path: None,
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
            config_write_error: None,
            config_drag_active: false,
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

    /// 按**当前**硬件档案重算性能档位与推荐配置（不重新探测硬件）。
    ///
    /// # 为什么不在这里重新探测
    ///
    /// 这个函数的两个调用点都在「模型下载完成」之后（单个下载与批量下载各一处），
    /// 而下载模型不可能改变 CPU / 内存 / 显卡。`HardwareInfo::detect()` 却要新建一个
    /// wgpu Instance 并把所有 adapter 枚举一遍（启动注释里实测约 2.7s），而且它是跑在
    /// 渲染线程上的——用户侧看到的就是「下载完成那一刻界面僵住两三秒」。
    ///
    /// 真正的硬件探测入口只有启动时的 [`AppState::with_hardware`]：它走
    /// `HardwareInfo::detect_with_media_profile`，复用媒体管线选中的 adapter，
    /// 不额外枚举。要再探一次硬件，应该走那条路，而不是每次下载完都探。
    pub fn refresh_hardware_recommendation(&mut self) {
        self.performance_level = self.hardware_info.evaluate_performance();
        self.recommended_profile = crate::core::InferenceProfile::decide(
            self.performance_level,
            self.user_strategy,
            &self.hardware_info,
        );
    }

    /// 把当前配置落盘，并把失败原因记进 [`Self::config_write_error`]。
    ///
    /// # 为什么收成一个方法而不是到处 `let _ = self.config.save_to_file(...)`
    ///
    /// 设置页 / 性能页 / 字幕样式滑条 / 翻译设置等入口都会在改动后落盘，此前一律
    /// `let _ = ...` 吞掉错误。写盘失败（磁盘满、目录只读、`config.toml` 被别的
    /// 程序占用）于是完全静默：用户改完设置、重启、发现设置又变回去了，却没有任何
    /// 线索。这与字幕写库失败是同一类「用户以为保存了」的缺陷，处理方式也一致——
    /// 挂到跨页常驻的提示条上，**不重试**（失败通常是持久性原因，重试只会再失败）。
    pub(crate) fn persist_config(&mut self) {
        self.persist_config_to("config.toml");
    }

    /// 打开「拖动中」落盘闸门：此后到 [`Self::end_config_drag`] 之前，
    /// [`Self::persist_config`] 只把改动留在内存、不写盘。
    ///
    /// 幂等：已在拖动中时重复调用无副作用。
    pub(crate) fn begin_config_drag(&mut self) {
        self.config_drag_active = true;
    }

    /// 关闭闸门并补一次落盘：把整段拖动期间的改动一次性写进 `config.toml`。
    ///
    /// 幂等：没在拖动时是空操作，重复调用只会在第一次真正写盘（之后
    /// `save_to_file` 见内容未变也不再重写）。
    pub(crate) fn end_config_drag(&mut self) {
        self.end_config_drag_to("config.toml");
    }

    /// [`Self::end_config_drag`] 的可注入路径版本（单测用）。
    fn end_config_drag_to<P: AsRef<std::path::Path>>(&mut self, path: P) {
        if !self.config_drag_active {
            return;
        }
        self.config_drag_active = false;
        self.persist_config_to(path);
    }

    /// [`Self::persist_config`] 的可注入路径版本：便于单测用临时目录复现写盘失败，
    /// 而不必去动真正的 `config.toml`（跑测试时写它会覆盖用户配置）。
    ///
    /// 拖动闸门在这里生效：`config_drag_active` 为真时只把改动留在内存
    /// （见字段说明），松手由 [`Self::end_config_drag`] 统一补写。
    fn persist_config_to<P: AsRef<std::path::Path>>(&mut self, path: P) {
        if self.config_drag_active {
            return;
        }
        match self.config.save_to_file(path) {
            Ok(()) => self.config_write_error = None,
            Err(err) => {
                tracing::warn!(error = %err, "配置落盘失败，改动尚未持久化");
                self.config_write_error = Some(format!("设置保存失败：{err}"));
            }
        }
    }

    /// 切换 Whisper 模型档位并**落盘**（「按档位选内置模型」的语法糖入口，
    /// 转发给 [`Self::set_selected_model_path`]）。
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
        // 转发给路径版本：档位只是「选哪个内置预设」的语法糖，真正落盘与生效的是
        // 它解析出的那条路径。两条入口共用一套落盘 + 重算，避免出现
        // 「从档位进去的选中项不带 selected_model_path」这种半更新状态。
        let path = AppConfig::resolve_path(&tier.model_relative_path());
        self.set_selected_model_path(path);
    }

    /// 按**文件路径**选中一个模型并落盘（唯一真相是路径，不是档位）。
    ///
    /// 与 `set_whisper_model_tier` 的区别：那条走「档位 → 内置路径」，只能表达
    /// 8 个预设；这条接受任意路径，因此用户自备的模型也能被选中并真正加载。
    ///
    /// 落盘用 `local_models::to_config_path`：项目内的模型写成相对路径（留在可提交
    /// 的 `config.toml`），目录外的写成绝对路径（由配置层自动剥离到
    /// `config.local.toml`）。这样既支持任意位置，又不破坏「共享配置不含机器路径」
    /// 的既有约定。
    pub fn set_selected_model_path(&mut self, path: PathBuf) {
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        self.whisper_model_tier =
            WhisperModelTier::from_model_path(&file_name).unwrap_or_else(|| {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                crate::utils::local_models::tier_for_unknown_size(size)
            });
        self.config.paths.whisper_model = crate::utils::local_models::to_config_path(&path);
        self.selected_model_path = path;
        self.persist_config();
        // 选中项换了，「模型与组件」卡里 whisper 各档的就位判定要跟着重算
        // （`configured_path_for` 只认「配置指向的那个档位」）。
        self.refresh_model_presence();
    }

    /// 当前选中模型的展示名：命中预设用友好名，自备模型显示文件名。
    ///
    /// 界面一律用这个方法而不是 `whisper_model_tier.label()`：后者对自备模型会
    /// 显示成兜底档位的名字（用户明明选了 `my-q5_1.bin`，界面却写「Whisper
    /// Small-Q5」），把「你选的到底是哪个文件」这件事糊掉。
    pub fn selected_model_label(&self) -> String {
        let file_name = self
            .selected_model_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        match WhisperModelTier::from_model_path(file_name) {
            Some(tier) => tier.label().to_string(),
            // 认不出预设：如实显示文件名，让用户认得出自己放进去的是哪个
            None if file_name.is_empty() => "未选择模型".to_string(),
            None => file_name.to_string(),
        }
    }

    /// 切换硬件加速层级（GPU / CPU），联动推理配置并持久化落盘
    pub fn set_hardware_tier(&mut self, is_gpu: bool) {
        self.config.apply_hardware_tier(is_gpu);
        if is_gpu {
            self.whisper_model_tier = WhisperModelTier::TurboSpeed;
            self.gpu_mode = self.config.gpu.mode().to_string();
        } else {
            self.whisper_model_tier = WhisperModelTier::Balanced;
            self.gpu_mode = "cpu".to_string();
            let cores = Self::logical_cores();
            self.whisper_threads =
                Self::clamp_threads_to_cores(self.config.pipeline.whisper_threads, cores);
        }
        self.persist_config();
        self.refresh_model_presence();
    }

    /// 首次启动引导框的「稍后再说」：只把向导标记为已完成，**不改任何检测结果**。
    ///
    /// 为什么单独一个入口而不是让界面直接写 `config.gpu.hardware_init_completed`：
    /// 那两个选择按钮走的是 `set_hardware_tier`，它除了置完成标记还会套用一整套餐
    /// （推理策略、模型路径、线程数、落盘）。「稍后再说」要的只是**别再弹这个框**，
    /// 不该顺手改掉用户还没表态的硬件档位——否则用户下次启动会发现自己被默许了一个
    /// 从没选过的模式。
    ///
    /// 仍然要落盘：否则每次启动都会再弹一次，等于没有关闭入口。
    pub(crate) fn defer_hardware_setup(&mut self) {
        self.defer_hardware_setup_to("config.toml");
    }

    /// [`Self::defer_hardware_setup`] 的可注入路径版本（单测用）。
    ///
    /// 与 `persist_config_to` 同一套路：单测跑 `defer_hardware_setup` 时不能去写仓库根
    /// 的 `config.toml`（那会覆盖开发机上的真实配置）。
    fn defer_hardware_setup_to<P: AsRef<std::path::Path>>(&mut self, path: P) {
        if self.config.gpu.hardware_init_completed {
            return;
        }
        self.config.gpu.hardware_init_completed = true;
        self.persist_config_to(path);
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
        self.persist_config();
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

        self.persist_config();
    }

    /// SenseVoice 原生自带高精度标点与数字规范化（ITN），管线会整段跳过 CT-Punc
    /// 阶段（`pipeline.rs` 里 `is_sensevoice && mode == "punc"` 分支直接进
    /// 「原生标点」瞬时完成）。此时界面上那个「极速标点」档位是**无操作**的：
    /// 用户以为自己选了一个处理步骤，实际什么都没发生。
    ///
    /// 返回 `false` 时界面应把该档位标注为「原生自带 / 不适用」，而不是让它看起来
    /// 和其它档位一样是个生效的选项。
    pub fn punc_polish_is_noop(&self) -> bool {
        self.whisper_model_tier == WhisperModelTier::SenseVoice
            && self.polish_mode == PolishMode::PuncFast
    }

    /// 设置页 / 主界面调整字幕样式后落盘，避免重启丢失
    pub fn save_subtitle_style(&mut self) {
        // 同步一份给管线：转写收尾的自动导出没有别的途径拿到样式，
        // 而这里正是「样式刚被改过」的唯一收口（滑条 / 预设 / 预览框宽都走它）。
        self.pipeline
            .set_subtitle_style(self.config.subtitle_style.clone());
        self.persist_config();
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
            self.persist_config();
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
    pub fn save_translate_config(&mut self) {
        self.persist_config();
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

    /// 把术语表写到软件根目录下的 `glossary.txt` 并返回路径，供外部编辑器（记事本）打开。
    pub fn write_glossary_temp_file(&self) -> std::io::Result<std::path::PathBuf> {
        let path = crate::utils::AppConfig::resolve_path("glossary.txt");
        let header = "# Voice2Word 术语表：每行一条「原文=译文」（也支持 -> / → / : 分隔）\n\
                     # 以 # 开头的是注释。保存后回到程序点「应用术语表」。\n\n";
        std::fs::write(&path, format!("{header}{}", self.config.translate.glossary))?;
        Ok(path)
    }

    /// 从 `glossary.txt` 读回术语表（跳过自动写入的注释头），落盘并返回生效条数。
    pub fn reload_glossary_from_temp_file(&mut self) -> std::io::Result<usize> {
        let path = crate::utils::AppConfig::resolve_path("glossary.txt");
        let text = std::fs::read_to_string(&path)?;
        self.config.translate.glossary = text;
        self.save_translate_config();
        Ok(self.config.translate.glossary_entries().len())
    }

    /// 从 `.txt` / `.csv` 导入术语表并与现有表合并（同名以导入的为准）。
    ///
    /// 解析规则见 `utils::glossary`：`.csv`（不分大小写）走 `glossary::from_csv`，
    /// 其余扩展名一律按「一行一条 `原文=译文`」走 `glossary::parse`。合并用
    /// `glossary::merge`（`incoming` 覆盖同名、忽略大小写、空来源跳过），结果以
    /// `glossary::to_text` 写回**当前的术语表字段** `config.translate.glossary`，
    /// 再走既有落盘入口 [`Self::persist_config_to`]。
    ///
    /// 返回 `(新增, 覆盖, 未变)`——三个数都来自 `glossary::MergeReport`，界面直接引用
    /// 即可，不要自己再数一遍（两处口径迟早分叉）。
    pub fn import_glossary_file(
        &mut self,
        path: &std::path::Path,
    ) -> std::io::Result<(usize, usize, usize)> {
        self.import_glossary_file_to(path, "config.toml")
    }

    /// [`Self::import_glossary_file`] 的可注入落盘路径版本（单测用）。
    ///
    /// 与 [`Self::persist_config_to`] 同一套路：单测调用导入时不能去写仓库根的
    /// `config.toml`（那会覆盖开发机上的真实配置）。
    fn import_glossary_file_to(
        &mut self,
        path: &std::path::Path,
        config_path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<(usize, usize, usize)> {
        let text = std::fs::read_to_string(path)?;
        // 表格软件「另存为 UTF-8」几乎都会带 BOM；这里统一吃掉，`from_csv` 自己也认。
        let body = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
        let is_csv = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"));
        let incoming = if is_csv {
            crate::utils::glossary::from_csv(body)
        } else {
            crate::utils::glossary::parse(body)
        };
        // base 取现有那份文本的解析结果：合并发生在**结构层**，文本只是存储表示。
        let base = crate::utils::glossary::parse(&self.config.translate.glossary);
        let (merged, report) = crate::utils::glossary::merge(&base, &incoming);
        self.config.translate.glossary = crate::utils::glossary::to_text(&merged);
        self.persist_config_to(config_path);
        Ok((report.added, report.overwritten, report.unchanged))
    }

    /// 把当前术语表导出成 CSV 文本（不落盘，交给调用方选路径）。
    ///
    /// 走 `glossary::to_csv`：首行表头 `source,target`，字段按最小 RFC4180 规则转义。
    /// 与 [`Self::import_glossary_file`] 共用同一套结构层，导出再导入逐条往返不丢内容。
    pub fn export_glossary_csv_text(&self) -> String {
        let entries = crate::utils::glossary::parse(&self.config.translate.glossary);
        crate::utils::glossary::to_csv(&entries)
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
                    // 先进语言包，再投影成「当前展示」：换目标语言重译时，旧语言的
                    // 那一份留在 `translations` 里，用户随时能在「字幕语言包」切回去。
                    if let (Some(lang), Some(text)) =
                        (src.translation_lang.as_deref(), src.translation.as_deref())
                    {
                        seg.set_translation_pack(lang, text);
                    }
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

    /// 这卷字幕里已经存在的「语言包」清单：`(语言名, 句数)`，按句数降序。
    ///
    /// 只统计**非空**译文。老工程没有语言包字段（只在 `translation` 里存了一份），
    /// 这里把它当作「只有这一种语言的包」兼容进来，不必等用户重译一遍。
    pub fn translation_packs(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for seg in &self.segments {
            for (lang, text) in &seg.translations {
                if !text.trim().is_empty() {
                    *counts.entry(lang.clone()).or_insert(0) += 1;
                }
            }
            if let (Some(lang), Some(text)) = (
                seg.translation_lang.as_deref(),
                seg.translation.as_deref(),
            ) {
                if !text.trim().is_empty() && !seg.translations.contains_key(lang) {
                    *counts.entry(lang.to_string()).or_insert(0) += 1;
                }
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// 当前正被展示的语言包（`None` = 只显示原文）。
    ///
    /// 取「句数最多的那个语言」：混排数据（半篇英文半篇日语）下没有唯一答案，
    /// 但面板上高亮哪个必须有个确定值，否则每次重绘都可能换一个。
    pub fn current_display_pack(&self) -> Option<String> {
        let mut counts: std::collections::BTreeMap<&str, usize> =
            std::collections::BTreeMap::new();
        for seg in &self.segments {
            if seg.has_translation() {
                if let Some(lang) = seg.translation_lang.as_deref() {
                    *counts.entry(lang).or_insert(0) += 1;
                }
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(lang, _)| lang.to_string())
    }

    /// 切换「当前展示 / 导出」的语言包：`Some(lang)` 展示该语言，`None` 只显示原文。
    ///
    /// 只改 `translation` / `translation_lang`——预览、双语导出、翻译统计全认这两个
    /// 字段，所以切语言包不用改下游一行；语言包本身一份都不会丢。
    pub fn apply_translation_pack(&mut self, lang: Option<&str>) {
        for seg in self.segments.iter_mut() {
            match lang {
                Some(l) => {
                    // 这一句没有该语言的包：清掉展示，避免留下上一门语言的残余。
                    if !seg.show_translation_pack(l) {
                        seg.show_source_only();
                    }
                }
                None => seg.show_source_only(),
            }
        }
        self.bump_segments_revision();
        self.segments_dirty = true;
        self.defer_segments_flush();
    }

    /// 切换「选句翻译」模式：关闭 → 开启（清空上次勾选）；开启 → 关闭。
    ///
    /// 开启时**必须**清空上一次的勾选：句子号是按文档编的，沿用上一个工程的
    /// 勾选会静默译错别的句子。
    pub fn toggle_translate_pick(&mut self) {
        self.translate_pick = match self.translate_pick.take() {
            Some(_) => None,
            None => Some(std::collections::BTreeSet::new()),
        };
    }

    /// 选句模式是否开启。
    pub fn is_picking_translate(&self) -> bool {
        self.translate_pick.is_some()
    }

    /// 某一句是否已被勾选（选句模式关闭时恒为 `false`）。
    pub fn is_translate_picked(&self, index: i64) -> bool {
        self.translate_pick
            .as_ref()
            .is_some_and(|set| set.contains(&index))
    }

    /// 勾选 / 取消一句；未处于选句模式时无副作用。
    pub fn toggle_translate_pick_one(&mut self, index: i64) {
        if let Some(set) = self.translate_pick.as_mut() {
            if !set.remove(&index) {
                set.insert(index);
            }
        }
    }

    /// 已勾选的句数（选句模式关闭时为 0）。
    pub fn translate_pick_count(&self) -> usize {
        self.translate_pick.as_ref().map_or(0, |set| set.len())
    }

    /// 把目标语言改成「当前视频语言」。
    ///
    /// 用户明确要求：翻译语言下拉框的默认选中项是**当前视频的语言**，而不是
    /// `config.toml` 里上次残留的那个。否则日语视频配上残留的「日本語」目标，
    /// 点「开始翻译」等于日语译日语（引擎会判定为复制并整片失败）。
    pub fn adopt_source_language_for_translation(&mut self) {
        // 换文档必须退出选句模式：勾选的是**上一份文档**的句子号，留着它下次
        // 点「开始翻译」就会去译这份文档里同号的、完全不相干的句子。
        self.translate_pick = None;
        let Some(lang) = source_language_label(&self.segments) else {
            return;
        };
        if self.translate_target_lang != lang {
            self.set_translate_target_lang(lang);
        }
    }

    /// 目标语言是否恰好就是这个视频本身的语言（此时翻译没有意义）。
    pub fn translate_target_equals_source(&self) -> bool {
        source_language_label(&self.segments).is_some_and(|lang| lang == self.translate_target_lang)
    }
    pub fn set_selected_file(&mut self, path: PathBuf) {
        self.selected_file = Some(path);
    }

    // ─────────── 阶段产物目录（GUI 断点续跑）───────────

    /// 阶段产物根目录名（与 CLI 的 `resolve_artifact_root` 同一约定）。
    const STAGE_DIR_NAME: &str = ".voice2word-stages";

    /// 同一个产物根下最多保留几个子目录（按 mtime 从新到旧），更旧的在开工时清掉。
    ///
    /// 缓存是「跑崩了不用从头来」的兜底，不是资产；留最近几支片子的中间结果足够用，
    /// 无上限地堆积只会把素材目录越撑越大。
    const STAGE_KEEP_RECENT: usize = 8;

    /// `媒体文件 → 阶段产物目录`：`<文件所在目录>/.voice2word-stages/<文件 stem>`。
    ///
    /// 口径与 CLI 的 `resolve_artifact_root` + `file_stem` **逐字一致**（见 `cli.rs`）：
    /// 桌面端与命令行跑同一支片子时必须命中同一份缓存，绝不能各写一套——两套口径意味着
    /// 同一个文件在 CLI 能续跑、在界面里却从头再来。取不到 stem 时回落 `"output"`，
    /// 无父目录时回落当前目录 `"."`，同样与 CLI 一致。
    pub fn stage_artifact_dir(media: &std::path::Path) -> std::path::PathBuf {
        let stem = media
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        let base = match media.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        base.join(Self::STAGE_DIR_NAME).join(stem)
    }

    /// 为即将开工的媒体铺好阶段产物目录并挂到管线（桌面端断点续跑的入口）。
    ///
    /// # 三条口径
    ///
    /// - **只在真正开工时创建**：本方法由开工路径调用；程序启动、仅仅在文件对话框里
    ///   选一圈文件都**不碰磁盘**，否则用户每浏览一次素材就在每个目录里撒一个空目录。
    /// - **换文件不读别人的产物**：目录按「文件 stem」分家；即使两支片子同名共用一个根，
    ///   管线读取时还有归属信封（`core::pipeline` 的 `StageArtifacts::owned_by`）核对
    ///   产出它的输入媒体，不符就当没有产物重算。这里只要保证传对目录。
    /// - **旧产物会被清掉**：同根下按 mtime 只保留最近 [`Self::STAGE_KEEP_RECENT`] 个
    ///   子目录，更旧的整体删除（best-effort，失败只记日志，绝不让开工失败）。
    ///
    /// # 回退开关
    ///
    /// 「目录不存在就不续跑」即是天然开关：只要管线 `artifact_dir` 还是 `None`，落盘与
    /// 续跑都关闭，行为与历史版本逐字一致。本轮**不改 `config.rs`**，因此不做持久化开关；
    /// 要让某次转写不续跑，删掉该目录或用 [`Self::disable_stage_artifacts`]。
    ///
    /// `media` 的父目录必须真实存在（GUI 里是文件对话框选出的绝对路径）。测试与
    /// 相对路径（如 `"a.mp4"`）会在这里被挡下，避免在仓库根散落 `.voice2word-stages`。
    pub fn prepare_stage_artifacts(
        &self,
        media: &std::path::Path,
    ) -> std::io::Result<std::path::PathBuf> {
        let Some(parent) = media
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "媒体路径没有所在目录，无法放置阶段产物: {}",
                    media.display()
                ),
            ));
        };
        if !parent.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("媒体所在目录不存在: {}", parent.display()),
            ));
        }
        let dir = Self::stage_artifact_dir(media);
        if let Some(root) = dir.parent() {
            prune_stale_stage_dirs(root, Self::STAGE_KEEP_RECENT);
        }
        std::fs::create_dir_all(&dir)?;
        self.pipeline.set_artifact_dir(Some(dir.clone()));
        Ok(dir)
    }

    /// 关闭阶段产物落盘与断点续跑（管线行为回到历史默认：不写、不读）。
    pub fn disable_stage_artifacts(&self) {
        self.pipeline.set_artifact_dir(None);
    }

    /// 当前生效的阶段产物目录（`None` = 未开启落盘 / 续跑）。
    pub fn stage_artifacts_dir(&self) -> Option<std::path::PathBuf> {
        self.pipeline.artifact_dir()
    }

    /// 重新读取历史库列表并替换内存缓存。
    ///
    /// 仍然是同步入口：调用点散落在 `ui/actions.rs`、`ui/mod.rs`、
    /// `ui/views/*.rs`、`ui/components/navigation.rs`，本轮改动范围碰不到它们。
    /// 但阻塞 UI 的大头已在数据层消掉——`Database::list_recent_tasks` 现在是
    /// 纯 SELECT 三个物化列，不再对每条工程的 `segments_json` 跑 json_each /
    /// json_extract，50 条的代价从「解析几 MB JSON」降到「拷 50 行定长字段」。
    ///
    /// 若要彻底把查询挪出 UI 线程：`Database` 内部是 `Arc<Mutex<Connection>>`
    /// （Send + Sync），调用点可以 clone 一份交给 `cx.background_executor()`，
    /// 在后台跑 `list_recent_tasks(50)`，回到 UI 线程后把结果写回
    /// `recent_tasks`。这一步必须与调用点（上面列出的文件）一起改：把
    /// `refresh_recent_tasks()` 换成「异步发起 + 完成回调」，因此留到 UI 侧
    /// 那一轮统一迁移，本轮先把查询本身变便宜、并保证失败不再静默。
    pub fn refresh_recent_tasks(&mut self) {
        match self.db.list_recent_tasks(50) {
            Ok(tasks) => self.recent_tasks = tasks,
            Err(err) => tracing::warn!(error = %err, "刷新历史库列表失败，沿用上一次的列表"),
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
        // 时长回退必须取**优化后**的末句 end：`optimize_segments` 会平滑延展极短尾句
        // （最短可读 0.8s）、也会剔除无效/鬼影片段。此前用优化前的 `task.segments`
        // 取末句，`total_duration` 会比真实时间轴还短——`seek_to` 于是把播放指针夹在
        // 末尾之前，用户点时间轴最后一段跳不过去。与落库口径保持一致：
        // `db.update_task_segments` 写的就是 `MAX(duration, 末句 end)`。
        let timeline_end = self.segments.last().map(|s| s.end).unwrap_or(0.0);
        self.total_duration = task.duration.max(timeline_end);
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

    /// 删除历史任务记录（若被删的正是当前工作区工程，则静默补位至列表最新一条或彻底清空，不切换页面）。
    ///
    /// 返回删除是否成功。此前这里是 `let _ = self.db.delete_task(id)`，删库失败被
    /// 完全静默：界面照样刷新、照样提示「已删除该记录」，而记录其实还在库里，下次
    /// 刷新列表它又冒出来。失败时**不**做补位/清空——工程仍在，不该被切走。
    pub fn delete_task_record(&mut self, id: i64) -> anyhow::Result<()> {
        self.db.delete_task(id)?;
        self.refresh_recent_tasks();

        // 判定「被删的是不是当前工作区」必须用主键 `active_task_id`，不能比文件
        // 路径：同一路径允许多条历史记录（每次转写各插一行），按路径比较会把
        // 「删掉同路径的另一条老记录」也误判成删掉当前工程，把用户正在编辑的
        // 工作区无故切走；而且 `selected_file` 与 `active_task_id` 不一定同步。
        // `active_task_id` 由 `load_task_with` / `load_from_cache` 维护、
        // `clear_current_workspace` 清空，是权威来源。
        //
        // 注意：即便列表里找不到该 id（`refresh_recent_tasks` 后 recent_tasks 无此项），
        // 只要主键命中就照常补位/清空——库已删成功，工作区不能继续指向已消失的记录。
        if self.active_task_id == Some(id) {
            // `refresh_recent_tasks` 已把被删项移出，`first()` 即删后最新一条
            // （`list_recent_tasks` 按 id 倒序）。
            if let Some(next_task) = self.recent_tasks.first().cloned() {
                // 静默补位：只换数据，不把用户拽离视频库页面
                self.load_task_with(&next_task, false);
            } else {
                self.clear_current_workspace();
            }
        }
        Ok(())
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
        self.preview_frame_image = None;
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
        // 时长基准：优先工程记录，其次本次转写探测值。**不含**片段回退——
        // 片段末句要等 `optimize_segments` 跑完再取，否则会读到被平滑/剔除前的旧边界。
        let base_dur = if cached.duration > 0.0 {
            cached.duration
        } else if self.transcribe_duration > 0.0 {
            self.transcribe_duration
        } else {
            0.0
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
        // 与 `load_task_with` 同口径：取「基准时长」与优化后末句 end 的较大者。
        let timeline_end = self.segments.last().map(|s| s.end).unwrap_or(0.0);
        self.total_duration = base_dur.max(timeline_end);

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
        if added > 0 {
            // 入队即落盘：用户排好队就去干别的了，此刻不存的话一关窗就白排。
            self.persist_queue();
        }
        added
    }

    /// 批量队列的持久化路径（与 `config.toml` 同目录）。
    ///
    /// 为什么用 `resolve_path` 而不是相对当前目录：程序可能从开始菜单 / 快捷方式
    /// 启动，cwd 会变成 `C:\Windows\System32`，相对路径会把队列文件写到系统目录。
    pub fn queue_file_path() -> std::path::PathBuf {
        // 测试隔离：测试里改用**进程专属的临时目录**。
        //
        // 不加这一层的话，`cargo test` 会把这个文件写到开发机**真实的项目根**上：
        // 凡是用到 `persist_queue` 的单测（队列结算、进度定格等好几个）都会顺手
        // 把用户排好的队列覆盖成测试数据。这不是假设——本项目就发生过：跑完测试后
        // 项目根下的 `batch_queue.json` 里剩下的是测试用的 `a.mp4`，用户排的队没了。
        #[cfg(test)]
        {
            std::env::temp_dir()
                .join(format!("v2w_test_queue_{}", std::process::id()))
                .join(crate::utils::queue_store::QUEUE_FILE_NAME)
        }
        #[cfg(not(test))]
        {
            AppConfig::resolve_path(crate::utils::queue_store::QUEUE_FILE_NAME)
        }
    }

    /// 把当前队列落盘（原子写）。
    ///
    /// 队列是**易失状态**，所以这里刻意不返回错误、失败只记日志：一次写盘失败不该
    /// 打断用户正在做的事（他可能刚点了「开始全部」）。代价是极端情况下队列没存上，
    /// 那也比因为一个可选的辅助文件而报错强。
    pub fn persist_queue(&self) {
        let items: Vec<crate::utils::queue_store::StoredQueueItem> = self
            .batch_queue
            .iter()
            .map(|q| crate::utils::queue_store::StoredQueueItem {
                path: q.path.to_string_lossy().to_string(),
                duration: q.duration,
                status: match &q.state {
                    QueueState::Pending => "pending".to_string(),
                    QueueState::Running => "pending".to_string(),
                    QueueState::Done { .. } => "done".to_string(),
                    QueueState::Failed(_) => "failed".to_string(),
                },
            })
            .collect();
        let path = Self::queue_file_path();
        if let Err(e) = crate::utils::queue_store::save_queue(&path, &items) {
            tracing::warn!(error = %e, "批量队列落盘失败（不影响本次会话）");
        }
    }

    /// 启动时恢复上次的队列。
    ///
    /// `running` 状态**恢复为等待中**：上次退出时正在跑的那条要么已经跑完（DB 里
    /// 有记录，`find_cached_task` 会命中）、要么被中断，无论哪种都不该显示成
    /// 「转写中…」——那会让用户以为程序还在干活。
    pub fn restore_queue(&mut self) {
        let items = crate::utils::queue_store::load_queue(&Self::queue_file_path());
        if items.is_empty() {
            return;
        }
        self.batch_queue = items
            .into_iter()
            .map(|stored| {
                let mut item = QueueItem::new(std::path::PathBuf::from(&stored.path));
                item.duration = stored.duration;
                item.state = match stored.status.as_str() {
                    s if crate::utils::queue_store::is_finished(s) => {
                        // 句数未知（磁盘上只存了状态）：显示「已完成」，具体句数在
                        // 用户点开 / 重跑时才需要。写 0 而不是编一个数。
                        QueueState::Done { segments: 0 }
                    }
                    "failed" => QueueState::Failed("上次运行未完成".to_string()),
                    _ => QueueState::Pending,
                };
                item
            })
            .collect();
        tracing::info!(count = self.batch_queue.len(), "已恢复上次的批量队列");
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

    /// 把某条队列项标记为「转写中」，让队列卡片立刻有反馈。
    ///
    /// 顺带把本条进度清零：条目重跑（失败重排、或用户再点一次「开始全部」）时，
    /// 不清就会带着上一轮残留的百分比进新任务——第一批进度事件到达前它一直挂在
    /// 卡片上，看起来就像「刚开始就已经跑到 80%」。
    ///
    /// 真正开工：铺好这一条的阶段产物目录，桌面端批量跑崩了可从产物续跑。
    ///
    /// 放在这里的理由：这是**批量模式**里「开始跑某一条」的唯一状态入口（`ui/actions.rs`
    /// 的 `start_processing_for` 在标记转写中时调用）。单文件模式不走队列，由开工路径直接
    /// 调 [`Self::prepare_stage_artifacts`]（同一支片子的同一口径，桌面端与 CLI 共用缓存）。
    /// 目录建不起来只记一条日志：续跑是加速手段，不能反过来挡住转写。
    pub fn mark_queue_running(&mut self, idx: usize) {
        let path = match self.batch_queue.get_mut(idx) {
            Some(item) => {
                item.state = QueueState::Running;
                item.progress = 0.0;
                item.stage = None;
                item.path.clone()
            }
            None => return,
        };
        if let Err(err) = self.prepare_stage_artifacts(&path) {
            tracing::warn!(
                error = %err,
                path = %path.display(),
                "阶段产物目录不可用，本次不落盘 / 不续跑"
            );
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
        // 删条目同样落盘（否则下次启动它又回来了）。
        self.persist_queue();
        self.batch_active = match self.batch_active {
            // 被删的正是活动项（理论上不会发生，防御性处理）：没有归属了，作废
            Some(active) if active == idx => None,
            // 删的是它前面的条目：整条队列前移一格，指针跟着前移
            Some(active) if active > idx => Some(active - 1),
            other => other,
        };
        true
    }

    /// 把队列里的某一条搬到指定下标（`from` → `to`，都是**当前**队列的下标）。
    ///
    /// 只动顺序，不碰 [`Self::batch_running`] / [`Self::status`]：正在跑的那条换了位置
    /// 也还是它自己在跑，重排不该打断或重启任何一条转写。
    ///
    /// 关键在活动指针：搬迁会让**所有**下标失效，所以这里不假定下标不变，而是先记下
    /// `batch_active` 那条的**路径**，搬完再按路径查回它的新位置。少了这一步，
    /// [`Self::finish_active_queue_item`] 会把完成/失败结算给它的邻居，而真正在跑的
    /// 那条永远停在「转写中」。
    ///
    /// 返回是否真的动了：越界、`from == to`、空队列一律 `false` 且不改任何状态。
    pub fn move_queue_item(&mut self, from: usize, to: usize) -> bool {
        let len = self.batch_queue.len();
        if from >= len || to >= len || from == to {
            return false;
        }
        // 记路径而不是下标：搬完只有「同一条」这个身份还可靠。`batch_active` 越界
        // （理论上的脏值）时拿到 `None`，此时保持原值不动——顺手把一个本就无效的指针
        // 「修」成别的样子只会更糟。
        let active_path = self
            .batch_active
            .and_then(|idx| self.batch_queue.get(idx))
            .map(|item| item.path.clone());
        let item = self.batch_queue.remove(from);
        self.batch_queue.insert(to, item);
        if let Some(path) = active_path {
            self.batch_active = self.queue_index_of(&path);
        }
        // 顺序也要落盘：用户排好队才关窗，下次启动必须还是这个顺序。
        self.persist_queue();
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
                // 进度条定格：成功收到满格，失败保留出事那一刻的值不动
                // （失败信息本身走 `QueueState::Failed`，这里只负责不让进度条
                // 看起来还活着）。阶段名一律清掉：任务已经结束，再挂一个
                // 「语音识别」只会让人以为它还在跑。
                if outcome.is_ok() {
                    // 沿用「百分比」这套单位（面板也按 0~100 判满格），
                    // 不写 1.0：两处口径不一致时进度条会比文字状态早一帧消失。
                    item.progress = 100.0;
                }
                item.stage = None;
                item.state = match outcome {
                    Ok(segments) => QueueState::Done { segments },
                    Err(reason) => QueueState::Failed(reason),
                };
            }
            // 每条结算都落盘：批量跑几十条时用户随时可能关窗，进度必须留下，
            // 否则重启后「已完成」的条目又变回等待中，整批白跑。
            self.persist_queue();
        }
        // 无论有没有结算到具体条目（用户可能把正在跑的那条从队列里删了，
        // 于是 `batch_active` 已作废），这一条转写都结束了，状态都必须归位。
        // 只收「还在处理中」的状态：调用点可能已经先写入了更具体的失败原因
        // （如「未识别出任何有效字幕」），那类提示要留给用户看，不能被抹掉。
        if matches!(self.status, ProcessStatus::Processing { .. }) {
            self.status = ProcessStatus::Idle;
        }
    }

    /// 把队列里**路径匹配**的那一条结算为「已完成」。
    ///
    /// # 为什么需要它（这是一个真实的功能 bug 的修复）
    ///
    /// 队列卡片是可点的：[`Self::select_queue_item`] 会把该文件设成 `transcribe_file`。
    /// 用户点完队列卡片、再按「开始转写」，走的是**单文件入口**
    /// `MainWindow::start_processing`——那条路会显式把 `batch_running` 置 false
    /// （它的注释写明「单文件入口显式退出批量续跑语义」），于是收尾时
    /// `in_batch` 为假，[`Self::finish_active_queue_item`] 永远不会被调用。
    ///
    /// 结果：这一条其实已经转写完成、结果也已入库，队列里却一直停在「等待中」；
    /// 又因为入队时已按 `pending` 落过盘，**每次重启都会再冒出来**。用户看到的正是
    /// 「明明转写完了，队列里总挂着一条待转写」。
    ///
    /// 按**路径**匹配而不是按下标：单文件入口全程不碰 `batch_active`，且队列在
    /// 一次转写期间可能被增删（用户边跑边加片子），下标早就不可靠了。
    ///
    /// 已经结算过（`Done`）的条目直接返回 `false`：不重复写盘，也不改动
    /// 已有的真实句数。
    pub fn settle_queue_item_by_path(&mut self, path: &std::path::Path, segments: usize) -> bool {
        let key = path.to_string_lossy().to_lowercase();
        let Some(idx) = self
            .batch_queue
            .iter()
            .position(|i| i.path.to_string_lossy().to_lowercase() == key)
        else {
            return false;
        };
        if matches!(self.batch_queue[idx].state, QueueState::Done { .. }) {
            return false;
        }
        {
            let item = &mut self.batch_queue[idx];
            item.state = QueueState::Done { segments };
            // 与 \`finish_active_queue_item\` 同口径：进度条定格满格、阶段名清掉，
            // 避免卡片上还挂着「语音识别」让人以为它仍在跑。
            item.progress = 100.0;
            item.stage = None;
        }
        // 结算即落盘：与批量结算同一条规则，否则重启后这条又变回「等待中」。
        self.persist_queue();
        true
    }

    /// 把一次管线进度事件记到**当前正在跑的那一条**队列项上。
    ///
    /// 判据只用 `batch_active`：它就是「这一条」的唯一权威来源，进度事件里
    /// 不带文件名。`batch_active == None` 有两种情形都要**直接忽略**：
    /// 单文件转写、以及批量已收尾（`finish_batch_queue` 清掉指针之后）。
    /// 少了这层判断，单文件转写的百分比就会写进队列里某个「等待中」的条目，
    /// 批量模式连开都没开，卡片上却冒出进度。
    ///
    /// 阶段名沿用管线给的字符串（`PipelineEvent::Progress` 里的 `stage`），
    /// 这里不改写措辞：UI 换词就得跟着管线的阶段表改两处，迟早不同步。
    pub fn record_batch_progress(&mut self, stage: &str, progress: f64) {
        let Some(idx) = self.batch_active else {
            return;
        };
        let Some(item) = self.batch_queue.get_mut(idx) else {
            return;
        };
        // `abs()` 兜住上游万一发来的负值（管线目前只发 0.0~1.0 的比率），
        // 再换算成 0~100 的百分比。
        item.progress = (progress.abs() * 100.0).clamp(0.0, 100.0);
        item.stage = Some(stage.to_string());
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
        // 清空也要落盘：否则下次启动会把「已经清掉的队列」又恢复回来。
        self.persist_queue();
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
        let total_est = self.whisper_model_tier.estimate_seconds(
            dur,
            self.whisper_threads,
            self.hardware.use_gpu_pipeline(),
            self.enable_polish.then_some(self.polish_mode),
        );

        // 若处于转写进行中，根据启动时间与真实推流速率动态计算「剩余时间」
        if let ProcessStatus::Processing { .. } = self.status {
            if let Some(started) = self.transcribe_started_at {
                let elapsed = started.elapsed().as_secs_f64();
                let audio_ratio = if dur > 0.0 && self.streaming_current_sec > 0.0 {
                    (self.streaming_current_sec / dur).clamp(0.0, 1.0)
                } else {
                    0.0
                };

                // 若已转写出有效字幕（>= 2%）且已跑满 3 秒，按真实吞吐倍率预测剩余耗时
                if audio_ratio >= 0.02 && elapsed >= 3.0 {
                    let rate = audio_ratio / elapsed;
                    if rate > 0.0001 {
                        let rem_secs = ((1.0 - audio_ratio) / rate).clamp(1.0, 86400.0);
                        return format!("剩余 {}", WhisperModelTier::format_eta(rem_secs));
                    }
                }

                // 刚启动或首句尚未吐出时，用初始预估减去已耗时，实现平滑真实倒计时
                let rem_secs = (total_est - elapsed).max(3.0);
                return format!("剩余 {}", WhisperModelTier::format_eta(rem_secs));
            }
        }

        WhisperModelTier::format_eta(total_est)
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
        // 切句前把上一句的编辑登记为「稍后落库」：逐句跳转时每跳一次都序列化整份工程
        // 太贵（5000 句约 10~30ms），写库交给停手 1.2s 后的定时器
        // （见 `SEGMENTS_FLUSH_IDLE`）。关窗 / 换工程仍会立刻写，不会丢。
        self.defer_segments_flush();
        // 换了一句就是另一段编辑会话，之前那句的文本合并到此为止
        self.undo_text_coalesce = None;
        self.selected_segment_index = Some(index);
        if let Some(seg) = self.segments.iter().find(|s| s.index == index) {
            self.editing_text = seg.display_text().to_string();
            self.current_time = seg.start;
        }
    }

    /// 让「选中句 / 编辑缓冲 / 画面字幕」按**同一个判据**对齐到 `current_time`。
    ///
    /// 返回选中句是否发生了变化。
    ///
    /// # 为什么必须有这个唯一入口
    ///
    /// 监视器画面上压的字幕走 [`Self::get_active_segment`]（按 `current_time` 现算，
    /// 半开区间 `[start, end)`）；而右侧编辑卡里的正文走 `editing_text`——它是
    /// 「上一次选中某句时缓存下来的副本」。两者是**两个独立状态**：
    /// - 播放时钟每 40ms 推进 `current_time`，原先只把 `selected_segment_index`
    ///   改成命中的新句，**没有**同步 `editing_text`；
    /// - 于是一旦播到第 N 句，画面上已经是第 N 句的字幕，右侧编辑框却还停在第 N-2 句，
    ///   看起来就是「视频字幕和右边字幕对不上」。
    ///
    /// 本函数把这处归一：一律用 `get_active_segment()` 这一条判据定位命中句，
    /// 命中且与当前选中不同时，同步 `selected_segment_index` 与 `editing_text`，
    /// 并结束上一段「连续文本编辑合并」会话（与 `select_segment` 口径一致）。
    ///
    /// `preserve_editing`：用户此刻正在编辑框里打字时为 `true`——只跟随选中项、
    /// **不覆盖** `editing_text`，否则播放推进会把用户敲到一半的字冲掉。失焦后由
    /// 下一次选中自然对齐。跳转 / 外部驱动时间一律传 `false`。
    ///
    /// 注意：只改「视图游标」，不写文档内容——不 flush、不记撤销，才能在 40ms
    /// 的播放回调里每帧零成本调用。
    pub fn sync_selection_to_time(&mut self, preserve_editing: bool) -> bool {
        let Some(seg) = self.get_active_segment() else {
            return false;
        };
        let seg_idx = seg.index;
        if self.selected_segment_index == Some(seg_idx) {
            return false;
        }
        let text = seg.display_text().to_string();
        self.selected_segment_index = Some(seg_idx);
        if !preserve_editing {
            self.editing_text = text;
            self.undo_text_coalesce = None;
        }
        true
    }

    /// 跳转播放指针时间
    pub fn seek_to(&mut self, time_sec: f64) {
        // 拖动时间轴时本函数每 40ms 就被调一次（`seek_by_mouse_x` 的节流），
        // 这里绝不能同步写库：登记完就走，写库留给停手后的定时器。
        self.defer_segments_flush();
        let max_dur = if self.total_duration > 0.0 {
            self.total_duration
        } else {
            self.segments.last().map(|s| s.end).unwrap_or(3600.0)
        };
        self.current_time = time_sec.clamp(0.0, max_dur);
        // 跳转后让「选中句 / 编辑缓冲」跟随落点，判据与画面字幕同源（见上）。
        self.sync_selection_to_time(false);
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

    /// 把后台写线程的回报收进 `db_write_error` / `segments_dirty`（在渲染线程上调）。
    ///
    /// # 后台错误怎么回到界面
    ///
    /// 后台线程**不碰 `AppState`**（渲染线程按帧借用它，共享可变状态等于给整个界面加锁），
    /// 只把 `SegmentWriteDone` 投进 `segments_write_rx`。这里是唯一的消费点，
    /// 也是唯一写这两个字段的地方，规则与同步版一致：
    ///
    /// - **失败一律不静默**：`Result::Err` 会被挂进 `db_write_error`（界面渲染成提示条），
    ///   并把 `segments_dirty` 置回（若这份快照仍是当前修订），下一次 flush 会重试；
    /// - **成功按序号收口**：只认「当前最新」那个写序号与修订号的结果，
    ///   更旧的一律丢弃（见 `drain_segment_writes` 与 `BackgroundWriteTracker`）；
    /// - 本次快照已经被更新的编辑取代（`revision` 变了）时，失败不再置脏：
    ///   更新的内容本来就要重新走一遍落库，置脏只会让旧快照再写一次。
    ///
    /// 调用点：定时去抖（`flush_segments_if_due_at`）、同步落库前（`flush_segments_if_dirty`）、
    /// 登记待落库时（`defer_segments_flush`：高频编辑也会顺手把结果收掉，
    /// 失败提示因此最迟在下一次交互就出现，不必等 1.2s 的定时器）。
    pub fn poll_background_writes(&mut self) {
        // 快照必须在排空之前取：`drain_segment_writes` 用它与每条回报的「派发时提示条」
        // 比对，判定这条回报是否已经被别的写库改动过（见该方法文档）。
        let pre_drain_error = self.db_write_error.clone();
        let done = self.segments_writes.drain_results();
        if done.is_empty() {
            return;
        }
        drain_segment_writes(
            &mut self.db_write_error,
            &mut self.segments_dirty,
            self.segments_revision,
            pre_drain_error.as_deref(),
            &done,
        );
    }

    /// 给后台线程的过闸写字幕入口（转写收尾回写上一个工程的脏字幕）。
    ///
    /// 只暴露 `&SegmentWriteGate`（内部是 `Database` 的克隆 + `Arc<BackgroundWrites>`）：
    /// 后台线程因此拿不到 `AppState` 的其它字段，也就没有「后台改界面状态」的可能。
    /// 调用点在派发前还要拿一个「当时的序号」（`next_write_seq`）一起带过去。
    pub(crate) fn segments_write_gate(&self) -> &SegmentWriteGate {
        &self.segments_write_gate
    }

    /// 下一个要发放的写序号（只读快照，不领号）。
    ///
    /// 给转写收尾这类「在后台线程上写库」的调用点用：它不能自己 `fetch_add`
    /// （`next_seq` 的单调性靠「领号只在渲染线程」这个前提），于是带上派发那一刻的
    /// 待发号。过闸判定是「同工程水位 >= 该号」，因此旧收尾写遇到更新的写只会被跳过。
    pub(crate) fn next_write_seq(&self) -> u64 {
        self.segments_writes.tracker.next_seq.load(Ordering::SeqCst)
    }

    /// 是否还有后台字幕写在飞（诊断 / 测试用；关窗守卫看的是 `segments_dirty`）。
    pub fn background_segments_write_pending(&self) -> usize {
        self.segments_write_pending.load(Ordering::SeqCst)
    }

    /// 给后台写线程的一致快照（当前主键 + 序列化好的整份字幕 + 修订号）。
    ///
    /// 这里只做一次 `Vec<Segment>` 克隆：共享借用会让编辑与后台写交错，不可取；
    /// 克隆是浅层复制，换来的是序列化 + sqlite UPDATE 全部离开渲染线程。
    ///
    /// 没有主键（尚未落库）时返回 `None`：退化成按路径更新会改掉同路径的其它历史记录。
    ///
    /// 序列化**不在这里做**：它和 sqlite UPDATE 一起交给后台线程（`write_segments_to_db`），
    /// 这里只截一份一致的字幕快照。这样 5000 句工程的序列化开销（JSON 几十毫秒量级）
    /// 也不再压渲染线程。
    fn segments_write_snapshot(&self) -> Option<SegmentWriteSnapshot> {
        let task_id = self.active_task_id?;
        let json = match serde_json::to_string(&self.segments) {
            Ok(json) => json,
            Err(err) => {
                // `Segment` 的字段都能被 `serde_json` 表达，这条路实际到不了。
                // 真到了说明内存里的内容已经不是本程序写进去的形状，重试一万次也还是
                // 同样的结果——只留日志，不派发（派发出去必然失败），也不动脏标记。
                tracing::warn!(error = %err, "字幕序列化失败，本次不派发后台写");
                return None;
            }
        };
        Some(SegmentWriteSnapshot {
            task_id,
            revision: self.segments_revision,
            json,
        })
    }

    /// 去抖路径专用：把当前快照**派发**给后台写线程（立刻返回，不阻塞渲染线程）。
    ///
    /// # 与同步版的分工
    ///
    /// - `flush_segments_if_dirty()` 给「换工程 / 导出 / 关窗守卫 / 翻译收尾」用：
    ///   返回时必须已经写进库（它还会把这里在飞的写等回来，见该函数与
    ///   `SEGMENTS_SYNC_FLUSH_WAIT`）；
    /// - 本函数给「停手 1.2s」的去抖路径用：只负责派发，
    ///   结果在 `poll_background_writes` 里收（写失败同样会挂 `db_write_error`）。
    ///
    /// # 脏标记：派发**不清**，落库被确认后才清
    ///
    /// 脏标记的语义是「当前内容还没落库」。派发只是把快照交给后台线程、还没写完，所以这里
    /// **保留** `segments_dirty`，等后台回报确认落库后在 `apply_segment_write_result` 里
    /// 清掉（只有 `revision` 仍是当前那一份才算）。
    ///
    /// # 为什么不能「派发即清」（本轮修的真实漏洞）
    ///
    /// 旧实现在派发成功那一刻就清脏，于是「改完 → 去抖派发 → 用户立刻关窗」这条链上，
    /// `flush_segments_before_close` 读到的 `segments_dirty` 是 `false`：它既不会做同步落库，
    /// 也不会因为在飞写的失败而拦住关窗。若那份在飞写恰好失败（磁盘满 / 库被锁），失败回报
    /// 要等下一次 poll 才被看到——而窗口已经关了，改动**静默丢失**。
    ///
    /// 不清脏会不会让定时器每个窗口重复派发同一份内容？不会：派发前已把
    /// `segments_flush_due` 置空，`flush_segments_if_due_at` 因此不会再次进来；这条路径
    /// 只由「一次新的编辑重新登记」触发。
    ///
    /// 返回「现在是否有一份后台写在飞」。调用方（`flush_segments_if_due_at`）据此决定要不要
    /// 重新登记 `segments_flush_due`：在飞时脏标记是刻意保留的，不能当成「派发失败」反复登记。
    pub fn flush_segments_if_dirty_async(&mut self) -> bool {
        if !self.segments_dirty {
            return false;
        }
        self.segments_flush_due = None;
        match self.segments_write_snapshot() {
            Some(snapshot) => match self.dispatch_segment_write(snapshot) {
                // 已交给后台：保留脏标记，等回报确认落库。
                SegmentDispatch::Dispatched => true,
                // 没起后台线程、但同步兜底已经写进去了：等同已保存，直接清脏。
                SegmentDispatch::Saved => {
                    self.segments_dirty = false;
                    false
                }
                // 兜底同步写也失败：错误已挂上提示条，脏标记保持（与旧实现一致）。
                SegmentDispatch::Failed => false,
            },
            // 没有主键：没有可写回的行，等同已保存（与同步版一致），脏标记直接清掉。
            None => {
                self.segments_dirty = false;
                false
            }
        }
    }

    /// 把一份字幕快照交给后台写线程；返回这一份的落库结局（见 [`SegmentDispatch`]）。
    ///
    /// # 防回退：派发前先按**同工程水位**检查
    ///
    /// 查水位、领序号都在 `db_write_writer` 锁里完成，和同步落库（也持有这把锁）
    /// 是互斥的，于是各种顺序都自洽：
    ///
    /// - 同步写先完成（水位被推高）：本快照的序号比水位旧，直接不派发——它写进去
    ///   就是**回退**，而库里已经有更新的版本；
    /// - 本快照先派发：序号比水位新，正常入队（后台线程写库前会再查一次水位，
    ///   挡住「排队等闸期间同步写 / 更新的后台写插进来」那种情形，见
    ///   [`write_snapshot_under_gate`]）。
    ///
    /// 派发失败（线程起不来）不静默：退回**过闸的同步写**，写不进去才挂提示条并把
    /// 脏标记放回，下一次去抖（或关窗守卫的同步落库）会重试。
    ///
    /// 返回三态而不是布尔：`Saved`（当场写完，调用方应清脏）与 `Dispatched`（交给后台、
    /// 尚未确认）对 `segments_dirty` 的处理相反，混成一个 `true` 就没法让脏标记钉在
    /// 「确认落库才清」上。
    fn dispatch_segment_write(&mut self, snapshot: SegmentWriteSnapshot) -> SegmentDispatch {
        let writes = Arc::clone(&self.segments_writes);
        // 派发侧与写线程各持一份：`pending` 归派发侧记账（见 `InFlightGuard`），
        // `worker_pending` 交给线程在收尾时 -1。失败分支没有线程，还得自己还回去。
        let pending = Arc::clone(&self.segments_write_pending);
        let worker_pending = Arc::clone(&self.segments_write_pending);
        // 写线程自己再拿一份 `Arc`：`writes` 会被 move 进闭包，派发侧的失败分支还要用。
        let worker_writes = Arc::clone(&self.segments_writes);
        let db = self.db.clone();
        // 快照会被 move 进闭包，失败分支只剩「当前渲染线程上的内容」可写；
        // 把主键先抄出来，兜底分支同样要按工程记水位。
        let task_id = snapshot.task_id;
        let seq = {
            let _guard = writes.lock_writer();
            let next = writes.tracker.next_seq.load(Ordering::SeqCst);
            if writes.tracker.applied_seq(task_id) >= next {
                // 同一工程已经有更新的内容落库：这份快照写进去就是回退。直接算
                // 「已保存」——它描述的内容本来就来自水位之前的状态。
                //
                // 老实说：在「领号只在渲染线程、且严格单调」的现状下这条判据**不会**
                // 触发（水位一定小于下一个待发号）。留着它是因为它是「同工程不许回退」
                // 这条规则的完整表述：以后若有人再加一条派发路径，这里就是派发侧唯一
                // 的拦截点；真正兜底的是落库时的 `write_snapshot_under_gate`。
                tracing::debug!(task_id, "同工程已有更新的写落库，放弃派发这次后台写");
                return SegmentDispatch::Saved;
            }
            let seq = writes.tracker.next_seq.fetch_add(1, Ordering::SeqCst);
            writes.in_flight_writes.fetch_add(1, Ordering::SeqCst);
            // 「在飞」从**派发**这一刻算起：线程还没开跑时结算方也不能漏掉它。
            pending.fetch_add(1, Ordering::SeqCst);
            seq
        };
        let done = SegmentWriteDone {
            seq,
            error_at_dispatch: self.db_write_error.clone(),
            revision: snapshot.revision,
            task_id: snapshot.task_id,
            result: Ok(()),
        };
        let spawn = std::thread::Builder::new()
            .name("voice2word-segments-write".to_string())
            .spawn(move || {
                // 计数器在闭包退出时自动 -1（`InFlightGuard` 的 Drop）：
                // 正常返回 / 提前放弃 / panic 三种结局都不会把它挂住。
                let in_flight = InFlightGuard(worker_pending);
                let write_guard = worker_writes.lock_writer();
                // 持闸写库：闸里一次完成「查同工程水位 -> 写 -> 记水位」，
                // 因此不可能出现「更旧的快照在更新的内容之后落地」这种回退。
                let result = write_snapshot_under_gate(&worker_writes, &db, &snapshot, seq)
                    .map(|_| ())
                    .map_err(|err| err.to_string());
                drop(write_guard);
                let sent = worker_writes
                    .submit
                    .send(SegmentWriteDone { result, ..done })
                    .is_ok();
                // 诊断计数器（在飞快照数）这次写已经结束，无论结果有没有送出去都 -1：
                // 它只用于观测，不能像 `pending` 那样把「结果已进通道」当成归零条件
                // ——那会让它随着使用时长单调上涨，变成一个骗人的数。
                worker_writes
                    .in_flight_writes
                    .fetch_sub(1, Ordering::SeqCst);
                if sent {
                    // 结果已进通道：这一份可以算「不在飞」了（同步落库等的是它落地）。
                    in_flight.disarm();
                }
            });
        match spawn {
            Ok(_handle) => SegmentDispatch::Dispatched,
            Err(spawn_err) => {
                // 线程都没起来：退回同步写（写的是渲染线程上这份最新内容）。
                // 退路也必须过闸、也要记水位——否则这次「成功」在账本上不存在，
                // 更旧的在飞快照醒来后照样能把新内容盖回去。
                tracing::warn!(error = %spawn_err, "后台写线程启动失败，改为同步写库");
                // 没有线程来收尾：派发时记的那两个「在飞」计数都由这里还回去，
                // 否则关窗守卫会一直以为有一份写没落地。
                pending.fetch_sub(1, Ordering::SeqCst);
                writes.in_flight_writes.fetch_sub(1, Ordering::SeqCst);
                let fallback = {
                    let _guard = writes.lock_writer();
                    if writes.tracker.applied_seq(task_id) >= seq {
                        Ok(())
                    } else {
                        match self.sync_segments_to_db() {
                            Ok(()) => {
                                writes.tracker.note_applied(task_id, seq);
                                Ok(())
                            }
                            Err(err) => Err(err),
                        }
                    }
                };
                match fallback {
                    // 兜底已经写进去了：脏标记交给调用方按「已保存」清掉，不再谎报失败。
                    Ok(()) => SegmentDispatch::Saved,
                    Err(err) => {
                        self.segments_dirty = true;
                        self.record_db_write_error(format!(
                            "字幕保存失败：{err}（后台写线程启动失败：{spawn_err}）"
                        ));
                        SegmentDispatch::Failed
                    }
                }
            }
        }
    }

    /// 将编辑产生的脏字幕落库（同步；幂等，未脏时零开销）
    ///
    /// 换工程 / 导出 / 关窗守卫 / 结构性编辑（拆分合并删除）都走这条**立刻写入**的路；
    /// 高频交互走 `AppState::defer_segments_flush`（迟到 1.2s 后由
    /// `flush_segments_if_dirty_async` 派发到后台线程）。
    ///
    /// # 与后台写的关系（旧语义必须保住）
    ///
    /// 调用点要的是「**返回时数据已经在库里**」。后台路径（去抖）只负责派发，返回时写
    /// 可能还在飞——所以这里先等它们落地（上限 `SEGMENTS_SYNC_FLUSH_WAIT`），再自己写
    /// 当前快照。
    ///
    /// 等待超时也**不会丢写、不会回退**：本次写和后台写共用同一把写入闸，且都按
    /// **同工程水位**收口（见 [`Self::sync_segments_gated`] 与 [`write_snapshot_under_gate`]）。
    /// 超时只决定「什么时候拿到闸」，不决定最终库里是谁的内容——拿到闸更晚的那一份
    /// 一定更新，更旧的快照醒来后会看到水位比自己新而直接放弃。
    ///
    /// 顺手作废「待落库」登记：登记留着的话，定时器稍后还会把同一份（或更旧的）内容再写一遍。
    pub fn flush_segments_if_dirty(&mut self) {
        // 先把后台的回报收进 `db_write_error`：另一条入口只是定时器去抖，界面上就指望
        // 这里把「后台写失败」变成可见提示。不收的话一次失败要等到下一次交互才显示。
        self.poll_background_writes();
        // 等待是**优化**，不是顺序保证：计数从派发那一刻算起（见 `InFlightGuard`），
        // 所以这里等的是「已派发还没收尾」的写；即便超过上限不再等，最终顺序仍由
        // 写入闸 + 同工程水位决定（旧快照醒来会看到水位比自己新而放弃）。
        let pending = Arc::clone(&self.segments_write_pending);
        if pending.load(Ordering::SeqCst) > 0 {
            let deadline = std::time::Instant::now() + SEGMENTS_SYNC_FLUSH_WAIT;
            while pending.load(Ordering::SeqCst) > 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let still = pending.load(Ordering::SeqCst);
            if still > 0 {
                tracing::warn!(
                    pending = still,
                    "后台字幕写未在期限内收尾，同步落库不再等它（写入闸会保证落地顺序）"
                );
            }
        }
        // 等完之后再收一次后台回报：等待期间到货的结局若留在通道里，`segments_dirty` 就还是
        // 「派发时保留」的旧值——成功不置回会让下面白写一遍，失败不置回更糟：这次「同步落库」
        // 会把一份其实没落盘的改动当成已保存放行（关窗守卫的盲区）。先收干净，再决定要不要自己写。
        self.poll_background_writes();
        self.segments_flush_due = None;
        if self.segments_dirty {
            self.segments_dirty = false;
            // 写库失败不能静默：用户以为改动已保存，重启后却发现全丢了。
            // 记进 `db_write_error`，界面把它渲染成可见的提示条。
            let result = match self.active_task_id {
                Some(task_id) => self.sync_segments_gated(task_id),
                // 没有主键（尚未落库）：没有可写回的行，等同已保存。
                None => Ok(()),
            };
            match result {
                Ok(()) => self.db_write_error = None,
                Err(err) => {
                    tracing::warn!(error = %err, "字幕写库失败，改动尚未持久化");
                    self.record_db_write_error(format!("字幕保存失败：{err}"));
                    // 写失败必须把脏标记重新置回：否则下面关窗守卫读到的
                    // `segments_dirty` 是「已保存」，直接放行关闭窗口，用户重启后
                    // 改动全丢（与 db_write_error 提示条同时存在，自相矛盾）。
                    // 保持脏 → 后续 flush 会重试，守卫也仍会拦截关闭。
                    self.segments_dirty = true;
                }
            }
        }
    }

    /// 同步落库的「过闸」版本：领号 -> 写 -> 记水位，全程持有写入闸。
    ///
    /// 为什么必须持闸写完：后台写线程也是在持闸期间写库的（见 `dispatch_segment_write`
    /// 里的 worker）。两者合起来让「谁更晚拿到闸」成为唯一顺序；配合同工程水位
    /// （[`BackgroundWriteTracker::applied`]），任意交错下库里留下的都是序号最大的那份，
    /// 不会出现「更旧的快照后落地」把新内容盖掉。
    fn sync_segments_gated(&self, task_id: i64) -> anyhow::Result<()> {
        let writes = &self.segments_writes;
        let _guard = writes.lock_writer();
        let seq = writes.tracker.next_seq.fetch_add(1, Ordering::SeqCst);
        if writes.tracker.applied_seq(task_id) >= seq {
            // 同一工程已经有更新的写落库（理论上到不了：同步写总是领最新的号），
            // 本次要写的内容反而更旧，放弃——算成功，绝不拿旧内容回退。
            tracing::debug!(task_id, seq, "同工程已有更新的写落库，放弃这次同步写");
            return Ok(());
        }
        // 写的是 `self.segments`（渲染线程上这份最新内容），与快照路径同源。
        self.sync_segments_to_db()?;
        // 记水位就够了：同工程里更早派发的回报会在 `drain_results` 看到
        // `applied > seq` 而被当成过期结论丢掉——用户刚存好的东西不该被一条
        // 迟到的旧失败说成「又坏了」。
        writes.tracker.note_applied(task_id, seq);
        Ok(())
    }

    /// 登记「稍后落库」：高频交互节点用它代替立刻写库（见 [`SEGMENTS_FLUSH_IDLE`]）。
    ///
    /// 不脏就不登记：空登记只会让定时器白白醒来查一次。
    ///
    /// 顺手收一次后台写的回报：编辑是最频繁的动作，失败提示因此最迟在下一次敲键 /
    /// 切句就出现，不必干等 1.2s 的定时器。
    pub fn defer_segments_flush(&mut self) {
        self.poll_background_writes();
        if self.segments_dirty {
            self.segments_flush_due = Some(std::time::Instant::now());
        }
    }

    /// 停手满 [`SEGMENTS_FLUSH_IDLE`] 就把待落库的改动交给后台写线程；
    /// 返回「这次真的派发了一次写」。
    ///
    /// # 为什么这里能返回 `true` 却还没写完
    ///
    /// 去抖路径的语义是「别再拖了、让它去写」，不是「已经写完」——写库本身
    /// （序列化 + sqlite UPDATE，5000 句约 10~30ms）已经挪到后台（见
    /// `flush_segments_if_dirty_async`）。返回值只用来驱动界面重绘。
    /// 需要「写完才继续」的调用点走的是同步版 [`Self::flush_segments_if_dirty`]。
    ///
    /// 写失败时后台回报会在 `poll_background_writes` 里把 `segments_dirty` 置回
    /// （提示条同时挂上 `db_write_error`）。
    ///
    /// # 失败**不会**被这条定时器自动重试（现状，别被旧注释骗了）
    ///
    /// 派发之后登记位已经被本函数清掉，而这里只有「这次没派发出去（序列化失败）」
    /// 才会重新登记——后台线程写失败**不会**重新登记。也就是说：一份内容写库失败后，
    /// 下一次重试发生在**用户下一次编辑**（`defer_segments_flush` 会重新登记）或
    /// 下一次同步落库（关窗守卫 / 换工程 / 导出）时。这是刻意的取舍：自动重试要
    /// 每秒重试一次，而提示条是按「；」追加的，持久性故障（磁盘满 / 库被外部进程锁住）
    /// 会把提示条刷成几百条重复文案，还会让渲染线程每分钟白付几十次整份序列化。
    /// 关窗守卫仍然拦着「脏着退出」，所以失败不会静默地丢数据。
    pub fn flush_segments_if_due(&mut self) -> bool {
        self.flush_segments_if_due_at(std::time::Instant::now())
    }

    /// [`Self::flush_segments_if_due`] 的可注入时钟版本（单测直接给时刻，不睡真实时间）。
    ///
    /// 返回值除了「这次真的派发了一次写」，还包括「后台回报刚刚改动了要显示的字段」
    /// （提示条 / 脏标记）：调用方（界面定时器）只按这个返回值决定要不要重绘，
    /// 而失败提示正是在这里被收进来的——不把这种变化报出去，用户不碰鼠标就看不到
    /// 「保存失败」的提示（状态早就更新了，界面却一直没重画）。
    pub fn flush_segments_if_due_at(&mut self, now: std::time::Instant) -> bool {
        let error_before_poll = self.db_write_error.clone();
        let dirty_before_poll = self.segments_dirty;
        // 到点与否都得先把后台回报收掉：否则失败置回的脏标记要等到下一个窗口才看得到，
        // 而那个窗口又可能因为「还没到点」直接返回，白拖一轮。
        self.poll_background_writes();
        let poll_changed_ui =
            self.db_write_error != error_before_poll || self.segments_dirty != dirty_before_poll;
        let Some(due) = self.segments_flush_due else {
            return poll_changed_ui;
        };
        if now.saturating_duration_since(due) < SEGMENTS_FLUSH_IDLE {
            return poll_changed_ui;
        }
        if !self.segments_dirty {
            // 期间已经被别的路（换工程 / 关窗 / 导出 / 后台写已完成）写过：登记作废，别一直挂着。
            self.segments_flush_due = None;
            return poll_changed_ui;
        }
        let dispatched = self.flush_segments_if_dirty_async();
        if self.segments_dirty && !dispatched {
            // 没派发出去（序列化失败 / 同步兜底也失败）：重新登记，下一个窗口再试。
            // `dispatched == true` 说明脏标记是「后台写在飞、还没确认落库」时被刻意保留的，
            // 不能当成失败反复登记（否则会退化成每 1.2s 重派发一次的刷屏重试）。
            self.segments_flush_due = Some(std::time::Instant::now());
        }
        true
    }

    /// 追加一条写库失败提示（`db_write_error`）。
    ///
    /// 已有提示时用「；」拼接而非覆盖：一次操作里可能先后失败两次（例如「上一个
    /// 工程的 flush 失败」+「当前转写结果入库失败」），只留最后一条会让用户以为
    /// 只有一处出了问题，去排查时漏掉另一条。成功落库时调用方仍会把它清成 `None`。
    pub fn record_db_write_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.db_write_error = Some(match self.db_write_error.take() {
            Some(prev) => format!("{prev}；{msg}"),
            None => msg,
        });
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
        // 重复译文会被持久化。切分策略直接复用自动拆分的
        // `subtitle::segment::split_following`，两边同一份实现：先按标点就近切、
        // 找不到标点再按比例硬切，**绝不让后半段落空**。
        let (trans_left, trans_right) = match orig.translation.as_deref() {
            Some(t) if !t.trim().is_empty() => {
                let (l, r) = crate::subtitle::segment::split_following(t, ratio);
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
            translations: Default::default(),
            polished: if !orig.polished.is_empty() {
                part2
            } else {
                String::new()
            },
            language: orig.language.clone(),
            confidence: orig.confidence,
            speaker: orig.speaker,
            speaker_name: orig.speaker_name.clone(),
            // 切分出的右半段暂不携带词级时间戳：拆分点未必落在词边界上，
            // 强行分配会造出跨段的错误词时间；留空由后续需要时再对齐。
            words: Vec::new(),
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

    /// 「修复常见错误」的预览：按当前勾选的规则能改多少处、分别是什么。
    ///
    /// 每帧现算而不是缓存：纯字符串扫描（一小时素材量级在毫秒以下），比再引一份
    /// 缓存与失效键便宜得多，也不会出现「预览是旧的、落地是新的」这种两张皮。
    pub fn cleanup_preview(&self) -> crate::subtitle::cleanup::CleanupPlan {
        crate::subtitle::cleanup::plan(&self.segments, self.cleanup_rules)
    }

    /// 勾选/取消一条修复规则。规则一变，上一条「已修复 N 处」的提示就成了旧结果，
    /// 继续挂着会把用户引到已经不成立的结论上，所以一起清掉。
    pub fn toggle_cleanup_rule(&mut self, rule: crate::subtitle::cleanup::CleanupRule) {
        self.cleanup_rules.toggle(rule);
        self.cleanup_note = None;
    }

    /// 一键修复常见错误（语气词 / 重复字词 / 听障标记 / 空白标点）。
    ///
    /// 没有可改之处时**不压撤销点**：空操作占掉一层 Ctrl+Z，用户按下去只会把上一次
    /// 真正的编辑退掉，看起来像「撤销坏了」。
    pub fn apply_cleanup(&mut self) -> crate::subtitle::cleanup::CleanupPlan {
        let plan = crate::subtitle::cleanup::plan(&self.segments, self.cleanup_rules);
        if plan.is_empty() {
            self.cleanup_note = Some(if self.cleanup_rules.any() {
                "按当前勾选的规则没有发现可修复的地方".to_string()
            } else {
                "先勾选至少一条规则".to_string()
            });
            return plan;
        }
        self.snapshot_for_undo();
        // 审计按规则记摘要，不逐句记：一次清洗动辄上百句，逐句写会把有界日志瞬间
        // 挤满，把它们之后真正需要追溯的人工编辑挤出去。
        for rule in crate::subtitle::cleanup::CleanupRule::ALL {
            let hits: Vec<&crate::subtitle::cleanup::CleanupHit> =
                plan.hits.iter().filter(|h| h.rule == rule).collect();
            let Some(first) = hits.first() else {
                continue;
            };
            let sample = format!(
                "{} → {}",
                audit_snippet(&first.before),
                audit_snippet(&first.after)
            );
            self.audit(
                crate::subtitle::audit::EditKind::Replace,
                first.index,
                format!("{} 处未修复（{}）", hits.len(), rule.label()),
                sample,
            );
        }
        crate::subtitle::cleanup::apply(&mut self.segments, self.cleanup_rules);
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        let emptied = plan.emptied_count();
        self.cleanup_note = Some(if emptied > 0 {
            format!(
                "已修复 {} 处（{emptied} 句只剩标记或停顿词，已清空，要不要删条由你决定）",
                plan.len()
            )
        } else {
            format!("已修复 {} 处，可 Ctrl+Z 撤销", plan.len())
        });
        plan
    }

    /// 阅读速度超限的句下标（上限见 [`crate::subtitle::DEFAULT_MAX_CPS`]）。
    ///
    /// 薄封装 [`crate::subtitle::cps_violations`]：可见字符数与统计面板同一口径，
    /// 上限非法 / 非正时按字符集分档回落，零长与倒挂句也入列。只读 `segments`。
    pub fn cps_violation_indices(&self, max_cps: f64) -> Vec<usize> {
        crate::subtitle::cps_violations(&self.segments, max_cps)
    }

    /// 句尾标点缺失的复核计划（只报告，不改文本）。
    ///
    /// 薄封装 [`crate::subtitle::plan_punctuation_fix`]：补标点必须猜语气，工具不替用户
    /// 改正文，这里只产出「哪几句值得看一眼」。只读 `segments`。
    pub fn punctuation_plan(&self) -> crate::subtitle::PunctuationPlan {
        crate::subtitle::plan_punctuation_fix(&self.segments)
    }

    /// 按 [`crate::subtitle::plan_cps_fix`] 延长过短句的终点（只动时间、不动文本）。
    ///
    /// 写回口径与 [`Self::apply_cleanup`] 逐条一致：有改动时先
    /// [`Self::snapshot_for_undo`] 压快照，再改 `segments`、置脏、递增修订号、落库；
    /// **无改动不压撤销点**（空操作占掉一层 Ctrl+Z 会让用户觉得「撤销坏了」）。
    ///
    /// `plan_cps_fix` 保证 `plan.times` 与输入等长同序，因此这里按下标逐条写回即可；
    /// 上限 / 时长下限的默认值分别为 [`crate::subtitle::DEFAULT_MAX_CPS`] 与
    /// [`crate::subtitle::MIN_EDIT_DUR`]，界面传参不要硬编码这两个数字。
    pub fn apply_cps_fix(&mut self, max_cps: f64, min_dur: f64) -> crate::subtitle::CpsFixPlan {
        let plan = crate::subtitle::plan_cps_fix(&self.segments, max_cps, min_dur);
        if !plan.has_changes() {
            return plan;
        }
        self.snapshot_for_undo();
        for (seg, (start, end)) in self.segments.iter_mut().zip(plan.times.iter()) {
            seg.start = *start;
            seg.end = *end;
        }
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        plan
    }

    /// 相邻重复句的预览（界面据此显示「N 组重复、可删 M 句」）。
    pub fn duplicate_cue_groups(&self) -> Vec<crate::subtitle::cleanup::DuplicateGroup> {
        crate::subtitle::cleanup::duplicate_groups(&self.segments)
    }

    /// 折叠相邻重复句：每组保留第一句并让它的结束时间盖住整组，其余删掉。
    ///
    /// 为什么交给用户点而不是自动做：真说过两遍同样的话（副歌、口头禅）与模型重复
    /// 出句在数据上长得一样，只差时间间隔；规则给出候选，删不删由人决定——所以
    /// 界面先显示组数与被删句数，让用户看清代价再点。
    pub fn fold_duplicate_segments(&mut self) -> usize {
        let groups = crate::subtitle::cleanup::duplicate_groups(&self.segments);
        if groups.is_empty() {
            self.cleanup_note = Some("没有发现相邻重复句".to_string());
            return 0;
        }
        // 折叠会让被删句之后的序号整体前移，所以先按「原表位置」记下两件事：选中句的
        // 位置，以及它被折进哪一组（选中句本身就是要被删掉的那句时）。不这么做，
        // 用户会发现「点了一下折叠，选中的句子悄悄换成了另一句」。
        let selected_index = self.selected_segment_index;
        let selected_pos =
            selected_index.and_then(|idx| self.segments.iter().position(|s| s.index == idx));
        let folded_owner_pos: Option<usize> = selected_index.and_then(|idx| {
            groups
                .iter()
                .find(|g| g.indices.iter().skip(1).any(|i| *i == idx))
                .and_then(|g| g.indices.first().copied())
                .and_then(|keep| self.segments.iter().position(|s| s.index == keep))
        });
        let removed: Vec<usize> = groups
            .iter()
            .flat_map(|g| g.indices.iter().skip(1).copied())
            .collect();
        let survivors: Vec<usize> = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| !removed.contains(&s.index))
            .map(|(pos, _)| pos)
            .collect();

        self.snapshot_for_undo();
        for group in &groups {
            let index = group.indices.first().copied().unwrap_or(0);
            self.audit(
                crate::subtitle::audit::EditKind::Merge,
                index,
                format!(
                    "{} 句重复：{}",
                    group.indices.len(),
                    audit_snippet(&group.text)
                ),
                format!("保留 1 句，时间 {:.2}-{:.2}", group.start, group.end),
            );
        }
        let report = crate::subtitle::cleanup::fold_duplicates(&mut self.segments);
        // 删条会改序号与清单长度：与 `delete_selected_segment` 同一套收尾。
        self.reindex_segments();
        if selected_index.is_some() {
            // 选中句还在就跟随同一句（删除发生在它之前时序号前移），被折掉就跟随
            // 它并进去的那一句——那正是用户原本在看的那句话的归并结果。
            let anchor = folded_owner_pos.or(selected_pos);
            let new_pos = anchor.and_then(|pos| survivors.iter().position(|&s| s == pos));
            match new_pos.and_then(|pos| self.segments.get(pos).map(|s| s.index)) {
                Some(idx) => self.select_segment(idx),
                None => match self.segments.first().map(|s| s.index) {
                    Some(first) => self.select_segment(first),
                    None => {
                        self.selected_segment_index = None;
                        self.editing_text.clear();
                    }
                },
            }
        }
        self.segments_dirty = true;
        self.bump_segments_revision();
        self.flush_segments_if_dirty();
        self.cleanup_note = Some(format!(
            "已折叠 {} 组重复句（删除 {} 句），可 Ctrl+Z 撤销",
            report.groups, report.removed
        ));
        report.removed
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
/// 审计摘要里的一段文本：只留前 40 个字符并压成单行。
///
/// 编辑日志是有界的、还要整份进导出，把整句（甚至整段）塞进 before/after 会让
/// 日志体积随素材长度线性膨胀；而审计要回答的是「改过什么」，一句样例就够。
/// 只保留 `root` 下最近 `keep` 个子目录（按 mtime 从新到旧），其余尽力删除，返回删除数。
///
/// 纯 best-effort：读目录失败、条目不是目录、取不到 mtime、删除失败都静默跳过——清缓存
/// 绝不能反过来挡住一次转写开工。只删 `root` 的**子目录**，不碰 `root` 自身，也不删文件
/// （阶段产物是「一阶段一文件」放在子目录里）。`root` 不存在时直接返回 0。
fn prune_stale_stage_dirs(root: &std::path::Path, keep: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        dirs.push((modified, entry.path()));
    }
    if dirs.len() <= keep {
        return 0;
    }
    // 新的在前；越界的那批就是「最旧且超出保留额度」的。
    dirs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut removed = 0usize;
    for (_, path) in dirs.into_iter().skip(keep) {
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}
fn audit_snippet(text: &str) -> String {
    const MAX_CHARS: usize = 40;
    let flat = text.replace(['\r', '\n'], " ");
    let flat = flat.trim();
    let mut out: String = flat.chars().take(MAX_CHARS).collect();
    if flat.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

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

/// 在**已持有写入闸**的前提下把一份快照写进库（防回退的最后一道）。
///
/// 只用 `Database` 已有的入口：`update_task_segments_json` 接收已经序列化好的
/// `segments_json`，自己解析一次算派生列（样例文本 / 句数 / 有无译文）并刷新 `duration`，
/// 所以这里不重复造轮子。后台线程因此不需要 `Vec<Segment>` 的所有权，快照可以是纯 String。
///
/// 本函数只拿到 `Database`（内部是 `Arc<Mutex<Connection>>`，天生可跨线程）与一个纯值快照，
/// 拿不到 `AppState`；调用方负责持闸，因此它同时被后台写线程与同步落库复用。
///
/// 顺序判定只用一个事实：`applied`（每个工程已经真正落库的最大写序号）。
/// 闸里先查后写，所以「查」与「写」之间不可能插进另一份写：
///
/// - `applied[task] >= seq`：同一工程已经有更新的快照落库，本次写进去就是**回退**，
///   直接放弃并返回 `false`。返回成功而不是错误：库里确实有更新的内容，
///   报错只会让界面把一份已经写完的最新内容重新标脏、再派发一轮；
/// - 否则真写，并在**写成功之后**把水位推到自己：失败时水位不动，后面那份更新的
///   快照照样会写。
///
/// 调用方必须持有 `BackgroundWrites::lock_writer()`（本函数要用这把锁保证
/// 「查水位 + 写 + 记水位」是一个原子段）。
fn write_snapshot_under_gate(
    writes: &BackgroundWrites,
    db: &Database,
    snapshot: &SegmentWriteSnapshot,
    seq: u64,
) -> anyhow::Result<bool> {
    if writes.tracker.applied_seq(snapshot.task_id) >= seq {
        tracing::debug!(
            task_id = snapshot.task_id,
            seq,
            "同工程已有更新的写落库，放弃这次写入"
        );
        return Ok(false);
    }
    db.update_task_segments_json(snapshot.task_id, &snapshot.json)?;
    writes.tracker.note_applied(snapshot.task_id, seq);
    Ok(true)
}

/// 后台线程也能用的「过闸写字幕」入口（转写收尾专用）。
///
/// # 为什么要有这个结构，而不是让调用点传 `Arc<AppState>`
///
/// 转写收尾（`finish_db_work`）跑在后台线程上，它要把**上一个工程**的脏字幕写库。
/// 那条路径过去直接 `db.update_task_segments(id, segs)`：既没领写序号、也没按工程查水位，
/// 于是后台在飞的字幕写与它撞同一个 task 时，**旧内容可能覆盖新内容**——与写入闸要防的
/// 正是同一件事（见 `BackgroundWrites` 的文档）。
///
/// 修法不是「把 `AppState` 塞进后台线程」：`AppState` 是渲染线程按帧 `&mut` 借用的，
/// 搬进去就得给整个界面状态上锁。真正需要的只有两样东西——`Database`（内部
/// `Arc<Mutex<Connection>>`，天生可 `Send + Sync`）与按工程的水位账本。把这两样
/// 打成一个小句柄 `SegmentWriteGate`，后台线程只拿它，规则仍然只有一份
/// （`write_snapshot_under_gate`），不复制一套平行实现。
///
/// # 序号为什么由调用方给，而不是这里领
///
/// 收尾路径在后台线程上，而 `next_seq` 的单调性依赖「领号只在渲染线程」这个前提
/// （派发侧与同步写都在渲染线程上领号）。所以这里不自己 `fetch_add`，而是让调用点
/// 带上它那份快照的序号：派发那一刻（渲染线程）从 `next_write_seq` 取一个当时值即可，
/// 水位判定（`applied[task] >= seq`）因此照样成立——旧收尾写遇到更新的写只会被跳过。
#[derive(Clone)]
pub(crate) struct SegmentWriteGate {
    db: Database,
    writes: Arc<BackgroundWrites>,
}

impl SegmentWriteGate {
    /// 把一份**上一工程**的脏字幕快照过闸写库；返回「这次真的写了没有」。
    ///
    /// 全程持有写入闸，闸里一次完成「查同工程水位 → 写 → 记水位」，与同步落库
    /// （`AppState::sync_segments_gated`）和后台写线程共用同一把锁、同一个账本。
    pub(crate) fn write_finished_task_segments(
        &self,
        task_id: i64,
        seq: u64,
        segments: &[crate::subtitle::Segment],
    ) -> anyhow::Result<bool> {
        let _guard = self.writes.lock_writer();
        if self.writes.tracker.applied_seq(task_id) >= seq {
            // 同一个工程已经有更新的内容落库：这次写回去就是回退。算成功——库里
            // 那份更新的内容本来就是用户最后看到的状态。
            tracing::debug!(
                task_id,
                seq,
                "同工程已有更新的写落库，放弃转写收尾的脏字幕回写"
            );
            return Ok(false);
        }
        self.db.update_task_segments(task_id, segments)?;
        self.writes.tracker.note_applied(task_id, seq);
        Ok(true)
    }
}

/// 把后台写线程的回报并进 UI 状态（**纯函数**，只在渲染线程上跑，便于单测钉死判定）。
///
/// # 判定规则（防回退的核心）
///
/// 一条回报只有在**两件事同时成立**时才算数：
///
/// （调用方 `BackgroundWrites::drain_results` 已经先筛掉「同工程已有更新的写落库」的
/// 过期回报，这里不再重复判水位——水位是按工程算的，判定只留一处，免得两处漂移。）
/// 2. `pre_drain_error`（**本轮排空之前**的 `db_write_error`；调用方在调用前取快照）
///    与这条回报的「派发时提示条」一致——否则说明后台写这段时间里用户别的落库也
///    失败 / 成功过，那是**更新**的事实，不能被这次结局回写提示条。用调用方给的快照
///    而不是 `db_write_error` 的当前值：同一轮里可能同时收到失败与成功两条，若拿已被
///    前一条改过的字段去比后一条的派发快照，后一条会被误判成「别人改过」整条丢掉；
/// 3. 这份快照仍然是当前内容（`revision` 没变）或者这次写**成功**。旧快照的失败
///    不再置脏：用户已经又改过，新的内容会自己走一遍落库，置脏只会让定时器把那份
///    旧内容再派发一次（而它注定写不进最新状态）。
///
/// 成功：把提示条清空（与同步版 flush 成功一致），并在快照仍是当前修订时**清掉**
/// `segments_dirty`——脏标记从「派发即清」改成「确认落库才清」，收口就在这里
/// （见 `flush_segments_if_dirty_async`）。
/// 失败：追加「字幕保存失败：…」；**只有**这条回报本身就是**同工程**最新的一条
/// **且** `revision` 还是当前的才把 `segments_dirty` 置回，
/// 让定时器 / 关窗守卫重试。
///
/// # 为什么置脏还要看「是不是最新一条」
///
/// 同一轮排空里可能同时收到一份旧快照的失败与一份更新的成功。旧失败若也把脏标记
/// 置回，界面会显示「有未保存改动」并要求再写一遍——而库里那一段内容其实已经被
/// 更新的成功写进去了。这与「旧快照的失败不回写当前状态」是同一条原则。
///
/// # 提示条竞价输了的失败：提示条不动，但脏标记必须补上
///
/// 第 2 条只否定「提示条该不该回写」，不该顺手否定「这份内容还没落库」这个事实。
/// 一条落在**当前修订**上的失败如果既不挂提示、也不置脏，用户就永远看不到它、也不会
/// 有任何一条路径去重试——用户改的东西一直躺在内存里，等到关窗时的同步落库来救。
/// 所以竞价输了的失败仍然按修订号补脏（只补脏，不碰提示条）。
///
/// 「同工程已经有更新的写落库」这种过期回报由调用方（`drain_results`）先筛掉；本函数
/// 只判「这条回报该不该回写界面状态」，因此不需要再去比一个全局水位。
fn drain_segment_writes(
    db_write_error: &mut Option<String>,
    segments_dirty: &mut bool,
    current_revision: u64,
    pre_drain_error: Option<&str>,
    done: &[SegmentWriteDone],
) {
    // 「本批里每个工程最新的一条」：脏标记只在**同工程**没有更新的结论时才补。
    // 写序号是跨工程共用的，但两个工程的内容互不覆盖——拿 A 的新序号去压 B 的失败，
    // 会让 B 的失败既不提示也不重试，而 B 的当前修订其实还没落库。
    let mut newest_by_task: HashMap<i64, u64> = HashMap::new();
    for write in done {
        let slot = newest_by_task.entry(write.task_id).or_insert(write.seq);
        *slot = (*slot).max(write.seq);
    }
    for write in done {
        if pre_drain_error != write.error_at_dispatch.as_deref() {
            // 派发之后又发生过别的失败 / 成功：**提示条**不能按这条回写（更新的事实
            // 已经占位）。但脏标记必须照修订号对齐——要么失败补上（见函数文档「提示条
            // 竞价输了的失败」），要么成功清掉（否则内容已落库、脏标记还挂着，关窗守卫会白等）。
            match &write.result {
                Err(_) => {
                    mark_segments_dirty_for_revision(
                        segments_dirty,
                        write.revision,
                        current_revision,
                    );
                }
                Ok(()) => {
                    if write.revision == current_revision {
                        *segments_dirty = false;
                    }
                }
            }
            tracing::debug!(
                seq = write.seq,
                "派发之后提示条已被别的写库改动过，本次后台写结局不再回写提示条"
            );
            continue;
        }
        apply_segment_write_result(
            db_write_error,
            segments_dirty,
            write,
            current_revision,
            newest_by_task
                .get(&write.task_id)
                .is_some_and(|newest| *newest == write.seq),
        );
    }
}

/// 一条后台写结局落到状态上的**唯一入口**（纯函数，供 `poll_background_writes` 与测试复用）。
///
/// `newest`：这条回报是不是同一轮里**同一个工程**的最新一条（调用方按工程分别取 max 判定）。
///
/// # 错误通道不静默（唯一的例外在调用方）
///
/// 失败会写进 `db_write_error`（与同步版同文案、同「；」拼接规则），界面提示条读的
/// 就是这个字段——后台线程与渲染线程之间没有任何别的错误通路。
///
/// 唯一的例外是「提示条竞价输给更新事实」的失败：调用方（[`drain_segment_writes`]）
/// 不会把它的文案挂上去，但会保留「这份内容还没落库」的脏标记。宁可少一条过期文案，
/// 也不能没有重试信号。
///
/// # 为什么「置脏」必须看修订号
///
/// `segments_dirty` 的语义是「**当前**内容还没落库」。后台写的是**过去某一刻**的快照：
/// 它失败时若直接把脏标记置回，而用户其实已经又改过（`segments_revision` 更大），
/// 定时器稍后会把那份**旧快照**当成当前内容派发一次——落进库的就不是用户最新改的东西。
/// 所以只有「失败的那份快照正好还是当前修订」才置脏；否则脏标记归更新的那份编辑负责，
/// 由下一次 `defer_segments_flush` / 去抖重新标记。
///
/// 同理，已经被同一轮里更新的一条越过（`newest == false`）的失败也不置脏：那段内容
/// 已经有更新的写落库了，再标脏只会让用户看到一次多余的「未保存」与一次无谓的重写。
fn apply_segment_write_result(
    db_write_error: &mut Option<String>,
    segments_dirty: &mut bool,
    write: &SegmentWriteDone,
    current_revision: u64,
    newest: bool,
) {
    match &write.result {
        Ok(()) => {
            *db_write_error = None;
            // 内容已确认落库，脏标记的语义（「当前内容还没落库」）到此为止。派发时不再清脏
            // （见 `flush_segments_if_dirty_async`），清脏的责任就落在这里——只有快照仍是当前
            // 修订才算数；否则用户已经又改过，那份新内容会自己走一遍落库，别替它清。
            if write.revision == current_revision {
                *segments_dirty = false;
            }
        }
        Err(err) => {
            // 与同步版文案、拼接规则完全一致：界面提示条读的就是这个字段。
            *db_write_error = Some(match db_write_error.take() {
                Some(prev) => format!("{prev}；字幕保存失败：{err}"),
                None => format!("字幕保存失败：{err}"),
            });
            // 失败不能静默之外，还得**能重试**：把脏标记放回，让定时器 / 关窗守卫
            // 再走一遍。只对「落库时那份快照就是当前内容、且这条就是最新一条」的失败
            // 置回，理由见函数文档。
            if newest {
                mark_segments_dirty_for_revision(segments_dirty, write.revision, current_revision);
            }
        }
    }
}

/// 失败发生在旧修订上时，`segments_dirty` 该怎么处理（纯函数）。
///
/// - `queued_revision == current_revision`：这份快照仍是当前内容，放回脏标记以便重试；
/// - 否则：用户已经又改过（更新修订号），新的内容会自己走一遍落库，不要动脏标记。
fn mark_segments_dirty_for_revision(
    segments_dirty: &mut bool,
    queued_revision: u64,
    current_revision: u64,
) -> bool {
    if queued_revision == current_revision {
        *segments_dirty = true;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_segment_write_result, drain_segment_writes, mark_segments_dirty_for_revision,
        write_snapshot_under_gate, AppState, BackgroundWrites, PolishMode, ProcessStatus,
        SegmentDispatch, SegmentWriteDone, SegmentWriteGate, SegmentWriteSnapshot,
        WhisperModelTier, SEGMENTS_FLUSH_IDLE,
    };
    use crate::subtitle::Segment;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::Arc;

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

    /// 测试专用：给临时文件一个不会撞名的后缀。
    fn unique_suffix() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// 「稍后再说」只关引导框：向导标记被置完成，但**硬件档位与推理策略一动不动**。
    ///
    /// 回归护栏：两个选择按钮走 `set_hardware_tier`，它会顺带套用一整套餐并落盘。
    /// 「稍后再说」若复用那条路，用户还没表态就被默许了一个从没选过的模式——
    /// 下次启动会发现推理策略、模型路径、线程数全变了。
    #[test]
    fn defer_hardware_setup_only_closes_the_wizard() {
        // 落盘目标指到临时目录：单测不许写仓库根的 `config.toml`（会覆盖真实配置）。
        let cfg_path = std::env::temp_dir().join(format!("v2w_hw_later_{}.toml", unique_suffix()));
        let mut state = test_state(Vec::new());
        // 首次启动的初始状态：向导未完成（默认 cpu 档，见 `GpuConfig::default`）。
        assert!(!state.config.gpu.hardware_init_completed);
        let tier_before = state.config.gpu.hardware_tier.clone();
        let offload_before = state.config.gpu.whisper_offload;
        let decode_before = state.config.gpu.hwaccel_decode;
        let model_before = state.config.paths.whisper_model.clone();
        let threads_before = state.config.pipeline.whisper_threads;

        state.defer_hardware_setup_to(&cfg_path);

        assert!(
            state.config.gpu.hardware_init_completed,
            "稍后再说必须让引导框不再出现，否则等于没有关闭入口"
        );
        assert_eq!(
            state.config.gpu.hardware_tier, tier_before,
            "不许改硬件档位"
        );
        assert_eq!(
            state.config.gpu.whisper_offload, offload_before,
            "不许改推理 offload"
        );
        assert_eq!(
            state.config.gpu.hwaccel_decode, decode_before,
            "不许改硬解开关"
        );
        assert_eq!(
            state.config.paths.whisper_model, model_before,
            "不许换模型路径（那会改变用户下次转写用的档位）"
        );
        assert_eq!(
            state.config.pipeline.whisper_threads, threads_before,
            "不许改线程数"
        );

        // 幂等：重复调用（Esc + 按钮都点到）不会有第二次副作用。
        state.defer_hardware_setup_to(&cfg_path);
        assert_eq!(state.config.gpu.hardware_tier, tier_before);
    }

    /// 下载完模型后的「重算推荐」不许重新探测硬件。
    ///
    /// 回归护栏：这里以前调 `HardwareInfo::detect()`——那会新建 wgpu Instance 枚举一遍
    /// 适配器（实测 ~2.7s）且跑在渲染线程上，用户看到的是「下载完成卡两三秒」。
    /// 现在只按已有档案重算，硬件档案本身必须**原样不动**。
    #[test]
    fn refresh_hardware_recommendation_reuses_existing_probe() {
        let mut state = test_state(vec![]);
        let before_gpus = state.hardware_info.gpus.len();
        let before_threads = state.hardware_info.logical_threads;
        let before_brand = state.hardware_info.cpu_brand.clone();
        // 先把派生值弄成明显不对的状态，证明函数确实重算过
        state.performance_level = crate::core::PerformanceLevel::Low;
        state.user_strategy = crate::core::UserStrategy::Balanced;

        state.refresh_hardware_recommendation();

        assert_eq!(state.hardware_info.gpus.len(), before_gpus, "不许重探显卡");
        assert_eq!(state.hardware_info.logical_threads, before_threads);
        assert_eq!(state.hardware_info.cpu_brand, before_brand);
        let expected_level = state.hardware_info.evaluate_performance();
        assert_eq!(
            state.performance_level, expected_level,
            "档位必须按档案重算"
        );
        assert_eq!(
            state.recommended_profile,
            crate::core::InferenceProfile::decide(
                expected_level,
                state.user_strategy,
                &state.hardware_info
            ),
            "推荐配置必须按档案重算"
        );
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

    /// 队列落盘 → 读回：状态映射必须保真，且「转写中」要恢复成等待中。
    ///
    /// 这条锁住的是「关窗不丢队列」的接线。最容易错的映射是 `Running`：直接照搬
    /// 会让重启后的界面显示「转写中…」，而实际上什么都没在跑——用户会一直等下去。
    #[test]
    fn queue_roundtrip_restores_states_and_demotes_running() {
        use crate::utils::queue_store::{load_queue, save_queue, StoredQueueItem};
        let dir = std::env::temp_dir().join(format!("v2w_qtest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.json");

        // 模拟 persist_queue 写出的三条：等待 / 完成 / 转写中
        let items = vec![
            StoredQueueItem {
                path: r"D:\a.mp4".to_string(),
                duration: 12.5,
                status: "pending".to_string(),
            },
            StoredQueueItem {
                path: r"D:\b.mp4".to_string(),
                duration: 30.0,
                status: "done".to_string(),
            },
            StoredQueueItem {
                path: r"D:\c.mp4".to_string(),
                duration: 0.0,
                status: "pending".to_string(), // Running 也写 pending，见 persist_queue
            },
        ];
        save_queue(&path, &items).expect("落盘应成功");
        let back = load_queue(&path);
        assert_eq!(back, items, "往返必须逐字保真");
        assert_eq!(back[1].duration, 30.0);

        let _ = std::fs::remove_dir_all(&dir);
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

    /// 回归：润色附加开销必须**按档位**计，而不是一个 bool 了事。
    ///
    /// SenseVoice 是出厂默认引擎、`punc` 是默认档位，而管线对
    /// 「SenseVoice + punc」整段跳过标点——若沿用旧的 `enable_polish: bool`，
    /// 就只能在「给误开它的 SenseVoice 用户虚高 +1.3 分钟」与「让选了 Qwen 的人
    /// 被严重低估」之间二选一。按档位计，两种误导同时消掉。
    #[test]
    fn polish_overhead_depends_on_mode_not_just_switch() {
        use crate::app::PolishMode;
        let d = 1920.0; // 32 分钟样片
        let sv = WhisperModelTier::SenseVoice;
        let base = sv.estimate_seconds(d, 8, false, None);
        let punc = sv.estimate_seconds(d, 8, false, Some(PolishMode::PuncFast));
        let qwen = sv.estimate_seconds(d, 8, false, Some(PolishMode::QwenDeep));

        assert_eq!(base, punc, "SenseVoice 的极速标点被跳过，不该加时间");
        assert!(
            qwen > punc,
            "Qwen 深度润色必须计入附加开销：punc={punc} qwen={qwen}"
        );
        let delta = qwen - punc;
        assert!(
            (delta - 76.8).abs() < 1.0,
            "Qwen 附加开销应按 2.4 s/分钟 计，实际 {delta}"
        );

        let w = WhisperModelTier::Balanced;
        assert_eq!(
            w.estimate_seconds(d, 8, false, Some(PolishMode::PuncFast)),
            w.estimate_seconds(d, 8, false, None),
            "CT-Punc 毫秒级，不该计入预估"
        );
        assert!(
            w.estimate_seconds(d, 8, false, Some(PolishMode::QwenDeep))
                > w.estimate_seconds(d, 8, false, None)
        );
    }

    #[test]
    fn more_threads_shortens_cpu_eta() {
        let d = 32.0 * 60.0;
        let t4 = WhisperModelTier::Balanced.estimate_seconds(d, 4, false, None);
        let t8 = WhisperModelTier::Balanced.estimate_seconds(d, 8, false, None);
        let t16 = WhisperModelTier::Balanced.estimate_seconds(d, 16, false, None);
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
        let cpu = WhisperModelTier::Precise.estimate_seconds(d, 8, false, None);
        let gpu = WhisperModelTier::Precise.estimate_seconds(d, 8, true, None);
        assert!(gpu < cpu * 0.5);
    }

    /// P0-A5：GPU ETA 按档位标定——大模型的 GPU 加速比要显著大于小模型，
    /// 否则 Turbo 档会被严重高估（界面说 35 s、实际 25 s）。
    #[test]
    fn gpu_speedup_is_tier_dependent() {
        let d = 32.0 * 60.0;
        let ratio = |tier: WhisperModelTier| {
            let cpu = tier.estimate_seconds(d, 8, false, None);
            let gpu = tier.estimate_seconds(d, 8, true, None);
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

    /// 进度只落在当前活动项上，别的条目一个字节都不许动——它们还没开始跑。
    #[test]
    fn progress_lands_on_active_item_only() {
        use super::QueueItem;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
        ];
        state.batch_active = Some(1);

        state.record_batch_progress("语音识别", 0.37);

        assert_eq!(state.batch_queue[1].progress, 37.0);
        assert_eq!(state.batch_queue[1].stage.as_deref(), Some("语音识别"));
        assert_eq!(state.batch_queue[0].progress, 0.0, "还没跑的那条不许有进度");
        assert!(state.batch_queue[0].stage.is_none());
        assert_eq!(state.batch_queue[2].progress, 0.0, "还没跑的那条不许有进度");
        assert!(state.batch_queue[2].stage.is_none());

        // 越界（活动下标指向已被删掉的条目）必须静默忽略，不许 panic
        state.batch_active = Some(99);
        state.record_batch_progress("语音识别", 0.9);

        // 指针被清空之后（批量收尾 / 单文件模式）再来的事件同样无处可落
        state.batch_active = None;
        state.record_batch_progress("语音识别", 0.9);
    }

    /// 切到下一条：上一条的进度必须**定格**，不能跟着新事件继续爬。
    ///
    /// 定格规则取的是「上一条被自己结算时的状态」——成功是 1.0、失败保留当时值；
    /// 这条用例走的是「上一条报错退出、下一条接管」的路径，失败那条约 0.5 定格，
    /// 之后所有进度事件都只许落在下一条身上。
    #[test]
    fn progress_stops_at_previous_item_when_batch_advances() {
        use super::QueueItem;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
        ];
        state.batch_active = Some(0);
        state.mark_queue_running(0);
        state.record_batch_progress("语音识别", 0.5);

        state.finish_active_queue_item(Err("坏文件".to_string()));
        assert_eq!(
            state.batch_queue[0].progress, 50.0,
            "失败条目的进度定格在出事那一刻，交给续跑逻辑去开始下一条"
        );

        state.batch_active = Some(1);
        state.mark_queue_running(1);
        state.record_batch_progress("提取音频", 0.1);

        assert_eq!(state.batch_queue[1].progress, 10.0, "进度必须跟着新条目走");
        assert_eq!(state.batch_queue[1].stage.as_deref(), Some("提取音频"));
        assert_eq!(state.batch_queue[0].progress, 50.0, "上一条的进度不许再变");
        assert!(state.batch_queue[0].stage.is_none());
        assert_eq!(state.batch_queue[2].progress, 0.0, "还没跑的那条不许有进度");
        assert!(state.batch_queue[2].stage.is_none());
    }

    /// 单文件模式（`batch_active == None`）绝不往队列里写：队列卡片上的条目
    /// 应当是干净的，否则单跑一支片子，队列里某个「等待中」的条目会凭空
    /// 冒出百分比，用户以为它已经在转写。
    #[test]
    fn batch_progress_is_ignored_outside_batch_mode() {
        use super::QueueItem;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        assert!(state.batch_active.is_none());

        state.record_batch_progress("语音识别", 0.88);

        assert_eq!(state.batch_queue[0].progress, 0.0);
        assert!(state.batch_queue[0].stage.is_none());
    }

    #[test]
    fn queue_item_progress_defaults_to_zero_and_clears_stale_stage() {
        use super::{QueueItem, QueueState};
        use std::path::PathBuf;

        // 新条目：没进度、没阶段。阶段留着 `Some` 会让「等待中」的卡片显示
        // 一个不存在的阶段名——面板上就是一条假信息。
        let fresh = QueueItem::new(PathBuf::from("a.mp4"));
        assert_eq!(fresh.progress, 0.0);
        assert!(fresh.stage.is_none());

        // 重跑同一条：残留进度和阶段都必须被清掉
        let mut state = test_state(Vec::new());
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        state.batch_queue[0].progress = 87.0;
        state.batch_queue[0].stage = Some("智能润色".to_string());
        state.mark_queue_running(0);
        assert_eq!(state.batch_queue[0].progress, 0.0, "重跑必须从 0 开始");
        assert!(state.batch_queue[0].stage.is_none());
        assert_eq!(state.batch_queue[0].state, QueueState::Running);
    }

    /// 回归：单文件入口跑完**队列里的条目**后，那条必须被结算掉。
    ///
    /// 这是「明明转写完了，队列里每次重启都挂着一条待转写」的根因：
    /// 队列卡片可点（`select_queue_item` 把文件设成 `transcribe_file`），
    /// 用户点完再按「开始转写」走的是单文件入口，那里 `batch_running` 被显式置 false，
    /// 收尾时 `in_batch` 为假 → `finish_active_queue_item` 不执行 → 该条永远停在
    /// `Pending`，并随队列以 `pending` 落盘，于是每次启动都再冒出来。
    ///
    /// 这里同时钉住三条：能按路径结算、结算后是 `Done`、重复调用不覆盖已有句数。
    #[test]
    fn settling_by_path_marks_the_matching_item_done() {
        use super::{QueueItem, QueueState};
        use std::path::{Path, PathBuf};

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        state.batch_queue[0].progress = 37.0;
        state.batch_queue[0].stage = Some("语音识别".to_string());
        assert!(matches!(state.batch_queue[0].state, QueueState::Pending));

        // 大小写不同也要命中：判等走小写归一（与入队去重同一套规则）。
        assert!(state.settle_queue_item_by_path(Path::new("A.MP4"), 12));
        assert_eq!(
            state.batch_queue[0].state,
            QueueState::Done { segments: 12 }
        );
        assert_eq!(state.batch_queue[0].progress, 100.0, "结算后必须满格");
        assert!(state.batch_queue[0].stage.is_none(), "不能留阶段残影");

        // 已经结算过：不重复返回 true，也不该把真实句数改成别的值。
        assert!(!state.settle_queue_item_by_path(Path::new("a.mp4"), 99));
        assert_eq!(
            state.batch_queue[0].state,
            QueueState::Done { segments: 12 }
        );

        // 不在队列里的文件：什么都不动（单文件转写大多数时候确实不在队列里）。
        assert!(!state.settle_queue_item_by_path(Path::new("other.mp4"), 5));
        assert_eq!(
            state.batch_queue[0].state,
            QueueState::Done { segments: 12 }
        );
    }

    /// 结算时的定格规则：**成功收到 100%，失败保留当下值**。
    ///
    /// 失败若也强行收到 0%，用户看到的就是「这一条根本没跑过」；若强行跳到 100%，
    /// 又像是在骗：「都满格了怎么还失败」。保留当下值才说得通，而失败原因由
    /// `QueueState::Failed` 负责解释。
    #[test]
    fn finishing_freezes_progress_at_full_on_success_and_keeps_it_on_failure() {
        use super::QueueItem;
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        state.batch_active = Some(0);
        state.batch_queue[0].progress = 42.0;
        state.batch_queue[0].stage = Some("语音识别".to_string());

        state.finish_active_queue_item(Ok(9));
        assert_eq!(state.batch_queue[0].progress, 100.0, "成功后必须满格");
        assert!(
            state.batch_queue[0].stage.is_none(),
            "结算后不能留下阶段残影"
        );

        state.batch_active = Some(0);
        state.batch_queue[0].progress = 55.0;
        state.batch_queue[0].stage = Some("生成字幕".to_string());
        state.finish_active_queue_item(Err("坏文件".to_string()));
        assert_eq!(
            state.batch_queue[0].progress, 55.0,
            "失败要保留出事那一刻的进度，别清零也别骗成满格"
        );
    }

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

    /// 重排只对「合法且真的会换位置」的入参生效：空队列 / 单元素队列 / 越界 /
    /// `from == to` 都是安全空操作，合法移动则把条目搬到目标下标上。
    ///
    /// 空操作必须是**一字不动**：这类调用来自第一行的「上移」、最后一行的「下移」
    /// （界面已置灰，但状态层要能兜住任何调用方），一旦顺手重排就会把用户排好的顺序改掉。
    #[test]
    fn move_queue_item_reorders_only_valid_distinct_indices() {
        use super::{QueueItem, QueueState};
        use std::path::PathBuf;

        fn names(state: &AppState) -> Vec<String> {
            state.batch_queue.iter().map(|i| i.name.clone()).collect()
        }

        let mut state = test_state(Vec::new());
        assert!(!state.move_queue_item(0, 0), "空队列不能移动");
        assert!(state.batch_queue.is_empty(), "失败的空操作不许建出条目");

        // 单元素队列：没有任何合法目标（上移 / 下移都无处可去）
        state.batch_queue = vec![QueueItem::new(PathBuf::from("a.mp4"))];
        assert!(!state.move_queue_item(0, 1));
        assert!(!state.move_queue_item(1, 0));
        assert!(!state.move_queue_item(0, 0));
        assert_eq!(names(&state), vec!["a.mp4"]);

        state.batch_queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
        ];
        state.batch_queue[1].state = QueueState::Running;

        // 下移一格：a 落到下标 1
        assert!(state.move_queue_item(0, 1));
        assert_eq!(names(&state), vec!["b.mp4", "a.mp4", "c.mp4"]);
        // 搬到队首：c 从队尾插到 0
        assert!(state.move_queue_item(2, 0));
        assert_eq!(names(&state), vec!["c.mp4", "b.mp4", "a.mp4"]);

        // 越界 / 原地：返回 false，顺序不动
        assert!(!state.move_queue_item(0, 3));
        assert!(!state.move_queue_item(3, 0));
        assert!(!state.move_queue_item(1, 1));
        assert_eq!(names(&state), vec!["c.mp4", "b.mp4", "a.mp4"]);
    }

    /// 重排不得让活动指针错位：`batch_active` 必须仍然指向**同一条**任务，
    /// 不论它在移动中换了哪个下标。
    ///
    /// 这是重排最致命的坑：下标一变，指针若留在原数字上就指向了邻居——
    /// `finish_active_queue_item` 会把完成 / 失败结算到别人头上，而真正在跑的那条
    /// 永远停在「转写中」。所以移动别的条目、甚至移动正在跑的那条本身，指针都得跟着走。
    #[test]
    fn moving_queue_items_keeps_active_pointer_on_the_running_item() {
        use super::{QueueItem, QueueState};
        use std::path::PathBuf;

        let mut state = test_state(Vec::new());
        state.batch_queue = vec![
            QueueItem::new(PathBuf::from("a.mp4")),
            QueueItem::new(PathBuf::from("b.mp4")),
            QueueItem::new(PathBuf::from("c.mp4")),
            QueueItem::new(PathBuf::from("d.mp4")),
        ];
        // c.mp4 正在跑（下标 2）
        state.batch_active = Some(2);
        state.batch_queue[2].state = QueueState::Running;

        // 把队首的 a 搬到队尾：c 前移到 1，指针必须跟着改到 1
        assert!(state.move_queue_item(0, 3));
        assert_eq!(state.batch_active, Some(1));
        assert_eq!(state.batch_queue[1].name, "c.mp4");

        // 再把队尾的 a 搬回队首：c 回到 2
        assert!(state.move_queue_item(3, 0));
        assert_eq!(state.batch_active, Some(2));
        assert_eq!(state.batch_queue[2].name, "c.mp4");

        // 失败的空操作更不能碰指针
        assert!(!state.move_queue_item(2, 2));
        assert!(!state.move_queue_item(9, 0));
        assert_eq!(state.batch_active, Some(2));
        assert_eq!(state.batch_queue[2].name, "c.mp4");

        // 连正在跑的那条自己也能搬（界面置灰只是不给点，状态层照样保持归属正确）
        assert!(state.move_queue_item(2, 0));
        assert_eq!(state.batch_active, Some(0));
        assert_eq!(state.batch_queue[0].name, "c.mp4");

        // 结算必须落在真正在跑的那条上，而不是它换位前的邻居
        state.finish_active_queue_item(Ok(9));
        assert_eq!(state.batch_queue[0].state, QueueState::Done { segments: 9 });
        assert!(state.batch_active.is_none());
        assert_eq!(state.batch_queue[1].state, QueueState::Pending);
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
            has_translation: false,
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

    /// 载入历史工程时 `total_duration` 必须与**优化后**的时间轴自洽。
    ///
    /// `load_task_with` 先 `optimize_segments` 再落定 `self.segments`，但时长回退
    /// 此前取的是优化**前**的 `task.segments.last().end`。极短尾句会在优化时被平滑
    /// 延展到最短可读时长（0.8s），于是 `total_duration` 比真实时间轴还短：`seek_to`
    /// 会把播放指针夹在末尾之前，用户点时间轴最后一段跳不过去。
    #[test]
    fn load_task_duration_uses_optimized_timeline() {
        use crate::storage::TaskRecord;

        let mut state = test_state(Vec::new());
        let task = TaskRecord {
            id: 7,
            file_path: "D:/video/c.mp4".to_string(),
            file_name: "c.mp4".to_string(),
            // 库里没记时长（探测失败）：必须回退到时间轴
            duration: 0.0,
            status: "completed".to_string(),
            segments: vec![
                Segment::new(1, 0.0, 1.0, "第一句话"),
                Segment::new(2, 1.0, 1.2, "嗯"),
            ],
            segment_count: 2,
            sample_text: "第一句话".to_string(),
            segments_loaded: true,
            has_translation: false,
            created_at: String::new(),
            content_hash: None,
            metrics: None,
        };
        state.load_task_with(&task, false);

        let timeline_end = state.segments.last().map(|s| s.end).unwrap_or(0.0);
        assert!(
            timeline_end > 1.2,
            "前置条件：尾句应被极短句平滑延展（实际末句 end = {timeline_end}）"
        );
        assert_eq!(
            state.total_duration, timeline_end,
            "total_duration 必须跟随优化后的时间轴，否则 seek_to 会把指针夹在末尾之前"
        );
    }

    /// 缓存命中载入与 `load_task_with` 同口径：`total_duration` 取优化后末句 end。
    ///
    /// 回归护栏：`load_from_cache` 此前在 `optimize_segments` **之前**用
    /// `cached.segments.last().end` 算回退时长，同一个坑。
    #[test]
    fn load_from_cache_duration_uses_optimized_timeline() {
        use crate::storage::TaskRecord;

        let mut state = test_state(Vec::new());
        let cached = TaskRecord {
            id: 9,
            file_path: "D:/video/d.mp4".to_string(),
            file_name: "d.mp4".to_string(),
            duration: 0.0,
            status: "completed".to_string(),
            segments: vec![
                Segment::new(1, 0.0, 1.0, "第一句话"),
                Segment::new(2, 1.0, 1.2, "嗯"),
            ],
            segment_count: 2,
            sample_text: "第一句话".to_string(),
            segments_loaded: true,
            has_translation: false,
            created_at: String::new(),
            content_hash: None,
            metrics: None,
        };
        state.load_from_cache(cached);

        let timeline_end = state.segments.last().map(|s| s.end).unwrap_or(0.0);
        assert!(
            timeline_end > 1.2,
            "前置条件：尾句应被平滑延展（实际 {timeline_end}）"
        );
        assert_eq!(
            state.total_duration, timeline_end,
            "缓存命中载入也必须用优化后的时间轴算 total_duration"
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

    /// 删除「同路径的另一条历史记录」不能切走当前工作区。
    ///
    /// 回归护栏：此前 `delete_task_record` 用 `selected_file` 路径比较判定
    /// 「被删的是不是当前工程」。同一路径允许多条记录（每次转写各插一行），
    /// 删掉同路径的另一条会把当前工程无故补位切走。改为按 `active_task_id`
    /// 主键判定后，删非当前记录应原样保留工作区。
    #[test]
    fn deleting_same_path_sibling_keeps_current_workspace() {
        let mut state = test_state(Vec::new());
        let path = "D:/video/same.mp4";
        let older = state
            .db
            .insert_task(
                path,
                "same.mp4",
                3.0,
                "completed",
                &[Segment::new(1, 0.0, 3.0, "旧记录")],
                None,
            )
            .expect("插入旧记录应成功");
        let current = state
            .db
            .insert_task(
                path,
                "same.mp4",
                3.0,
                "completed",
                &[Segment::new(1, 0.0, 3.0, "当前记录")],
                None,
            )
            .expect("插入当前记录应成功");

        state.refresh_recent_tasks();
        let current_task = state
            .recent_tasks
            .iter()
            .find(|t| t.id == current)
            .cloned()
            .expect("列表应能取到当前记录");
        state.load_task_with(&current_task, false);
        assert_eq!(state.active_task_id, Some(current));

        // 删同路径的**另一条**记录：当前工作区必须原样保留
        state.delete_task_record(older).expect("删除应成功");
        assert_eq!(
            state.active_task_id,
            Some(current),
            "删同路径另一条记录不应切走当前工作区"
        );
        assert_eq!(state.segments.len(), 1, "当前工程字幕不应被清空");
        assert_eq!(state.segments[0].text, "当前记录");
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
    /// 语言包：换目标语言重译**不能**把旧语言的译文覆盖掉，切回去要能原样还原。
    ///
    /// 这就是「字幕语言包」面板的数据基础：此前只有 `translation` 一份，译成英文
    /// 就把中文那遍冲掉了，用户既看不到「译过哪几种语言」，也切不回去。
    #[test]
    fn translation_packs_survive_relanguage_and_can_be_switched_back() {
        use crate::subtitle::Segment;

        let mut state = test_state(vec![Segment::new(1, 0.0, 2.0, "こんにちは")]);

        let mut first = state.segments.clone();
        first[0].translation = Some("你好".to_string());
        first[0].translation_lang = Some("简体中文".to_string());
        state.merge_translations(first);
        assert_eq!(
            state.translation_packs(),
            vec![("简体中文".to_string(), 1)],
            "译完一门语言应出现一个语言包"
        );

        // 改译成英文：中文那一份必须留着
        let mut second = state.segments.clone();
        second[0].translation = Some("Hello".to_string());
        second[0].translation_lang = Some("English".to_string());
        state.merge_translations(second);
        let packs = state.translation_packs();
        assert!(
            packs.iter().any(|(l, n)| l == "简体中文" && *n == 1),
            "旧语言包被覆盖了: {packs:?}"
        );
        assert!(packs.iter().any(|(l, n)| l == "English" && *n == 1));
        assert_eq!(state.current_display_pack().as_deref(), Some("English"));

        // 切回中文：展示字段被投影回来
        state.apply_translation_pack(Some("简体中文"));
        assert_eq!(state.segments[0].translation.as_deref(), Some("你好"));
        assert_eq!(
            state.segments[0].translation_lang.as_deref(),
            Some("简体中文")
        );

        // 切回原文：展示清空，但两份语言包都还在
        state.apply_translation_pack(None);
        assert!(state.segments[0].translation.is_none(), "应回到只显示原文");
        assert_eq!(state.translation_packs().len(), 2, "语言包一份都不该丢");
    }

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

    /// 回归：播放推进 / 跳转时，「选中句」与「编辑缓冲」必须与
    /// 画面字幕（`get_active_segment`）同步。
    ///
    /// 缺陷现象：播放到第 3 句时，画面上已是第 3 句的字幕，右侧编辑框却还停在
    /// 第 1 句——因为播放时钟只改 `selected_segment_index`、没同步 `editing_text`。
    /// 用户看到就是「视频字幕和右边字幕对不上」。
    #[test]
    fn playback_advance_keeps_selection_and_edit_buffer_in_sync() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
            Segment::new(3, 4.0, 6.0, "第三句"),
        ]);
        state.select_segment(1);
        assert_eq!(state.editing_text, "第一句");

        // 播放推进到 4.5s（落在第 3 句）
        state.current_time = 4.5;
        assert!(state.sync_selection_to_time(false));
        assert_eq!(state.selected_segment_index, Some(3));
        assert_eq!(
            state.editing_text, "第三句",
            "编辑缓冲必须跟上画面字幕，否则就是“对不上”"
        );

        // 同一句内重复调用不应报「已变化」（避免 40ms 回调里空转）
        assert!(!state.sync_selection_to_time(false));
    }

    /// 用户正在编辑框里打字时（`preserve_editing = true`），播放推进只跟随
    /// 选中项、**不得**覆盖 `editing_text`——否则把用户敲到一半的字冲掉。
    #[test]
    fn playback_advance_preserves_in_progress_edit_buffer() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.select_segment(1);
        state.editing_text = "第一句正在打字".to_string();

        state.current_time = 3.0;
        assert!(state.sync_selection_to_time(true));
        assert_eq!(state.selected_segment_index, Some(2));
        assert_eq!(
            state.editing_text, "第一句正在打字",
            "正在输入的文本不能被播放推进冲掉"
        );
    }

    /// 跳转（时间轴点击 / 上下句外的 seek）也必须同步编辑缓冲。
    #[test]
    fn seek_to_aligns_selection_and_edit_buffer_with_overlay() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 2.0, 4.0, "第二句"),
        ]);
        state.select_segment(1);
        state.seek_to(3.2);
        assert_eq!(state.selected_segment_index, Some(2));
        assert_eq!(state.editing_text, "第二句");

        // 半开区间：落在切句点 2.0（= 第二句的 start）归第二句，
        // 与画面字幕取值一致。
        state.seek_to(2.0);
        assert_eq!(state.selected_segment_index, Some(2));
        assert_eq!(state.editing_text, "第二句");
    }

    /// 点击字幕清单某行 = 选中该句并把播放位置归位到该句起点（校对台的鼠标点击契约）。
    /// 与键盘「上/下句」一致：`current_time` 必须落在被点句的 `start`，且编辑缓冲同步。
    #[test]
    fn click_row_selects_and_seeks_to_segment_start() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 2.0, "第一句"),
            Segment::new(2, 3.5, 5.0, "第二句"),
        ]);
        // 播放停在别处（第 2 句范围内），点击第 2 行应把位置拉回该句起点
        state.current_time = 4.8;
        state.select_segment(2);
        state.seek_to(3.5);
        assert_eq!(state.selected_segment_index, Some(2));
        assert_eq!(state.current_time, 3.5);
        assert_eq!(state.editing_text, "第二句");

        // 点击第 1 行同理：位置归位到 0.0
        state.select_segment(1);
        state.seek_to(0.0);
        assert_eq!(state.selected_segment_index, Some(1));
        assert_eq!(state.current_time, 0.0);
        assert_eq!(state.editing_text, "第一句");
    }

    /// 回归：线程数收敛到 `[2, cores]` 时，单核机器（cores == 1）不得 panic。
    /// 旧代码 `whisper_threads.clamp(2, cores)` 在 cores < 2 时是 `clamp(2, 1)`，
    /// `min <= max` 断言直接崩；CPU 档切换走的正是这条路径。
    #[test]
    fn clamp_threads_to_cores_handles_single_core() {
        // cores = 1：下界与上界都落到 2，绝不 panic
        assert_eq!(AppState::clamp_threads_to_cores(16, 1), 2);
        assert_eq!(AppState::clamp_threads_to_cores(2, 1), 2);
        // cores = 0（异常硬件档案）同样兜底到 2
        assert_eq!(AppState::clamp_threads_to_cores(8, 0), 2);
        // 常规核数：夹到 [2, cores]
        assert_eq!(AppState::clamp_threads_to_cores(1, 16), 2);
        assert_eq!(AppState::clamp_threads_to_cores(64, 16), 16);
        assert_eq!(AppState::clamp_threads_to_cores(8, 16), 8);
        // 不变量：结果恒在 [2, max(cores, 2)] 内，且永远 >= 2
        for cores in 0u32..=32 {
            for req in [1u32, 2, 4, 8, 16, 32, 64] {
                let v = AppState::clamp_threads_to_cores(req, cores);
                assert!(v >= 2, "线程数不得低于 2: req={req} cores={cores} -> {v}");
                assert!(
                    v <= cores.max(2),
                    "线程数不得超过核数: req={req} cores={cores} -> {v}"
                );
            }
        }
    }

    /// 配置落盘失败必须被记成可见提示，成功则清掉旧错误。
    ///
    /// 这是「设置保存失败」横幅的唯二状态来源：此前 7 处 `let _ = save_to_file`
    /// 把失败完全吞掉，用户改完设置、重启发现设置又变回去了，却毫无线索。
    #[test]
    fn persist_config_records_failure_and_clears_on_success() {
        let mut state = test_state(Vec::new());
        // 造一个「父路径是文件」的非法目标：`save_to_file` 第一步 create_dir_all
        // 就会失败，稳定复现写盘失败，不依赖磁盘满/只读这类环境条件。
        let base = std::env::temp_dir().join(format!(
            "v2w_persist_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad = blocker.join("config.toml");

        state.config_write_error = None;
        state.persist_config_to(&bad);
        assert!(
            state.config_write_error.is_some(),
            "写盘失败必须被记录成可见提示"
        );

        // 换成合法目标：成功落盘后旧错误应被清掉，否则提示条会一直挂着。
        let good = base.join("config.toml");
        state.persist_config_to(&good);
        assert!(good.exists(), "合法目标应真的写出 config.toml");
        assert!(
            state.config_write_error.is_none(),
            "成功落盘后必须清除上一次的失败提示"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
    /// 造一个进程内唯一的临时目录，供落盘相关单测隔离使用。
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个「父路径是文件」的非法落盘目标：`save_to_file` 第一步
    /// `create_dir_all` 就会失败，稳定复现写盘失败，不依赖磁盘满 / 只读。
    fn make_bad_target(base: &std::path::Path) -> std::path::PathBuf {
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        blocker.join("config.toml")
    }

    /// 拖动闸门：拖动期间 `persist_config_to` 不写盘、不改错误状态；
    /// 松手补写一次，文件真的落盘、闸门复位。
    #[test]
    fn config_drag_gate_defers_persist_until_release() {
        let mut state = test_state(Vec::new());
        let base = unique_temp_dir("v2w_drag_gate");
        let good = base.join("config.toml");

        state.config_write_error = None;
        state.begin_config_drag();
        assert!(state.config_drag_active, "按下后闸门应打开");

        // 闸门开着：拖动中反复调用都只留内存，不写盘、不产生错误。
        state.persist_config_to(&good);
        state.persist_config_to(&good);
        assert!(
            !good.exists(),
            "拖动中不得写盘，否则一次拖动会打出几十上百次同步写"
        );
        assert!(
            state.config_write_error.is_none(),
            "拖动中不写盘，自然也不该冒出保存失败提示"
        );

        // 松手：补写一次，文件真的出现、闸门复位。
        state.end_config_drag_to(&good);
        assert!(good.exists(), "松手后应真的写出 config.toml");
        assert!(!state.config_drag_active, "松手后闸门应关闭");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 未开闸门时 `end_config_drag_to` 是空操作：既不写盘也不误报错误
    /// （否则一次普通点击松手会把「其实没保存」的错误提示挂上去）。
    #[test]
    fn end_config_drag_without_begin_is_noop() {
        let mut state = test_state(Vec::new());
        let base = unique_temp_dir("v2w_drag_noop");
        let bad = make_bad_target(&base);

        state.config_write_error = None;
        state.end_config_drag_to(&bad);
        assert!(
            state.config_write_error.is_none(),
            "没在拖动时松手不该触发落盘、更不该报错"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 拖到非法路径：闸门照常打开、不 panic；松手才把失败记成可见提示。
    #[test]
    fn config_drag_gate_records_failure_on_release() {
        let mut state = test_state(Vec::new());
        let base = unique_temp_dir("v2w_drag_fail");
        let bad = make_bad_target(&base);

        state.config_write_error = None;
        state.begin_config_drag();
        state.persist_config_to(&bad);
        assert!(
            state.config_write_error.is_none(),
            "拖动中不写盘，失败也要等松手才记账"
        );

        state.end_config_drag_to(&bad);
        assert!(!state.config_drag_active, "松手后闸门应关闭");
        assert!(
            state.config_write_error.is_some(),
            "松手补写失败必须被记成可见提示"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `record_db_write_error` 追加而非覆盖：同一次操作里可能先后失败两处，
    /// 两条都要留下（只显示最后一条会让用户排查时漏掉另一处）。
    #[test]
    fn record_db_write_error_appends_and_keeps_order() {
        let mut state = test_state(Vec::new());
        assert!(state.db_write_error.is_none());

        state.record_db_write_error("历史记录保存失败：库被锁");
        assert_eq!(
            state.db_write_error.as_deref(),
            Some("历史记录保存失败：库被锁")
        );

        // 第二条来自另一个环节：两条都在，且先发生的排在前
        state.record_db_write_error("字幕保存失败：磁盘已满");
        assert_eq!(
            state.db_write_error.as_deref(),
            Some("历史记录保存失败：库被锁；字幕保存失败：磁盘已满")
        );

        // 成功落库由调用方显式清空，不会被追加逻辑复活
        state.db_write_error = None;
        assert!(state.db_write_error.is_none());
    }

    /// 空串也要如实记录：调用方传进来的就是「发生了什么」，helper 不该
    /// 擅自把空消息当成「没有错误」而吞掉。
    #[test]
    fn record_db_write_error_keeps_empty_message() {
        let mut state = test_state(Vec::new());
        state.record_db_write_error("");
        assert_eq!(state.db_write_error.as_deref(), Some(""));
    }

    /// 真实链路的顺序契约：转写结果入库失败时，错误要在第 2 步 flush 之后才挂到
    /// `db_write_error` 上。这里锁住两个事实——(1) flush 在无脏数据时不会误清已有
    /// 提示；(2) 之后补挂的入库失败会追加在已有提示后面，而不是被吞掉。
    #[test]
    fn db_insert_error_survives_successful_flush() {
        let mut state = test_state(Vec::new());
        // flush 成功（无脏数据，等同 no-op）会把 db_write_error 清空，
        // 若把 insert 错误在 flush 之前挂上就会被吞掉——这正是修复点。
        state.db_write_error = Some("旧错误".to_string());
        state.segments_dirty = false;
        state.flush_segments_if_dirty();
        assert_eq!(state.db_write_error.as_deref(), Some("旧错误"));

        // 模拟第 1 步入库失败：错误在 flush 之后补挂，必须留下
        state.record_db_write_error("历史记录保存失败：库被锁");
        assert_eq!(
            state.db_write_error.as_deref(),
            Some("旧错误；历史记录保存失败：库被锁")
        );
    }

    /// 与之相对：flush 自身成功时会把陈旧提示清空（成功即代表不再有未落库改动）。
    #[test]
    fn successful_flush_clears_stale_db_error() {
        let mut state = test_state(Vec::new());
        state.db_write_error = Some("上一次的失败".to_string());
        state.segments_dirty = true;
        state.active_task_id = None; // 无主键 → sync 直接 Ok
        state.flush_segments_if_dirty();
        assert!(state.db_write_error.is_none(), "成功落库后应清空提示");
    }

    /// 一条合成回报。`error_at_dispatch` 传 `None` 表示「派发时提示条是干净的」——
    /// 真实链路里就是这个状态（上一次成功已经清空），所以测试里的回报必须照抄。
    fn synthetic_write(seq: u64, revision: u64, result: Result<(), String>) -> SegmentWriteDone {
        SegmentWriteDone {
            seq,
            task_id: 1,
            revision,
            error_at_dispatch: None,
            result,
        }
    }

    /// 去抖落库：登记之后不许立刻写；停手满窗口才交给后台；立刻写入的入口要作废登记。
    ///
    /// 回归护栏：`seek_to` 在拖动时间轴时每 40ms 被调一次。一旦回到「每次都同步写库」，
    /// 5000 句的工程每拖一下就要多付 10~30ms 的序列化 + UPDATE，手感直接变差。
    ///
    /// 异步化之后的契约：到点那次只**派发**（脏标记在派发成功时清掉，登记同时作废），
    /// 写库本身在后台；同步版 `flush_segments_if_dirty`（换工程 / 导出 / 关窗守卫）仍然
    /// 是「返回时已写完」，所以它照样会把登记一起作废。
    #[test]
    fn deferred_flush_waits_for_idle_window() {
        let mut state = test_state(Vec::new());
        state.active_task_id = None; // 无主键 → 没有可写回的行，等同已保存

        // 没有改动就不登记：空登记只会让定时器白醒一次
        state.segments_dirty = false;
        state.defer_segments_flush();
        assert!(state.segments_flush_due.is_none(), "没改动不该登记待落库");

        state.segments_dirty = true;
        state.defer_segments_flush();
        let due = state.segments_flush_due.expect("有改动必须登记待落库");

        // 窗口没到：不派发，也不清脏标记，登记继续留着
        assert!(!state.flush_segments_if_due_at(due + std::time::Duration::from_millis(300)));
        assert!(state.segments_dirty, "没到点就不该清脏标记");
        assert!(state.segments_flush_due.is_some(), "没到点登记要留着");

        // 停手满窗口：派发一次（无主键 → 直接清脏），脏标记与登记一起清掉
        assert!(state.flush_segments_if_due_at(due + SEGMENTS_FLUSH_IDLE));
        assert!(!state.segments_dirty, "派发之后脏标记必须清掉");
        assert!(state.segments_flush_due.is_none(), "派发之后登记必须清掉");

        // 立刻写入的入口（换工程 / 导出 / 关窗）要作废登记：
        // 不作废的话定时器稍后还会把同一份内容再写一遍。
        state.segments_dirty = true;
        state.defer_segments_flush();
        assert!(state.segments_flush_due.is_some());
        state.flush_segments_if_dirty();
        assert!(state.segments_flush_due.is_none(), "立刻写入后登记必须作废");

        // 期间被别的路写过（脏标记已清）：到点也不写，登记顺手作废
        state.segments_dirty = true;
        state.defer_segments_flush();
        state.segments_dirty = false;
        assert!(!state.flush_segments_if_due_at(due + SEGMENTS_FLUSH_IDLE * 2));
        assert!(state.segments_flush_due.is_none(), "空登记不能一直挂着");
    }

    /// 后台写真的把字幕写进了库，而且**渲染线程不再等它**。
    ///
    /// 这是本任务的主路径验证：`flush_segments_if_dirty` 的同步语义一个字没改，
    /// 但去抖那条路（`flush_segments_if_dirty_async`）改成派发——派发返回时**脏标记仍在**
    /// （内容还没落库，只是交出去了）、快照已在后台（`background_segments_write_pending()`
    /// 能看到），一两帧之后落库完成、回报收进状态，`Database::load_task_segments` 能读回改动。
    #[test]
    fn async_flush_writes_to_database_from_background_thread() {
        let mut state = test_state(Vec::new());
        let db = crate::storage::Database::open(":memory:").expect("内存库应能打开");
        let task_id = db
            .insert_task("bg.mp4", "bg.mp4", 10.0, "completed", &Vec::new(), None)
            .expect("先插一条空工程");
        state.db = db;
        state.active_task_id = Some(task_id);
        state.segments = vec![Segment::new(1, 0.0, 1.0, "后台写进去的第一句")];
        state.bump_segments_revision();
        state.segments_dirty = true;

        state.flush_segments_if_dirty_async();

        assert!(
            state.segments_dirty,
            "派发只是把快照交给后台、还没落库——脏标记必须保留（关窗守卫靠它堵住「在飞写没落地就关窗」）"
        );
        assert!(
            state.segments_flush_due.is_none(),
            "派发同时要作废待落库登记"
        );

        // 后台线程跑完之前，渲染线程只做「收回报」这件事；这里轮询模拟定时器窗口。
        //
        // 等的判据是「写真的落库了」——`background_segments_write_pending()` 归零只说明
        // 「结果已经投进通道」，而收到回报本身不会让库里立刻可见（回报是在写完之后才投的，
        // 但本测试的轮询窗口比写线程快，可能先看到 pending == 0 而写还没落到读连接上）。
        // 所以直接以「读得回这一句」为完成条件，另外用错误通道做失败短路。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut loaded = Vec::new();
        while std::time::Instant::now() < deadline {
            state.poll_background_writes();
            if state.db_write_error.is_some() {
                break;
            }
            loaded = state
                .db
                .load_task_segments(task_id)
                .expect("读回后台写的字幕");
            if !loaded.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            state.db_write_error.is_none(),
            "写库不该失败：{:?}",
            state.db_write_error
        );
        assert_eq!(
            loaded.len(),
            1,
            "后台写必须真的落库（读到 {} 句）",
            loaded.len()
        );
        assert_eq!(loaded[0].text, "后台写进去的第一句");
        assert!(
            !state.segments_dirty,
            "后台写成功、回报收进状态后，脏标记必须清掉（否则界面一直显示未保存）"
        );
    }

    /// 派发不等于写完：旧快照的失败结局**不许**回写现在的状态（防回退）。
    ///
    /// 三条判定一起钉住：
    /// 1. 派发过的就作废旧登记，不让定时器把同一份内容再写一遍；
    /// 2. 迟到的旧失败（通道里还有、但水位已被更新的成功越过）只留日志，不点提示灯；
    /// 3. 落在旧修订上的失败不置脏——否则定时器会把那份旧内容当当前内容再派发一次。
    #[test]
    fn background_write_completion_never_rewinds_state() {
        let mut state = test_state(Vec::new());
        state.active_task_id = None;
        state.segments_dirty = true;
        state.defer_segments_flush();

        state.flush_segments_if_dirty_async();

        assert!(!state.segments_dirty, "派发之后脏标记必须清掉");
        assert!(
            state.segments_flush_due.is_none(),
            "派发过的快照不许再挂着待落库登记"
        );

        // 同一轮排空里同时到达「旧失败 + 更新的成功」：两条都要处理（这正是
        // 「拿被前一条改过的字段去比后一条」会漏掉成功的那种情形），最终以更新的成功收口。
        let mut dirty = false;
        let mut error = None;
        let old_failure = synthetic_write(1, 7, Err("库被锁".to_string()));
        let newer_success = synthetic_write(2, 7, Ok(()));
        drain_segment_writes(
            &mut error,
            &mut dirty,
            7,
            None,
            &[old_failure, newer_success],
        );
        assert!(error.is_none(), "更新的成功之后不许再挂旧失败");
        assert!(
            !dirty,
            "旧失败被同工程更新的成功越过，不许再标脏（否则界面会要求重写一遍）"
        );

        // 换到「更新的成功在前、更旧的失败在后」（乱序到达）：失败描述的是一份被同工程
        // 更新的写覆盖掉的快照——提示灯与脏标记都不许回退。过期筛选在 `drain_results`
        // 里按**同工程水位**做（不是全局写序号），所以这里走真实通道验证。
        state.segments_writes.tracker.note_applied(1, 3);
        state
            .segments_writes
            .submit
            .send(synthetic_write(
                1,
                state.segments_revision,
                Err("旧快照写失败".to_string()),
            ))
            .expect("投递回报");
        let dirty_before = state.segments_dirty;
        state.poll_background_writes();
        assert!(
            state.db_write_error.is_none(),
            "同工程已有更新结论的迟到失败不许挂提示条"
        );
        assert_eq!(
            state.segments_dirty, dirty_before,
            "迟到的旧失败不许改脏标记"
        );

        // 最新一条失败 + 旧修订：错误挂上（不许静默），但不置脏——交给更新的那份编辑去落库
        error = None;
        let mut dirty = false;
        apply_segment_write_result(
            &mut error,
            &mut dirty,
            &synthetic_write(4, 7, Err("磁盘已满".to_string())),
            9,
            true,
        );
        assert_eq!(
            error.as_deref(),
            Some("字幕保存失败：磁盘已满"),
            "失败本身必须可见（提示条是用户唯一的了解渠道）"
        );
        assert!(!dirty, "旧修订的失败不许把当前内容标脏");

        // 最新一条失败 + 当前修订：必须置脏，否则定时器不再重试
        error = None;
        let mut dirty = false;
        apply_segment_write_result(
            &mut error,
            &mut dirty,
            &synthetic_write(4, 9, Err("磁盘已满".to_string())),
            9,
            true,
        );
        assert!(dirty, "当前修订上的失败必须置脏以便重试");

        // 修订号判定的边界：同一修订才置脏，更新过的修订不置脏（纯函数直接钉住）
        let mut dirty = false;
        assert!(mark_segments_dirty_for_revision(&mut dirty, 9, 9));
        assert!(dirty);
        let mut dirty = false;
        assert!(!mark_segments_dirty_for_revision(&mut dirty, 8, 9));
        assert!(!dirty, "旧修订的失败不许把当前内容标脏");
    }

    /// 后台写失败**不能静默**：走一次真实的异步派发失败，等它把「字幕保存失败：…」
    /// 挂回 `db_write_error`，并把脏标记置回（下一次 flush 会重试）。
    ///
    /// 失败是**确定性**造出来的：库文件照常建、工程照常插，然后把库文件设成只读
    /// （模拟只读挂载 / 磁盘满这类持久性故障）。SQLite 在 WAL 模式下连写都起不来，
    /// `update_task_segments_json` 必然返回 Err——正是用户实际会撞上的那类错误，
    /// 也是「错误不许静默」这条要求的端到端验证。
    #[test]
    fn async_flush_surfaces_database_failure() {
        let dir = std::env::temp_dir().join(format!(
            "voice2word_bg_fail_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let db_path = dir.join("voice2word.db");
        let id = {
            let db = crate::storage::Database::open(&db_path).expect("打开文件库");
            let id = db
                .insert_task("fail.mp4", "fail.mp4", 5.0, "completed", &Vec::new(), None)
                .expect("先插一条空工程");
            // 把 WAL 合并回主库文件，保证随后只读打开时读得到这条记录。
            db.maintain_on_shutdown();
            id
        };
        let mut perms = std::fs::metadata(&db_path)
            .expect("库文件元数据")
            .permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&db_path, perms).expect("把库文件设为只读");

        let mut state = test_state(Vec::new());
        state.db = crate::storage::Database::open(&db_path).expect("只读打开（读没问题）");
        state.active_task_id = Some(id);
        state.segments = vec![Segment::new(1, 0.0, 1.0, "这一句写不进去")];
        state.bump_segments_revision();
        state.segments_dirty = true;

        state.flush_segments_if_dirty_async();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.db_write_error.is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
            state.poll_background_writes();
        }
        let text = state.db_write_error.clone().unwrap_or_default();
        assert!(
            text.contains("字幕保存失败"),
            "后台失败必须挂到提示通道（db_write_error），实际：{text:?}"
        );
        assert!(
            state.segments_dirty,
            "当前修订上的失败必须把脏标记置回，下一次 flush 才会重试"
        );

        // 收尾：恢复可写再整目录删掉（连同 -wal/-shm），不给下一次测试留垃圾。
        drop(state);
        let mut perms = std::fs::metadata(&db_path)
            .expect("收尾时读元数据")
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(&db_path, perms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 修复常见错误：命中才压撤销点；改动要标脏、推修订号、清词级时间戳，且可撤销。
    #[test]
    fn apply_cleanup_edits_text_and_is_undoable() {
        use crate::subtitle::cleanup::CleanupRule;

        let mut state = test_state(vec![
            Segment::new(1, 0.0, 1.0, "嗯，我来了。"),
            Segment::new(2, 1.0, 2.0, "这个这个"),
        ]);
        state.segments[1].words = vec![crate::subtitle::segment::WordTiming {
            word: "这个".to_string(),
            start: 1.0,
            end: 1.5,
        }];
        let revision = state.segments_revision;

        let plan = state.apply_cleanup();
        assert_eq!(plan.count_of(CleanupRule::Filler), 1);
        assert_eq!(state.segments[0].text, "我来了。");
        assert_eq!(state.segments[1].text, "这个");
        assert!(
            state.segments[1].words.is_empty(),
            "文本一变，词级时间戳必须清空"
        );
        assert!(state.segments_revision > revision, "清单缓存要失效");
        // 立刻写入的入口会把脏标记清掉（`flush_segments_if_dirty` 落库后清），
        // 所以这里看的是审计与修订号，而不是 `segments_dirty`。
        let logged = state
            .edit_log
            .of_kind(crate::subtitle::audit::EditKind::Replace);
        assert_eq!(logged.len(), 2, "语气词与重复字词各记一条摘要");
        assert!(
            logged[0].before.contains("未修复"),
            "摘要要写清改的是哪一类问题：{}",
            logged[0].before
        );
        assert!(state.cleanup_note.is_some(), "界面要能显示结果提示");
        assert!(state.can_undo(), "改动必须可撤销");
        assert!(state.undo());
        assert_eq!(state.segments[0].text, "嗯，我来了。");
    }

    /// 没有可改之处时不压撤销点：空操作占掉一层 Ctrl+Z 会让用户以为撤销坏了。
    #[test]
    fn apply_cleanup_without_hits_leaves_state_untouched() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 1.0, "你好世界")]);
        assert!(!state.can_undo());
        let plan = state.apply_cleanup();
        assert!(plan.is_empty());
        assert!(!state.can_undo(), "无命中不该产生撤销点");
        assert!(!state.segments_dirty, "无命中不该把工程标脏");
    }

    /// 规则全关时给出明确提示，而不是静默什么都不做。
    #[test]
    fn apply_cleanup_with_no_rules_points_at_the_toggles() {
        let mut state = test_state(vec![Segment::new(1, 0.0, 1.0, "嗯，我来了")]);
        state.cleanup_rules = crate::subtitle::cleanup::CleanupRules::NONE;
        let plan = state.apply_cleanup();
        assert!(plan.is_empty());
        let note = state.cleanup_note.clone().unwrap_or_default();
        assert!(note.contains("勾选"), "提示要指向勾选项：{note}");
    }

    /// 折叠重复句：句数减少、序号重排、时间跨度合并、可撤销。
    #[test]
    fn fold_duplicate_segments_merges_and_renumbers() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 1.0, "你好世界"),
            Segment::new(2, 1.0, 2.0, "你好世界"),
            Segment::new(3, 2.5, 3.5, "别的"),
        ]);
        state.selected_segment_index = Some(2);

        let removed = state.fold_duplicate_segments();
        assert_eq!(removed, 1);
        assert_eq!(state.segments.len(), 2);
        assert_eq!(state.segments[0].end, 2.0, "保留句的时间要盖住整组");
        assert_eq!(state.segments[1].index, 2, "删条后必须重排序号");
        assert_eq!(
            state.selected_segment_index,
            Some(1),
            "选中的那句被删掉后要落到现存的第一句"
        );
        // 立刻写入的入口会在落库后清掉脏标记，所以这里看审计留痕与撤销可用性。
        assert_eq!(
            state
                .edit_log
                .of_kind(crate::subtitle::audit::EditKind::Merge)
                .len(),
            1,
            "折叠属于结构性删除，审计必须留一条合并记录"
        );
        assert!(state.can_undo());
        assert!(state.undo());
        assert_eq!(state.segments.len(), 3);

        // 删除发生在选中句之前时，选中的仍是同一句话（序号前移一位），
        // 而不是「序号不变、内容换人」。
        state.selected_segment_index = Some(3);
        state.fold_duplicate_segments();
        assert_eq!(state.segments.len(), 2);
        assert_eq!(state.selected_segment_index, Some(2));
        assert_eq!(
            state
                .segments
                .iter()
                .find(|s| s.index == 2)
                .map(|s| s.text.as_str()),
            Some("别的"),
            "选中的必须还是原来那一句"
        );
    }

    /// 没有重复句时不产生撤销点、也不假装删了东西。
    #[test]
    fn fold_duplicate_segments_is_a_noop_without_duplicates() {
        let mut state = test_state(vec![
            Segment::new(1, 0.0, 1.0, "甲句"),
            Segment::new(2, 1.0, 2.0, "乙句"),
        ]);
        assert_eq!(state.fold_duplicate_segments(), 0);
        assert!(!state.can_undo());
        assert_eq!(state.segments.len(), 2);
    }
    /// 同工程的写按序号收口：更新的内容落库后，更旧的快照必须被跳过而不是盖回去。
    ///
    /// 回归护栏：写入闸只保证「不交错」，普通 `Mutex` 不保证先到先得——没有按工程的
    /// 水位，两个在飞的写完全可能「新的先写、旧的后写」，库里留下旧内容却没有任何提示。
    #[test]
    fn stale_snapshot_is_skipped_after_a_newer_write_landed() {
        use crate::storage::Database;
        let db = Database::open(":memory:").expect("内存库应能打开");
        let id = db
            .insert_task(
                "floor.mp4",
                "floor.mp4",
                3.0,
                "completed",
                &[Segment::new(1, 0.0, 3.0, "新内容")],
                None,
            )
            .expect("先插一条带内容的工程");
        let writes = BackgroundWrites::default();
        // 模拟「更新的写已经落库」：同工程水位已经是 7。
        writes.tracker.note_applied(id, 7);

        let stale = SegmentWriteSnapshot {
            task_id: id,
            revision: 3,
            json: serde_json::to_string(&vec![Segment::new(1, 0.0, 3.0, "旧内容")]).unwrap(),
        };
        let guard = writes.lock_writer();
        let wrote = write_snapshot_under_gate(&writes, &db, &stale, 5).expect("跳过不是失败");
        drop(guard);
        assert!(!wrote, "更旧的快照必须被跳过");
        assert_eq!(
            db.load_task_segments(id).expect("读回字幕")[0].text,
            "新内容",
            "旧内容不许盖掉新内容"
        );

        // 反例：序号比水位新时照常写入——水位不是「一写就锁死」的开关。
        let newer = SegmentWriteSnapshot {
            task_id: id,
            revision: 4,
            json: serde_json::to_string(&vec![Segment::new(1, 0.0, 3.0, "更新内容")]).unwrap(),
        };
        let guard = writes.lock_writer();
        let wrote = write_snapshot_under_gate(&writes, &db, &newer, 9).expect("写入应成功");
        drop(guard);
        assert!(wrote);
        assert_eq!(
            db.load_task_segments(id).expect("读回字幕")[0].text,
            "更新内容"
        );
    }

    /// 同步落库要把「同工程水位」推到自己领的序号上：落后于它的旧回报不许再回写状态。
    ///
    /// 回归护栏：水位原先只有「读」没有「写」（全代码库没有一处 store），两条判据永远
    /// 不成立——同步写完成后，一条迟到的旧失败照样会把「字幕保存失败」挂上提示条，
    /// 而那份内容其实已经写进库了。
    ///
    /// 交错序列完全按真实链路摆：旧快照先派发（不 await）→ 用户又改一版 → 同步落库
    /// （换工程 / 导出 / 关窗）把它写掉 → 旧快照的失败结局这时才回来。
    #[test]
    fn sync_flush_records_task_watermark_and_drops_late_stale_failure() {
        let mut state = test_state(Vec::new());
        let id = state
            .db
            .insert_task("sync.mp4", "sync.mp4", 2.0, "completed", &[], None)
            .expect("插一条工程");
        state.active_task_id = Some(id);

        // 第一代：派发出去（seq 1），但先不让它收尾。
        state.segments = vec![Segment::new(1, 0.0, 2.0, "旧内容")];
        state.bump_segments_revision();
        state.segments_dirty = true;
        state.flush_segments_if_dirty_async();
        let stale_seq = 1;

        // 用户又改一版，然后走同步落库：它领到更大的写序号。
        state.segments = vec![Segment::new(1, 0.0, 2.0, "最新内容")];
        state.bump_segments_revision();
        state.segments_dirty = true;
        state.flush_segments_if_dirty();

        assert!(
            state.segments_writes.tracker.applied_seq(id) > stale_seq,
            "同步写必须把同工程水位推到自己领的号上"
        );
        assert!(!state.segments_dirty, "同步写完就是干净状态");

        // 旧快照的失败结局现在才回来（乱序到达）：它描述的内容已经被同步写覆盖，
        // 既不许挂提示条，也不许把内容标脏。
        state
            .segments_writes
            .submit
            .send(SegmentWriteDone {
                seq: stale_seq,
                task_id: id,
                revision: state.segments_revision,
                error_at_dispatch: None,
                result: Err("库被锁".to_string()),
            })
            .expect("投递回报");
        state.poll_background_writes();
        assert!(
            state.db_write_error.is_none(),
            "被同步写越过的迟到失败不许挂提示条：{:?}",
            state.db_write_error
        );
        assert!(!state.segments_dirty, "内容已经在库里，不许再标脏");
        assert_eq!(
            state.db.load_task_segments(id).expect("读回字幕")[0].text,
            "最新内容",
            "库里必须是同步写那一版"
        );
    }

    /// 提示条竞价输了的失败：提示条不动，但当前内容必须补上脏标记（否则没有重试）。
    #[test]
    fn banner_suppressed_failure_still_marks_current_revision_dirty() {
        let pre_drain = Some("历史记录保存失败：库被锁".to_string());
        let mut error = pre_drain.clone();
        let mut dirty = false;
        // 派发时提示条是干净的（synthetic_write 的 error_at_dispatch 为 None），
        // 之后别的落库挂上了更新的事实 → 这条失败失去回写提示条的资格。
        let failure = synthetic_write(4, 9, Err("磁盘已满".to_string()));
        drain_segment_writes(&mut error, &mut dirty, 9, pre_drain.as_deref(), &[failure]);
        assert_eq!(
            error.as_deref(),
            Some("历史记录保存失败：库被锁"),
            "更新的提示条不许被旧失败覆盖"
        );
        assert!(
            dirty,
            "失败描述的正是当前内容：必须置脏，否则这次失败既不提示也不重试"
        );

        // 落在旧修订上的同类失败仍然不许置脏（原有边界保持不变）。
        let pre_drain = Some("别的错误".to_string());
        let mut error = pre_drain.clone();
        let mut dirty = false;
        let stale = synthetic_write(5, 3, Err("磁盘已满".to_string()));
        drain_segment_writes(&mut error, &mut dirty, 9, pre_drain.as_deref(), &[stale]);
        assert!(!dirty, "旧修订的失败不许把当前内容标脏");
        assert_eq!(error.as_deref(), Some("别的错误"));
    }

    /// 派发前的「同工程水位」判据：已经被更新的写覆盖的快照不再派发，也不算失败。
    ///
    /// 「水位 ≥ 待发号」这个输入是**人工构造**的（生产中领号只在渲染线程且单调递增，
    /// 走不到这条分支）。这里钉的是分支语义本身：以后若把判据改成 `>`、或者把
    /// 「跳过」改成「报失败 / 照样起线程」，这条测试会立刻红。
    #[test]
    fn dispatch_skips_snapshot_already_covered_by_task_floor() {
        let mut state = test_state(Vec::new());
        state.active_task_id = Some(42);
        state.segments = vec![Segment::new(1, 0.0, 1.0, "内容")];
        state.segments_dirty = true;
        let next = state
            .segments_writes
            .tracker
            .next_seq
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(next, 1, "写序号从 1 开始（0 是「还没写过」的哨兵）");
        // 模拟：同工程的同步写（或更新的后台写）已经完成，水位 = 下一个待发号。
        state.segments_writes.tracker.note_applied(42, next);
        let snapshot = state.segments_write_snapshot().expect("有主键就有快照");
        assert_eq!(
            state.dispatch_segment_write(snapshot),
            SegmentDispatch::Saved,
            "被更新的写覆盖时必须按「已保存」处理，不能报失败"
        );
        assert_eq!(
            state.background_segments_write_pending(),
            0,
            "跳过的派发不许起线程"
        );
        assert_eq!(
            state
                .segments_writes
                .tracker
                .next_seq
                .load(std::sync::atomic::Ordering::SeqCst),
            next,
            "跳过的派发不领号（否则水位相对序号就错位了）"
        );
    }
    /// 诊断计数器（在飞快照数）必须有去有回：只加不减会让它随使用时长单调上涨，
    /// 变成一个骗人的数（旧实现只在「线程起不来」那条路上减过一次）。
    #[test]
    fn in_flight_write_gauge_returns_to_zero_after_completion() {
        let mut state = test_state(Vec::new());
        let id = state
            .db
            .insert_task("gauge.mp4", "gauge.mp4", 1.0, "completed", &[], None)
            .expect("插一条工程");
        state.active_task_id = Some(id);
        state.segments = vec![Segment::new(1, 0.0, 1.0, "内容")];
        state.bump_segments_revision();
        state.segments_dirty = true;
        state.flush_segments_if_dirty_async();

        // 派发那一刻就要能看到「有一份在飞」——这正是旧实现的坑：计数跑到线程体里，
        // 派发返回到线程开跑之间它是 0，同步落库会以为自己可以走了。
        assert!(
            state.background_segments_write_pending() >= 1,
            "派发之后「在飞」计数必须立刻可见"
        );

        // 等「结果真的进通道」（pending 归零）+ 水位落下来，不是瞎睡。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (state.background_segments_write_pending() > 0
            || state.segments_writes.tracker.applied_seq(id) == 0)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state.poll_background_writes();

        assert_eq!(
            state.background_segments_write_pending(),
            0,
            "后台写应在期限内收尾"
        );
        assert_eq!(
            state
                .segments_writes
                .in_flight_writes
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "写结束后在飞计数必须归零（只增不减的诊断数字会骗人）"
        );
    }

    /// 转写收尾的脏字幕回写必须过闸：同工程已有更新的写落库时，旧的收尾快照不许盖回去。
    ///
    /// 回归护栏：这条路径原先直接 `db.update_task_segments(id, segs)`，既没领号也没查水位，
    /// 于是「后台在飞的新写」与「收尾的旧写」撞同一个 task 时，旧内容会后落地把新内容盖掉。
    #[test]
    fn finish_flush_write_is_gated_and_drops_stale_snapshot() {
        use crate::storage::Database;

        let db = Database::open(":memory:").expect("内存库应能打开");
        let id = db
            .insert_task("finish.mp4", "finish.mp4", 2.0, "completed", &[], None)
            .expect("插一条工程");
        // 与 AppState 内部同一个 BackgroundWrites：派发 / 同步写 / 过闸入口三方共用
        // （AppState::new 里正是把这个 Arc 同时交给 segments_writes 与 segments_write_gate）。
        let writes = Arc::new(BackgroundWrites::default());
        let gate = SegmentWriteGate {
            db: db.clone(),
            writes: Arc::clone(&writes),
        };

        // 用户又改了一版，走同步落库（换工程 / 导出）：它领到更大的序号并把水位推高。
        let newer = vec![Segment::new(1, 0.0, 2.0, "最新内容")];
        let newer_seq = writes.tracker.next_seq.fetch_add(1, SeqCst);
        {
            let _guard = writes.lock_writer();
            db.update_task_segments(id, &newer).expect("新内容先落库");
            writes.tracker.note_applied(id, newer_seq);
        }

        // 转写收尾这时才带着**旧**序号回写上一个工程的脏字幕。
        let stale = vec![Segment::new(1, 0.0, 2.0, "旧内容")];
        let wrote = gate
            .write_finished_task_segments(id, newer_seq.saturating_sub(1), &stale)
            .expect("被跳过不是失败");
        assert!(!wrote, "更旧的收尾写必须被水位挡下");
        assert_eq!(
            db.load_task_segments(id).expect("读回字幕")[0].text,
            "最新内容",
            "旧收尾快照不许盖掉更新的写"
        );

        // 反例：序号比水位新时照常写、并把水位推到自己——闸门不是「一写就锁死」。
        let fresh = vec![Segment::new(1, 0.0, 2.0, "收尾写的内容")];
        let fresh_seq = writes.tracker.next_seq.fetch_add(1, SeqCst);
        let wrote = gate
            .write_finished_task_segments(id, fresh_seq, &fresh)
            .expect("写入应成功");
        assert!(wrote);
        assert_eq!(
            db.load_task_segments(id).expect("读回字幕")[0].text,
            "收尾写的内容"
        );
        assert_eq!(
            writes.tracker.applied_seq(id),
            fresh_seq,
            "过闸写必须把同工程水位推到自己"
        );
    }

    /// 去抖派发**不清脏**，脏标记只在这一份内容被确认落库后才清。
    ///
    /// 回归护栏（本轮修的真实漏洞）：旧实现在派发成功那一刻就清 `segments_dirty`。于是
    /// 「改完 → 去抖派发 → 用户立刻关窗」这条链上，`flush_segments_before_close` 读到的
    /// `had_dirty` 是 `false`——它不会做同步落库，也不会因为在飞写的失败而拦住关窗；那份在飞
    /// 写一旦失败（磁盘满 / 库被锁），失败回报要等下一次 poll 才被看到，而窗口已经关了。
    ///
    /// 这条测试钉住不变量：**派发返回时脏标记必须还在**（内容只是在飞、还没落库），只有后台
    /// 成功收进状态之后才允许清掉。
    #[test]
    fn segments_dirty_survives_dispatch_until_write_confirmed() {
        use crate::storage::Database;

        let mut state = test_state(Vec::new());
        let db = Database::open(":memory:").expect("内存库应能打开");
        let id = db
            .insert_task("keep.mp4", "keep.mp4", 1.0, "completed", &[], None)
            .expect("插一条工程");
        state.db = db;
        state.active_task_id = Some(id);
        state.segments = vec![Segment::new(1, 0.0, 1.0, "改动")];
        state.bump_segments_revision();
        state.segments_dirty = true;

        let dispatched = state.flush_segments_if_dirty_async();

        assert!(dispatched, "有主键时应派发到后台（返回「有写在飞」）");
        assert!(
            state.segments_dirty,
            "派发只是把快照交出去、内容还没落库：脏标记必须保留，否则关窗守卫会在在飞写没落地时放行"
        );

        // 后台写完成、回报收进状态 → 这时才允许清脏。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.segments_dirty && std::time::Instant::now() < deadline {
            state.poll_background_writes();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        assert!(
            !state.segments_dirty,
            "后台写成功收口后脏标记必须清掉（否则界面会一直显示未保存）"
        );
        assert!(state.db_write_error.is_none(), "这条路径不该产生写库错误");
        let loaded = state.db.load_task_segments(id).expect("读回后台写的字幕");
        assert_eq!(loaded[0].text, "改动", "内容必须真的落库");
    }
}

/// 新增接口（术语导入导出 / CPS 修正 / 阶段产物目录）的定向单测。
///
/// 刻意不写 `use super::…` 这种通配导入：只显式引入被测项，把实现细节挡在测试命名空间之外——
/// 断言写出来是什么就是什么，不会因一条新增的私有 helper 而悄悄改义。
#[cfg(test)]
mod glossary_cps_and_stage_artifacts {
    use super::AppState;
    use crate::subtitle::Segment;
    use std::path::{Path, PathBuf};

    /// 造一个不碰显卡 / 真实 `config.toml` 的最小 AppState。
    fn fresh_state(segments: Vec<Segment>) -> AppState {
        use crate::engines::{FFmpegEngine, HardwareProfile, LLMEngine, WhisperEngine};
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
            HardwareProfile::cpu_only(),
        );
        state.segments = segments;
        state
    }

    /// 临时目录里的唯一名字（避免同批测试互相踩）。
    fn unique(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("v2w_{tag}_{nanos}"))
    }

    /// 导入 `.txt`：新增 / 覆盖 / 未变三个数必须如实分桶，且合并结果写回术语表字段。
    #[test]
    fn import_glossary_file_counts_added_overwritten_unchanged() {
        let dir = unique("gloss_txt");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("incoming.txt");
        // b / C 命中已有条目（b 换了译法、C 逐字相同），D 是新的。
        std::fs::write(&src, "b=B2\nC=丙\nD=丁\n").unwrap();

        let cfg = dir.join("config.toml");
        let mut state = fresh_state(Vec::new());
        state.config.translate.glossary = "A=甲\nB=乙\nC=丙\n".to_string();

        let (added, overwritten, unchanged) = state
            .import_glossary_file_to(&src, &cfg)
            .expect("导入应成功");

        assert_eq!((added, overwritten, unchanged), (1, 1, 1));
        // 覆盖留在原位、新增追加在后：b 的译法已更新、D 已写入、A 一字未动。
        let text = state.config.translate.glossary.clone();
        assert!(text.contains("b=B2"), "覆盖后译文应更新: {text}");
        assert!(text.contains("D=丁"), "新增条目应写回: {text}");
        assert!(text.contains("A=甲"), "未命中的旧条目必须保留: {text}");
        assert_eq!(
            text.lines().next(),
            Some("A=甲"),
            "覆盖应留在原位，不挪到末尾"
        );
        assert!(cfg.is_file(), "必须走既有落盘入口写出配置文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导入 `.csv`：按扩展名选 `from_csv`，带 BOM 与表头也能读。
    #[test]
    fn import_glossary_file_reads_csv_by_extension() {
        let dir = unique("gloss_csv");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("incoming.csv");
        std::fs::write(&src, "\u{FEFF}source,target\nOBS,obs studio\n").unwrap();

        let cfg = dir.join("config.toml");
        let mut state = fresh_state(Vec::new());
        let (added, overwritten, unchanged) = state
            .import_glossary_file_to(&src, &cfg)
            .expect("导入应成功");

        assert_eq!((added, overwritten, unchanged), (1, 0, 0));
        assert!(state.config.translate.glossary.contains("OBS=obs studio"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出 CSV：表头固定，含逗号的字段按 RFC4180 加引号。
    #[test]
    fn export_glossary_csv_text_writes_header_and_escapes() {
        let mut state = fresh_state(Vec::new());
        state.config.translate.glossary = "苹果=apple\na,b=c\n".to_string();

        let csv = state.export_glossary_csv_text();
        assert!(csv.starts_with("source,target\n"), "首行必须是表头: {csv}");
        assert!(csv.contains("苹果,apple\n"));
        assert!(csv.contains("\"a,b\",c\n"), "含逗号字段必须加引号: {csv}");
    }

    /// CPS 修正：有改动压撤销点且可撤销；无改动一个撤销点都不许压。
    #[test]
    fn apply_cps_fix_is_undoable_and_noop_pushes_nothing() {
        // 10 个汉字塞进 1 秒 = 10 字/秒 > 默认上限 9 → 必然被延长。
        let mut state = fresh_state(vec![Segment::new(1, 0.0, 1.0, "一二三四五六七八九十")]);
        let plan = state.apply_cps_fix(crate::subtitle::DEFAULT_MAX_CPS, 0.0);

        assert!(plan.has_changes(), "超速句必须被规划进改动");
        assert!(state.can_undo(), "有改动必须压一层撤销点");
        assert!(
            state.segments[0].end > 1.0,
            "终点应被延长: {:?}",
            state.segments[0]
        );

        assert!(state.undo(), "撤销应成功");
        assert!(
            (state.segments[0].end - 1.0).abs() < 1e-9,
            "撤销后终点回到原值: {:?}",
            state.segments[0]
        );

        // 已经读得完的句子：无改动，不许占撤销栈、不许动时间。
        let mut calm = fresh_state(vec![Segment::new(1, 0.0, 5.0, "短句")]);
        let before = calm.can_undo();
        let plan = calm.apply_cps_fix(crate::subtitle::DEFAULT_MAX_CPS, 0.0);
        assert!(!plan.has_changes());
        assert_eq!(calm.can_undo(), before, "无改动不许压撤销点");
        assert!(!calm.can_undo());
        assert!(
            (calm.segments[0].end - 5.0).abs() < 1e-9,
            "无改动不许动时间"
        );
    }

    /// 只读薄封装：CPS 违规下标与标点复核都按 `segments` 现算。
    #[test]
    fn cps_and_punctuation_wrappers_report_without_mutating() {
        let state = fresh_state(vec![
            Segment::new(1, 0.0, 1.0, "一二三四五六七八九十"),
            Segment::new(2, 2.0, 6.0, "没问题。"),
        ]);
        assert_eq!(
            state.cps_violation_indices(crate::subtitle::DEFAULT_MAX_CPS),
            vec![1]
        );
        let plan = state.punctuation_plan();
        assert_eq!(plan.indices, vec![1], "第一句没有句尾标点");
        assert_eq!(plan.skipped, 0);
    }

    /// 续跑目录口径：桌面端与 CLI 必须算出同一个目录（同级 `.voice2word-stages/<stem>`）。
    #[test]
    fn stage_artifact_dir_matches_cli_convention() {
        let media = Path::new("media").join("clips").join("talk.mp4");
        let got = AppState::stage_artifact_dir(&media);
        let want = Path::new("media")
            .join("clips")
            .join(".voice2word-stages")
            .join("talk");
        assert_eq!(got, want);

        // 无父目录时与 CLI 一样回落到当前目录。
        assert_eq!(
            AppState::stage_artifact_dir(Path::new("talk.mp4")),
            Path::new(".").join(".voice2word-stages").join("talk")
        );
    }

    /// 开工时建目录并挂到管线；可一键关掉；相对路径不会被误当作可续跑位置。
    #[test]
    fn prepare_stage_artifacts_creates_dir_and_attaches_then_can_be_disabled() {
        let root = unique("stage_prepare");
        std::fs::create_dir_all(&root).unwrap();
        let media = root.join("talk.mp4");
        std::fs::write(&media, b"x").unwrap();

        let state = fresh_state(Vec::new());
        assert_eq!(state.stage_artifacts_dir(), None, "开局必须是关闭状态");

        let dir = state.prepare_stage_artifacts(&media).expect("应能建目录");
        assert_eq!(dir, root.join(".voice2word-stages").join("talk"));
        assert!(dir.is_dir(), "真正开工时才创建目录");
        assert_eq!(state.stage_artifacts_dir(), Some(dir));

        state.disable_stage_artifacts();
        assert_eq!(state.stage_artifacts_dir(), None);

        // 相对路径（没有真实父目录）必须被挡下：不许在仓库根散落 `.voice2word-stages`。
        assert!(state
            .prepare_stage_artifacts(Path::new("talk.mp4"))
            .is_err());
        assert_eq!(state.stage_artifacts_dir(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 旧产物清理：超出保留额度的最旧子目录被整体删除，额度内一个都不动。
    #[test]
    fn prune_stale_stage_dirs_keeps_only_recent() {
        use super::prune_stale_stage_dirs;

        let root = unique("stage_prune");
        std::fs::create_dir_all(&root).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("01_extract_audio.json"), b"{}").unwrap();
        }

        // keep = 0：三个子目录都超出额度、理应全被清掉（与 mtime 顺序无关，断言稳定）。
        assert_eq!(prune_stale_stage_dirs(&root, 0), 3);
        for name in ["a", "b", "c"] {
            assert!(!root.join(name).exists(), "{name} 应被删掉");
        }
        // 根目录自身留着；空目录上再跑一次不 panic。
        assert!(root.is_dir());
        assert_eq!(prune_stale_stage_dirs(&root, 8), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
