//! 应用程序全局状态

use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use crate::core::TaskPipeline;
use crate::engines::TranslateMode;
use crate::engines::{HardwareProfile, ProxyManager};
use crate::storage::{Database, TaskRecord};
use crate::subtitle::Segment;
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
    Balanced, // 均衡 Small — ggml-small.bin / ggml-small-q5_0.bin (181MB)
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
                    if crate::utils::AppConfig::resolve_path(
                        "models/whisper/ggml-large-v3-turbo-q8_0.bin",
                    )
                    .exists()
                    {
                        "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string()
                    } else if crate::utils::AppConfig::resolve_path(
                        "models/whisper/ggml-large-v3.bin",
                    )
                    .exists()
                    {
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
    /// 范围选择的锚点：按住 Shift 连点时，从这一句一直选到当前句。
    ///
    /// 只存一个 `usize` 而不是一组下标：字幕 `index` 恒为 1..=n 的连续编号
    /// （每次增删都会 `reindex_segments`），因此「一段区间」用首尾两个下标就够，
    /// 不必再维护一个会与 `segments` 失步的集合。
    pub selection_anchor: Option<usize>,
    pub current_time: f64,
    pub editing_text: String,
}

/// 「0 秒智能缓存命中」查询结果的共享槽位：(文件路径, 命中记录)。
/// `None` 表示「查过库、确实没有缓存」。抽成别名只为给这个嵌套类型起个名字，
/// 暴露出的形状与语义和原字段完全一致。
pub type CachedTranscription = Arc<std::sync::Mutex<Option<(String, Option<Arc<TaskRecord>>)>>>;

mod edit;
mod queue;
mod translate;
mod writes;

/// 新增接口（术语导入导出 / CPS 修正 / 阶段产物目录）的定向单测。
///
/// 刻意不写 `use super::…` 这种通配导入：只显式引入被测项，把实现细节挡在测试命名空间之外——
/// 断言写出来是什么就是什么，不会因一条新增的私有 helper 而悄悄改义。
#[cfg(test)]
mod glossary_cps_and_stage_artifacts;
#[cfg(test)]
mod tests;

pub(crate) use writes::SegmentWriteGate;
use writes::{BackgroundWrites, PendingWait};

/// 转写步骤看板里一行的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageRowState {
    /// 还没轮到
    Pending,
    /// 正在跑（进度条会动、秒数每秒刷新）
    Running,
    /// 已完成
    Done,
    /// 本次任务不会执行（例如关闭了润色、没触发救场）
    Skipped,
}

/// 转写步骤看板的一行：一个流水线阶段 + 它的进度与耗时。
#[derive(Debug, Clone)]
pub struct StageRow {
    pub stage: crate::core::Stage,
    pub state: StageRowState,
    /// 该阶段自己的进度 0.0..=1.0（不是全局进度）
    pub progress: f32,
    /// 已完成阶段的耗时（秒）。进行中的那一行用 `AppState::stage_row_started_at` 实时算。
    pub secs: f64,
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
    /// 转写步骤看板：七个流水线阶段一行一个，**点开始就全部列出来**。
    ///
    /// 为什么要在状态里存一份而不是直接从 `ProcessStatus` 推导：
    /// `ProcessStatus` 只有「当前阶段」一个字符串，用户看不出「一共几步、走到第几步、
    /// 前面几步各花了多久」。步骤明细需要跨事件累积，所以单独维护。
    pub stage_rows: Vec<StageRow>,
    /// 当前正在跑的那一步的开始时刻（算实时秒数用）。
    pub stage_row_started_at: Option<std::time::Instant>,
    /// 秒数刷新用的代次：每次开始新任务 +1，旧的刷新循环据此自行退出。
    pub stage_rows_tick: u64,

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

    /// 文件夹监听已入队过的路径（跨轮次去重）。
    ///
    /// 只放内存：重启后重新扫一遍目录也无害——队列本身按路径去重，重复发现不会
    /// 产生重复条目；把这份集合落盘反而要处理「文件被删/改名后集合膨胀」。
    pub watch_seen: std::collections::HashSet<std::path::PathBuf>,

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
    /// 与 `segments_write_pending` 配套的唤醒停车位（同步落库等收尾用，见 [`PendingWait`]）。
    segments_write_wait: Arc<PendingWait>,

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
    /// 文档代次：**每次整份替换字幕表**（导入、转写完成、打开工程）就 +1。
    ///
    /// 翻译任务开工时记下代次，收尾/增量落盘时比对：不一致说明用户已经切走了，
    /// 这份结果属于上一个工程，绝不能并进现在这张表——那是最难排查的数据污染。
    pub doc_epoch: u64,
    /// 关窗守卫的「二次确认」标记：导入字幕的工程没有库主键，第一次关窗先警告，
    /// 第二次才放行（否则用户被永久锁在窗口里，只能强杀进程）。
    pub draft_close_warned: bool,

    // 硬件与模型资源监控
    pub metrics: ResourceMetrics,

    // 剪辑校对工作台状态 (主界面)
    pub active_tab: WorkspaceTab,
    pub current_time: f64,
    pub total_duration: f64,
    pub is_playing: bool,
    pub selected_segment_index: Option<usize>,
    /// 范围选择的锚点：按住 Shift 连点时，从这一句一直选到当前句（见 `selected_span`）。
    pub selection_anchor: Option<usize>,
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
    /// 每个条目**自己**的下载进度：`id -> (已下载字节, 总字节)`。
    ///
    /// 为什么要有它：以前只有一个全局进度条，串行下载时够用，但用户要的是
    /// 「进度显示在条目自己那一行」并允许并发——那就必须按 id 分开记。
    /// 空表示该条目当前没在下（下载完成 / 失败后立刻移除）。
    pub download_item_progress: std::collections::HashMap<String, (u64, u64)>,

    /// 鼠标当前悬停的模型条目 id（`None` = 没悬停）。用来渲染「配置要求」悬浮卡。
    ///
    /// 放在状态里而不是行内部：卡片要浮在**行之外**（否则会被行高裁掉），
    /// 只有把「谁被悬停」提到列表层才能定位。
    pub hovered_model_id: Option<&'static str>,
    /// 最近一次下载的结果提示（成功或失败原因）
    pub download_status_msg: String,
    /// 最近一次字幕写库失败的原因（`None` = 一切正常）。
    ///
    /// 存在的意义：写库失败此前被 `let _ =` 完全吞掉，用户改完字幕、切页面、
    /// 关程序，重启后发现改动全没了，而界面上从未出现任何异常。现在把它
    /// 渲染成可见提示条，让「没保存上」这件事至少是可被发现的。
    pub db_write_error: Option<String>,

    /// 启动时的一次性提示（例如历史库损坏后被隔离重建）。
    ///
    /// 与 `db_write_error` 分开：那条语义是「改动没写进库」，会附上「请检查磁盘空间
    /// 或权限」的处置建议；这里要说的是一次已完成的启动事件，不能套用同一句话。
    /// `MainWindow::new` 在构造时把它搬进 `notice` 提示条并清空。
    pub startup_notice: Option<String>,

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

/// 翻译任务开工时的句子基线：收尾合并与增量落盘时的冲突判定依据
/// （见 [`AppState::merge_translations_checked`]）。
#[derive(Clone, Default)]
pub struct TranslationBaseline {
    /// `Segment::index` -> 开工时的源文本（`translate_source()`）
    pub source: std::collections::HashMap<i64, String>,
    /// `Segment::index` -> 开工时的译文（空串视为 `None`）
    pub translation: std::collections::HashMap<i64, Option<String>>,
}

impl TranslationBaseline {
    pub fn capture(segments: &[Segment]) -> Self {
        let mut source = std::collections::HashMap::with_capacity(segments.len());
        let mut translation = std::collections::HashMap::with_capacity(segments.len());
        for seg in segments {
            let idx = seg.index as i64;
            source.insert(idx, seg.translate_source());
            translation.insert(
                idx,
                seg.translation.clone().filter(|t| !t.trim().is_empty()),
            );
        }
        Self {
            source,
            translation,
        }
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
        // 历史库若在启动时被隔离重建（确认损坏才会发生），把这件事带进界面：
        // 「记录全没了」必须当面说清，并给出备份路径与抢救办法。
        let startup_notice = db.take_heal_notice().map(|backup| {
            format!(
                "历史库损坏，已备份为 {} 并新建空库：历史记录暂时为空。\
                 如需抢救旧数据，可用 sqlite3 对该备份执行 .recover。",
                backup.display()
            )
        });
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
            stage_rows: Vec::new(),
            stage_row_started_at: None,
            stage_rows_tick: 0,
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
            watch_seen: std::collections::HashSet::new(),
            segments_writes: writes_arc.clone(),
            // 过闸入口与派发 / 同步写共用**同一个** `BackgroundWrites`（同一把写入闸、
            // 同一份按工程水位）：另起一份账本等于给它单开一条不受水位约束的写库通道，
            // 正是这次要堵的那类「旧内容覆盖新内容」。
            segments_write_gate,
            segments_write_pending: Arc::new(AtomicUsize::new(0)),
            segments_write_wait: Arc::new(PendingWait::default()),
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
            doc_epoch: 0,
            draft_close_warned: false,
            metrics: ResourceMetrics::default(),

            active_tab: WorkspaceTab::Editor, // 默认主界面为剪辑校对工作台
            current_time: 0.0,
            total_duration: 0.0,
            is_playing: false,
            selected_segment_index: None,
            selection_anchor: None,
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
            download_item_progress: std::collections::HashMap::new(),
            hovered_model_id: None,
            download_status_msg: String::new(),
            // 首次扫描放在 `with_hardware` 收尾处（那里能拿到 &mut self）
            cached_transcription: Arc::new(std::sync::Mutex::new(None)),
            db_write_error: None,
            startup_notice,
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
    /// 落盘到 `config.toml`。
    ///
    /// **单测里整个方法被编译成空实现**：`test_state` 用的是 `AppConfig::default()`，
    /// 而设置页 / 翻译 / 线程数这些 setter 都会调它。原先「跑一遍测试」就会把仓库根的
    /// `config.toml` 覆盖成默认值——用户改过的 `target_lang` 之类被静默清零，且没有
    /// 任何提示（这条是实测踩到的：跑完测试后 `target_lang` 从「日本語」变回「简体中文」）。
    ///
    /// 需要验证落盘行为的测试请用 [`Self::persist_config_to`] 指到临时目录，
    /// 那也是本文件其余「可注入路径」方法的既有约定。
    #[cfg(not(test))]
    pub(crate) fn persist_config(&mut self) {
        self.persist_config_to("config.toml");
    }

    /// 单测版本：不写盘（见上方说明）。
    #[cfg(test)]
    pub(crate) fn persist_config(&mut self) {
        let _ = self;
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
    #[cfg(not(test))]
    pub(crate) fn end_config_drag(&mut self) {
        self.end_config_drag_to("config.toml");
    }

    /// 单测版本：只关闸门、不写盘。
    #[cfg(test)]
    pub(crate) fn end_config_drag(&mut self) {
        self.config_drag_active = false;
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
        // 这个文件是**让用户用记事本改的**：中文 Windows 上记事本/编辑器的默认保存
        // 编码常常是 GBK，严格 UTF-8 读取会直接失败，界面只能报一句看不懂的 io 错误。
        let raw = std::fs::read(&path)?;
        let (text, non_utf8) = crate::utils::decode_text_bytes(&raw);
        if non_utf8 {
            tracing::warn!("glossary.txt 不是 UTF-8（已按 GBK 兼容读取）：建议另存为 UTF-8");
        }
        self.config.translate.glossary = text;
        self.save_translate_config();
        Ok(self.config.translate.glossary_entries().len())
    }

    /// 从当前字幕里提取候选术语，以**注释行**追加到术语表，返回追加条数。
    ///
    /// 为什么写成注释（`# term（出现 N 次）`）而不是直接 `term=`：注释行不会被
    /// `glossary::parse` 当成条目，用户补上译文、删掉行首 `#` 就生效；反过来直接
    /// 写 `term=` 会产生「译文为空」的**非法条目**，让整份术语表多出一批跳过行。
    ///
    /// 追加而不是覆盖：用户手写的条目一个字都不能动。
    pub fn append_glossary_candidates(&mut self) -> std::io::Result<usize> {
        let sources: Vec<String> = self.segments.iter().map(|s| s.translate_source()).collect();
        let refs: Vec<&str> = sources.iter().map(|s| s.as_str()).collect();
        // `glossary_entries()` 给的是 `(原文, 译文)` 元组；候选提取只需要原文，
        // 但仍按 `GlossaryEntry` 传进去，让「已在表里的不再推荐」这条与解析口径同源。
        let existing: Vec<crate::utils::glossary::GlossaryEntry> = self
            .config
            .translate
            .glossary_entries()
            .into_iter()
            .map(|(source, target)| crate::utils::glossary::GlossaryEntry { source, target })
            .collect();
        let candidates = crate::utils::glossary::term_candidates(&refs, &existing, 40);
        if candidates.is_empty() {
            return Ok(0);
        }

        let mut text = self.config.translate.glossary.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str("# ==== 从当前字幕提取的候选词：补上「原文=译文」、去掉行首 # 即生效 ====\n");
        for (term, n) in &candidates {
            text.push_str(&format!("# {term}（出现 {n} 次）\n"));
        }
        self.config.translate.glossary = text;
        self.save_translate_config();
        // 让磁盘上的 glossary.txt 与内存一致，用户打开编辑器就能看到新候选。
        self.write_glossary_temp_file()?;
        Ok(candidates.len())
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
        // 导入的文件多半来自 Excel 的「另存为 CSV」——中文 Windows 上 Excel 默认写
        // **GBK/ANSI**，严格 UTF-8 读取必然失败，用户只会看到一条 io 错误，
        // 完全想不到「用记事本另存为 UTF-8」这一步。
        let raw = std::fs::read(path)?;
        let (text, non_utf8) = crate::utils::decode_text_bytes(&raw);
        if non_utf8 {
            tracing::warn!(
                path = %path.display(),
                "术语表文件不是 UTF-8（已按 GBK 兼容读取）：建议在 Excel 里选「CSV UTF-8」导出"
            );
        }
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
        // 时长回退必须取**优化后**的时间轴终点：`optimize_segments` 会平滑延展极短尾句
        // （最短可读 0.8s）、也会剔除无效/鬼影片段。此前用优化前的 `task.segments`
        // 取末句，`total_duration` 会比真实时间轴还短——`seek_to` 于是把播放指针夹在
        // 末尾之前，用户点时间轴最后一段跳不过去。与落库口径保持一致：
        // `db.update_task_segments` 写的就是 `MAX(duration, 末句 end)`。
        // 取 max(end) 而不是「最后一句」：segments 不保证按时间排序（见
        // [`crate::subtitle::timeline_end`]）。
        let timeline_end = crate::subtitle::timeline_end(&self.segments);
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
                // `existing_proxy` 现在只读缓存高度（不在这里起 ffmpeg 探测）。
                // 没有现成代理时先把预览源设回**原片**：这是本函数唯一安全的兜底——
                // 保持旧值会让界面继续播上一个工程的视频。
                self.preview_source = Some(
                    self.proxy_manager
                        .existing_proxy(src, h)
                        .unwrap_or_else(|| src.clone()),
                );
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
        // 与 `load_task_with` 同口径：取「基准时长」与优化后时间轴终点的较大者。
        let timeline_end = crate::subtitle::timeline_end(&self.segments);
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
