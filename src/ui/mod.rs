//! Voice2Word GPUI 界面主视窗
//! 遵循 Codex / Zed 极简现代深色风格与模块化组件架构

pub mod actions;
pub mod components;
pub mod dialogs;
pub mod editor;
pub mod primitives;
pub mod shortcuts;
pub mod theme;
pub mod types;
pub mod views;

pub use types::*;

use gpui::prelude::*;
use gpui::*;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

use crate::app::state::{AppState, ProcessStatus, WorkspaceTab};
use crate::subtitle::segment::QualityReport;
use crate::subtitle::stats::SubtitleStats;
use crate::subtitle::{indices_cover_segments, ExportMode, Segment};
use crate::ui::actions::{
    expected_translation_count, has_copied_translations, translated_out_count,
};
use theme::Theme;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorExportFormat {
    #[default]
    JianYing, // 剪映草稿
    JianYingFolder, // 剪映草稿（导出到自选文件夹，不写入本机草稿库）
    Srt,            // SRT 字幕
    Ass,            // ASS 特效字幕
    Fcpxml,         // FCPXML (达芬奇 / FCP)
    PremiereXml,    // Premiere XML
    Txt,            // TXT 纯文本
    Vtt,            // VTT 网页字幕
    Json,           // JSON 结构化字幕（无损字段，供程序化消费 / 质检）
    EbuTtD,         // EBU-TT-D (TTML，广播分发)
    NetflixTtal,    // Netflix TTAL (TTML，流媒体交付)
    Sbv,            // SBV 播放器字幕（SubViewer 两行式）
    Lrc,            // LRC 唱词（每行只带起点时间）
    TxtTs,          // 带时间码纯文本（`[00:01:23.456] 正文`，扩展名同为 .txt）
    BurnVideo,      // 字幕烧录视频（硬字幕合成，导出带字幕的新 mp4）
}

impl EditorExportFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::JianYing => "剪映草稿（一键直出）",
            Self::JianYingFolder => "剪映草稿（导出到文件夹）",
            Self::Srt => "SRT 字幕（.srt）",
            Self::Ass => "ASS 特效字幕（.ass）",
            Self::Fcpxml => "FCPXML（达芬奇 / FCP）",
            Self::PremiereXml => "Premiere XML（.xml）",
            Self::Txt => "TXT 纯文本（.txt）",
            Self::Vtt => "VTT 网页字幕（.vtt）",
            Self::Json => "JSON 结构化字幕（.json）",
            Self::EbuTtD => "EBU-TT-D 广播字幕（.ttml）",
            Self::NetflixTtal => "Netflix TTAL（.ttal）",
            Self::Sbv => "SBV 播放器字幕（.sbv）",
            Self::Lrc => "LRC 唱词（.lrc）",
            // 与 `Txt` 同扩展名：显示名必须点明「带时间码」，否则用户把它当纯文本
            Self::TxtTs => "时间码纯文本（.txt）",
            Self::BurnVideo => "烧录字幕视频（.mp4）",
        }
    }

    /// 持久化用的稳定字符串（写进 `config.ui.export_format`）。
    ///
    /// 刻意不用 `Debug` 名：那是给日志看的，改名会静默作废用户已保存的偏好；
    /// 这里的取值视为**对外契约**，只在新增格式时追加。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::JianYing => "jianying",
            Self::JianYingFolder => "jianying_folder",
            Self::Srt => "srt",
            Self::Ass => "ass",
            Self::Fcpxml => "fcpxml",
            Self::PremiereXml => "premiere_xml",
            Self::Txt => "txt",
            Self::Vtt => "vtt",
            Self::Json => "json",
            Self::EbuTtD => "ttml",
            Self::NetflixTtal => "ttal",
            Self::Sbv => "sbv",
            Self::Lrc => "lrc",
            // 下划线而非连字符：这是写进 config.toml 的持久化键，与 CLI 的 `txt-ts` 拼写分开
            Self::TxtTs => "txt_ts",
            Self::BurnVideo => "burn_video",
        }
    }

    /// 从配置字符串还原；未知/空串回落到默认（剪映草稿）。
    ///
    /// 容错是必要的：`config.toml` 是用户可以手改的文件，写了个不存在的格式名时
    /// 应该安静地用默认值启动，而不是让整个界面构造失败。
    pub fn from_config_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "jianying" | "jianying_local" => Self::JianYing,
            "jianying_folder" | "jianying_dir" => Self::JianYingFolder,
            "srt" => Self::Srt,
            "ass" => Self::Ass,
            "fcpxml" => Self::Fcpxml,
            "premiere_xml" | "premiere" | "xml" => Self::PremiereXml,
            "txt" => Self::Txt,
            "vtt" => Self::Vtt,
            "json" => Self::Json,
            "ttml" | "ebu_tt_d" => Self::EbuTtD,
            "ttal" | "netflix_ttal" => Self::NetflixTtal,
            "sbv" => Self::Sbv,
            "lrc" => Self::Lrc,
            // 连字符与下划线都认：config.toml 是用户可手改的文件，与 CLI 的 `txt-ts` 对齐
            "txt_ts" | "txt-ts" => Self::TxtTs,
            "burn_video" | "burn" | "hardsub" => Self::BurnVideo,
            _ => Self::default(),
        }
    }

    pub fn all() -> &'static [EditorExportFormat] {
        &[
            Self::JianYing,
            Self::JianYingFolder,
            Self::Srt,
            Self::Ass,
            Self::Fcpxml,
            Self::PremiereXml,
            Self::Txt,
            Self::Vtt,
            Self::Json,
            Self::EbuTtD,
            Self::NetflixTtal,
            Self::Sbv,
            Self::Lrc,
            Self::TxtTs,
            Self::BurnVideo,
        ]
    }
}

/// 剪辑工作台右侧检查器的两个主面板。
///
/// 「字幕样式」与「字幕翻译」此前是纵向堆叠、随面板一起滚动的两张卡片，
/// 面板高度有限时必须滚动才能看到下面那张。改成二选一的互斥视图后，
/// 顶部标头的一对分段选项直接切换，两张卡片不再同时占版面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorSubtitlePanel {
    /// 全局字幕样式与排版配置
    #[default]
    Style,
    /// 字幕多语言翻译
    Translate,
    /// 字幕统计与导出（时长 / 字数 / 语速 / 过长句 + 导出内容 / 格式）
    ///
    /// 导出控件原本常驻在右侧面板底部，切到哪个面板都占着一条高度；
    /// 现收进本面板，三个面板各自占满整个右侧高度，可用空间更宽。
    Stats,
}

/// 在线翻译 API 配置卡片里的三个文本输入框，用于把按键派发到正确的缓冲区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiField {
    Base,
    Model,
    Key,
    /// 本地翻译（离线 Qwen）的模型文件路径输入框。
    ///
    /// 为什么不复用 `Model`：两者语义不同——`Model` 是在线 API 的**模型名**
    /// （`cn:deepseek-v4.1-flash`），这里是磁盘上的 **GGUF 文件路径**，
    /// 落盘字段、校验规则、提示文案都不一样。
    LocalModelPath,
}

/// 样式排版参数微调字段，支持点击/双击直接就地输入数值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleField {
    FontSize,
    LetterSpacing,
    BottomMargin,
    LineSpacing,
    MaxChars,
}

/// 视频库查询缓存的键：`(关键字, 只看译文, 只看原文, 排序, 记录数, 首条 id)`。
///
/// 抽成别名有两个理由：clippy 会拒绝对裸元组当场写两层泛型（`type_complexity`），
/// 而更重要的是**这个键的形状就是缓存正确性的契约**——哪几项能影响结果，一眼可见。
/// 少一项就会返回过期结果，多一项只是多一点无谓重算。
pub(crate) type LibraryQueryKey = (String, bool, bool, u8, usize, i64);

/// 「查找替换」面板里的两个文本输入框。
///
/// 与 [`ApiField`] 同一套理由：自绘输入框要一个可 `Copy` 的「当前是哪个框」标记，
/// 按键才能派发到正确的缓冲区。用枚举而不是两个独立布尔量，是因为「同时聚焦两个框」
/// 在逻辑上不可能，布尔量却允许这种非法状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceField {
    Find,
    With,
}

/// 命令面板里一条可执行命令 (P1-A10)。
///
/// 用枚举而不是闭包：闭包没法 `Clone`，也就没法跟面板状态一起存进 `MainWindow`；
/// 枚举则可以把「要执行什么」与「界面上的条目」解耦，且执行点只有一个 `match`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteCommand {
    OpenFile,
    StartTranscription,
    CancelTranscription,
    ExportCurrentFormat,
    StartTranslation,
    CancelTranslation,
    SwitchToEditor,
    SwitchToGenerate,
    SwitchToLibrary,
    SwitchToPerformance,
    Undo,
    Redo,
    /// 打开字幕「查找替换」面板
    OpenReplace,
    /// 导出质检复核表（CSV）
    ExportQcSheet,
    /// 导出诊断报告
    ExportDiagnostics,
    /// 把当前选中句拆成前后两段（剪辑台）
    SplitSegment,
    /// 把当前选中句与下一句合并（剪辑台）
    MergeWithNext,
    /// 清空当前选中句的译文（剪辑台）
    ClearTranslation,
    /// 按当前勾选应用一键清理规则（剪辑台）
    ApplyCleanup,
    /// 用当前选中的规则集替换全部句（剪辑台）
    ApplyReplaceRuleSet,
    /// 导出当前选中句为视频片段（剪辑台）
    ExportSelectedClip,
    /// 导出当前选中句为音频片段（剪辑台）
    ExportSelectedClipAudio,
    /// 整轨后移 1 秒（剪辑台重定时）
    ShiftTrackForward,
    /// 整轨前移 1 秒（剪辑台重定时）
    ShiftTrackBackward,
    /// 把整轨时间轴铺满媒体总时长（剪辑台重定时）
    FitTrackToMedia,
    /// 按默认间隔消除相邻字幕相贴（剪辑台重定时）
    ApplyMinGap,
}

/// 命令面板可用性判据的输入快照。
///
/// 只装「判据真正要看的那几个状态位」，不塞进整个 `AppState`：判据因此能脱开 GPUI
/// 与数据库单测（见 `mod tests`），新增判据时也一眼看得出它依赖哪些状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteContext {
    /// 当前工作区页签
    pub active_tab: WorkspaceTab,
    /// 正在转写
    pub processing: bool,
    /// 已选定待转写媒体
    pub has_source_file: bool,
    /// 有字幕片段
    pub has_segments: bool,
    /// 正在翻译
    pub translating: bool,
    /// 可以撤销
    pub can_undo: bool,
    /// 可以重做
    pub can_redo: bool,
    /// 选中了某一句
    pub has_selection: bool,
    /// 选中句可拆分（可见文本长度 > 1）
    pub selection_splittable: bool,
    /// 选中句后面还有下一句（可合并）
    pub selection_has_next: bool,
    /// 选中句有译文（可清除）
    pub selection_has_translation: bool,
    /// 已选中一个替换规则集
    pub has_replace_rule_set: bool,
    /// 已打开媒体文件（片段导出需要源文件）
    pub has_media_file: bool,
    /// 片段导出空闲（正在导出时两条片段命令置灰）
    pub clip_export_idle: bool,
}

/// 命令面板里一条命令的注册项：展示信息 + **自带**的可用性判据。
///
/// `zh` / `en` 都参与搜索：中文用户按「导出」找，英文习惯的用户按 `export` 找，
/// 只认一种别名就会有一半人搜不到。`hint` 是右侧的键位提示，没有独立键位的命令留空。
/// `available` 是**纯函数指针**：判据只读 [`PaletteContext`]，所以在无 GUI 的单测里
/// 能直接调（见 `mod tests`）；判据与命令写在同一条注册项里，新增命令时不会漏。
pub struct PaletteActionSpec {
    pub command: PaletteCommand,
    /// 中文名
    pub zh: &'static str,
    /// 英文别名（小写，仅作匹配与副标题展示）
    pub en: &'static str,
    /// 右侧键位提示；空串表示这条命令只有面板入口
    pub hint: &'static str,
    /// 当前是否可用：不可用时面板里置灰（不隐藏）
    pub available: fn(&PaletteContext) -> bool,
}

/// 命令面板的命令注册表：**唯一事实来源**。
///
/// 面板清单（[`filter_commands`]）、可用性（[`PaletteActionSpec::available`]）与执行
/// （`run_palette_command`）都以这张表为准——不再有「清单在一处、可用性在另一处」的
/// 漂移。每条命令都只复用 `MainWindow` 上已有的方法，这里不写业务逻辑。
/// 数组顺序即面板顺序；剪辑台那批命令追加在基础命令之后。
pub const PALETTE_ACTIONS: [PaletteActionSpec; 26] = [
    PaletteActionSpec {
        command: PaletteCommand::OpenFile,
        zh: "打开文件",
        en: "open file",
        hint: "",
        available: |_: &PaletteContext| true,
    },
    PaletteActionSpec {
        command: PaletteCommand::StartTranscription,
        zh: "开始转写",
        en: "start transcription",
        hint: "Ctrl+Enter",
        available: |ctx: &PaletteContext| !ctx.processing && ctx.has_source_file,
    },
    PaletteActionSpec {
        command: PaletteCommand::CancelTranscription,
        zh: "终止转写",
        en: "stop transcription",
        hint: "Esc",
        available: |ctx: &PaletteContext| ctx.processing,
    },
    PaletteActionSpec {
        command: PaletteCommand::ExportCurrentFormat,
        zh: "导出（当前格式）",
        en: "export subtitle",
        hint: "Ctrl+E",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::StartTranslation,
        zh: "开始翻译",
        en: "start translation",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments && !ctx.translating,
    },
    PaletteActionSpec {
        command: PaletteCommand::CancelTranslation,
        zh: "取消翻译",
        en: "cancel translation",
        hint: "",
        available: |ctx: &PaletteContext| ctx.translating,
    },
    PaletteActionSpec {
        command: PaletteCommand::SwitchToEditor,
        zh: "切到剪辑校对",
        en: "editor tab",
        hint: "",
        available: |ctx: &PaletteContext| ctx.active_tab != WorkspaceTab::Editor,
    },
    PaletteActionSpec {
        command: PaletteCommand::SwitchToGenerate,
        zh: "切到语音转写",
        en: "transcribe tab",
        hint: "",
        available: |ctx: &PaletteContext| ctx.active_tab != WorkspaceTab::Generate,
    },
    PaletteActionSpec {
        command: PaletteCommand::SwitchToLibrary,
        zh: "切到视频库",
        en: "library tab",
        hint: "",
        available: |ctx: &PaletteContext| ctx.active_tab != WorkspaceTab::Library,
    },
    PaletteActionSpec {
        command: PaletteCommand::SwitchToPerformance,
        zh: "切到性能设置",
        en: "performance tab",
        hint: "",
        available: |ctx: &PaletteContext| ctx.active_tab != WorkspaceTab::Performance,
    },
    PaletteActionSpec {
        command: PaletteCommand::Undo,
        zh: "撤销",
        en: "undo",
        hint: "Ctrl+Z",
        available: |ctx: &PaletteContext| ctx.can_undo,
    },
    PaletteActionSpec {
        command: PaletteCommand::Redo,
        zh: "重做",
        en: "redo",
        hint: "Ctrl+Y",
        available: |ctx: &PaletteContext| ctx.can_redo,
    },
    PaletteActionSpec {
        command: PaletteCommand::OpenReplace,
        zh: "查找替换",
        en: "find replace",
        hint: "Ctrl+H",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::ExportQcSheet,
        zh: "导出质检表",
        en: "export qc review sheet",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::ExportDiagnostics,
        zh: "导出诊断报告",
        en: "export diagnostics",
        hint: "",
        // 诊断报告不看字幕——环境坏了、连字幕都没有的时候，恰恰最需要它。
        available: |_: &PaletteContext| true,
    },
    PaletteActionSpec {
        command: PaletteCommand::SplitSegment,
        zh: "拆分当前句",
        en: "split segment",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_selection && ctx.selection_splittable,
    },
    PaletteActionSpec {
        command: PaletteCommand::MergeWithNext,
        zh: "合并下句",
        en: "merge with next",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_selection && ctx.selection_has_next,
    },
    PaletteActionSpec {
        command: PaletteCommand::ClearTranslation,
        zh: "清除当前句译文",
        en: "clear translation",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_selection && ctx.selection_has_translation,
    },
    PaletteActionSpec {
        command: PaletteCommand::ApplyCleanup,
        zh: "应用清理规则",
        en: "apply cleanup",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::ApplyReplaceRuleSet,
        zh: "应用替换规则集（全部句）",
        en: "apply replace rules",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments && ctx.has_replace_rule_set,
    },
    PaletteActionSpec {
        command: PaletteCommand::ExportSelectedClip,
        zh: "导出当前句片段",
        en: "export clip",
        hint: "",
        available: |ctx: &PaletteContext| {
            ctx.has_selection && ctx.has_media_file && ctx.clip_export_idle
        },
    },
    PaletteActionSpec {
        command: PaletteCommand::ExportSelectedClipAudio,
        zh: "导出当前句音频",
        en: "export clip audio",
        hint: "",
        available: |ctx: &PaletteContext| {
            ctx.has_selection && ctx.has_media_file && ctx.clip_export_idle
        },
    },
    PaletteActionSpec {
        command: PaletteCommand::ShiftTrackForward,
        zh: "整轨后移 1 秒",
        en: "shift track forward",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::ShiftTrackBackward,
        zh: "整轨前移 1 秒",
        en: "shift track backward",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::FitTrackToMedia,
        zh: "铺满媒体总时长",
        en: "fit track to media",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
    PaletteActionSpec {
        command: PaletteCommand::ApplyMinGap,
        zh: "应用句间最小间隔",
        en: "apply min gap",
        hint: "",
        available: |ctx: &PaletteContext| ctx.has_segments,
    },
];

/// 命令面板打开时的会话状态。
///
/// 输入缓冲 / 光标 / 选中行都只在这里，不进 `AppState`：面板是一次性的浮层，
/// 关掉即丢，不该污染工程状态，也不该进撤销栈。
pub struct CommandPaletteState {
    /// 搜索词（空串显示全部）
    pub(crate) query: String,
    /// 搜索框光标（字符下标，非字节）
    pub(crate) cursor: usize,
    /// 结果列表里的选中行号（不是 `PALETTE_ACTIONS` 的下标）
    pub(crate) selected: usize,
    /// 搜索框的焦点句柄；面板关闭时随之丢弃
    pub(crate) focus: FocusHandle,
    /// 结果列表滚动句柄：↑/↓ 移动选中时把该行滚进视野
    pub(crate) scroll: ScrollHandle,
}

/// 命令面板结果列表的最大高度：超过就滚动，避免十几条命令把面板撑满整屏。
const COMMAND_PALETTE_LIST_H: f32 = 280.0;

/// 按查询词过滤命令面板条目，返回 [`PALETTE_ACTIONS`] 里的下标序列。
///
/// 纯函数（不读 `MainWindow` / `AppState`），所以可以直接单测。规则：
/// - 空查询（含纯空白）返回全部，方便用户先看一眼有什么；
/// - 中文名与英文别名都参与匹配；
/// - 大小写不敏感（`to_lowercase` 而非 `to_ascii_lowercase`：中文无大小写，
///   但别名里可能混着非 ASCII 的大小写字母）；
/// - 无匹配返回空列表，由渲染层显示「没有匹配的命令」。
fn filter_commands(query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return (0..PALETTE_ACTIONS.len()).collect();
    }
    PALETTE_ACTIONS
        .iter()
        .enumerate()
        .filter(|(_, spec)| {
            spec.zh.to_lowercase().contains(needle.as_str())
                || spec.en.to_lowercase().contains(needle.as_str())
        })
        .map(|(index, _)| index)
        .collect()
}

/// 监视器舞台字幕拖拽操作模式
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SubtitleMonitorDragMode {
    /// 自由移动文本框位置
    Move,
    /// 左右边缘手柄：横向拖动改「单行最大字数」（即防换行阈值）
    ResizeHorizontal { is_left: bool },
    /// 四角手柄：横向拖动改字号（文字高度）
    ///
    /// 刻意**不带** `is_top`：竖向拖动不再有任何作用（用户明确要求「去掉上下的拖拽方向，
    /// 只能左右」）。上左与下左、上右与下右的行为完全相同，保留四个角只是为了符合
    /// PPT 式选框的视觉预期。
    ResizeCorner { is_left: bool },
}

/// 监视器舞台字幕拖拽会话
#[derive(Debug, Clone, Copy)]
pub(crate) struct SubtitleMonitorDrag {
    pub mode: SubtitleMonitorDragMode,
    pub start_mouse: (f32, f32),
    pub start_pos_x: f32,
    pub start_pos_y: f32,
    pub start_box_w: f32,
    pub start_font_size: u32,
}

pub struct MainWindow {
    pub(crate) state: AppState,
    pub(crate) metrics_rx: watch::Receiver<crate::app::ResourceMetrics>,
    pub(crate) text_focus: FocusHandle,
    pub(crate) is_text_focused: bool,
    pub(crate) is_extracting_frame: Arc<std::sync::atomic::AtomicBool>,
    /// 最新一次期望抽帧时间戳（单飞队列用）。读取时用 `unwrap_or_else(|e| e.into_inner())`
    /// 容忍 Mutex 中毒：这只是可覆盖的旁路状态，一次中毒不该打断整条预览抽帧链路。
    pub(crate) pending_extract_time: Arc<std::sync::Mutex<Option<f64>>>,
    pub(crate) last_drag_extract: std::time::Instant,
    pub(crate) play_tick_generation: u64,
    pub(crate) completion_dialog: Option<CompletionDialogInfo>,
    pub(crate) benchmark_dialog: Option<BenchmarkDialogInfo>,
    /// 待确认的破坏性操作（清空队列 / 删除记录）。`Some` 时渲染通用确认弹窗。
    pub(crate) confirm_dialog: Option<ConfirmDialogInfo>,
    /// 版本更新详情弹窗是否展开
    pub(crate) update_modal_open: bool,
    /// 一次性中性提示（成功类反馈，如「已删除该记录」）。
    ///
    /// 与 `state.status` 的 `Failed` 分开：那个语义是「出错了」，走红色横幅；
    /// 这里是「操作成功但需要告知」，走中性配色。用户在剪辑台/视频库做完操作后
    /// 需要知道结果，而底部状态栏只在转写页可见。
    pub(crate) notice: Option<String>,
    /// 字幕翻译的取消标志。`trigger_llm_translation` 起任务时把它注入引擎，
    /// 「取消」按钮置位。任务结束后必须复位（引擎在任务开始前也会复位一次，
    /// 双保险），否则下一次翻译一进来就在第一个批次检查点直接退出。
    pub(crate) translate_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// 模型下载的取消标志（与 `translate_cancel` 同一套模式）。
    /// 下载最大 833 MB，慢网下要几十分钟，必须有办法中断。
    pub(crate) model_download_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// 应用更新包下载取消标志
    pub(crate) update_download_cancel: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) editor_export_format: EditorExportFormat,
    /// 导出内容模式：原文 / 仅译文 / 双语。对字幕文件与剪辑工程文件统一生效。
    pub(crate) editor_export_mode: ExportMode,
    /// 术语表编辑的最近状态提示（打开文件 / 应用结果）。`None` 表示没操作过。
    pub(crate) glossary_status: Option<String>,
    pub(crate) is_export_dropdown_open: bool,
    /// 转写就绪卡片中是否展开转写质检与常见错误修复面板（默认折叠，保持就绪卡片极简清爽）
    pub(crate) ready_qc_open: bool,
    /// 样式面板「文字描边」下拉是否展开
    pub(crate) outline_dropdown_open: bool,
    /// 样式面板「背景底框」下拉是否展开
    pub(crate) bg_style_dropdown_open: bool,
    /// 媒体库「排序」下拉是否展开
    pub(crate) library_sort_dropdown_open: bool,
    /// 字幕预览框的**手动**宽度（px）。`Some` 时以它为准并压过「单行最大字数」
    /// 推出的宽度；拖动预览条两侧把手会写入这里并落盘，`None` 则回到按字数自动推算。
    pub(crate) preview_box_w: Option<f32>,
    /// 预览条拖动会话：(基准宽度, 按下时的鼠标 x, 是否左手柄)。`Some` 表示正在拖。
    ///
    /// 记下是哪个手柄，是因为左右两侧「往外拉」的位移方向相反：右手柄往右拉
    /// 位移为正，左手柄往左拉位移为负。宽度变化要按手柄所在侧取符号，否则左手柄
    /// 往外拉会算成负增量，框反而变窄。
    pub(crate) preview_drag: Option<(f32, f32, bool)>,
    /// 监视器舞台字幕拖拽会话
    pub(crate) monitor_subtitle_drag: Option<SubtitleMonitorDrag>,
    /// 是否显示水平居中（X=50%）对称辅助线
    pub(crate) monitor_snap_x: bool,
    /// 是否显示垂直居中（Y=50%）对称辅助线
    pub(crate) monitor_snap_y: bool,
    /// 监视器字幕框是否处于选中激活态
    pub(crate) monitor_subtitle_selected: bool,
    /// 剪辑工作台右侧检查器宽度（自由拖拽调节，上限为窗口宽度的 50%）
    pub(crate) editor_split_w: Option<f32>,
    /// 检查器宽度拖拽会话：(起始宽度, 按下时的鼠标 x)
    pub(crate) editor_split_drag: Option<(f32, f32)>,
    /// 右侧检查器当前显示哪张面板（样式 / 翻译），互斥切换
    pub(crate) subtitle_panel: EditorSubtitlePanel,
    pub(crate) text_cursor_pos: usize,
    /// 内嵌播放器 RenderImage 缓存：(帧版本号, 帧宽, 帧高, 已构建的 GPU 纹理图像)。
    /// 帧版本与尺寸均未变时直接复用，避免每次渲染深拷贝帧数据并重新上传纹理。
    pub(crate) cached_live_image: Option<(u64, u32, u32, Arc<RenderImage>)>,
    /// 监视器**静态定位帧**已经交给 GPUI 的那一张图。
    ///
    /// 只留一张：GPUI 的精灵图集只认 `drop_image` 这一条回收路径，不显式回收
    /// 就等于每帧常驻一张 GPU 纹理（拖动时间轴每 40ms 换一帧）。换帧时用这里的
    /// 旧引用把上一张交还，显存占用恒定在「一帧」。见 `render_monitor_picture`。
    pub(crate) cached_static_image: Option<Arc<RenderImage>>,
    /// 模型在位扫描的「代数」：每请求一次 +1，回写只接受最新一代。
    ///
    /// 扫描跑的是全文件哈希（几十秒级），而删除 / 恢复 / 下载完成都会触发它。没有这个
    /// 计数时，先起的那条后完成会用**旧配置快照**覆盖新结果——界面徽标跳回旧态，
    /// 用户以为自己的设置被撤销了。选「代数」而不是 busy 标志：它顺带解决「扫描期间
    /// 用户又改了配置」这一类，busy 只能挡住重复点击。
    pub(crate) model_presence_scan_gen: std::sync::atomic::AtomicU64,
    /// 脏字幕的延迟落库定时器是否已经起过（进程内只起一次，见
    /// [`MainWindow::ensure_segments_flush_ticker`]）。
    pub(crate) segments_flush_ticker: bool,
    /// 监视器「静态帧解码失败」兜底路径的已试结果：`(帧路径, 解码结果)`。
    ///
    /// `Some((path, None))` 表示这条路径已经试过、解不出来——渲染层据此显示同步
    /// 占位，而不是每帧重试一次失败解码（拖动时间轴每 0.1s 就是一个新路径）。
    /// 解出来的那一张会转交给 `cached_static_image` 走同样的换帧回收，本缓存只
    /// 负责「同一路径不重解」。为什么不把路径直接交给 GPUI 见
    /// `render_monitor_picture` 里那段说明。只保留最新一条即可：路径单调推进。
    pub(crate) cached_fallback_frame: Option<(PathBuf, Option<Arc<RenderImage>>)>,
    /// 字幕清单虚拟列表的滚动位置句柄（uniform_list 仅渲染可视行）
    pub(crate) subtitle_list_scroll: UniformListScrollHandle,
    /// 批量转写队列清单的滚动句柄。
    ///
    /// 该清单嵌在可滚动的页面壳里（`page_shell` 带
    /// `overflow_y_scroll`），而 GPUI 的滚轮监听不 `stop_propagation`：
    /// 没有句柄就无法判断清单是否到边界，也就无法避免「清单与页面一起滚」。
    pub(crate) batch_queue_scroll: ScrollHandle,
    /// 转写页左侧批量任务队列侧栏是否展开（左中右三栏布局）
    pub(crate) queue_sidebar_open: bool,
    /// 上次清单跟随滚动到的选中序号，用于选中变化时自动滚动跟随
    pub(crate) subtitle_list_followed_sel: Option<usize>,
    /// 视频库卡片首帧缩略图缓存：task_id -> 首帧 JPG 路径（磁盘级缓存，跨会话命中）
    /// 空路径表示该任务提取过但失败（视频文件缺失等），用占位框渲染且不再重复派发
    pub(crate) library_thumbs: HashMap<i64, PathBuf>,
    /// 正在后台提取首帧的任务 id 集合，防止重复派发
    pub(crate) library_thumb_inflight: HashSet<i64>,
    /// 视频库卡片显示的文件大小（task_id → 已格式化文本）。
    ///
    /// 缓存的原因：卡片渲染每帧都要这个字符串，而取它要 `fs::metadata`。
    /// 列表没有虚拟化，40 条记录就是每帧 40 次 stat。大小只在载入视频库时
    /// 探测一次，之后只查表。
    pub(crate) library_sizes: HashMap<i64, String>,
    /// 字幕清单搜索关键字（空串表示不过滤）
    pub(crate) subtitle_search: String,
    pub(crate) subtitle_search_focus: FocusHandle,
    pub(crate) root_focus: FocusHandle,
    /// 「查找替换」面板是否展开。
    ///
    /// 与搜索框分开：搜索只是过滤显示（不改数据），替换会**真的改字幕**，两者
    /// 混在一个输入框里很容易误操作——用户以为在筛选，结果把整篇的某个词换掉了。
    pub(crate) replace_panel_open: bool,
    /// 查找词缓冲
    pub(crate) replace_find: String,
    /// 替换词缓冲
    pub(crate) replace_with: String,
    /// 是否区分大小写
    pub(crate) replace_case_sensitive: bool,
    /// 是否同时替换译文（默认关：机翻结果常是用户已校对的成果，不能被顺手改掉）
    pub(crate) replace_include_translation: bool,
    /// 上一次替换的结果提示（`(是否成功, 文案)`），显示在替换面板里。
    pub(crate) replace_status: Option<(bool, String)>,
    pub(crate) replace_find_focus: FocusHandle,
    pub(crate) replace_with_focus: FocusHandle,
    /// 当前聚焦的替换输入框；`None` 表示两个都没聚焦
    pub(crate) focused_replace_field: Option<ReplaceField>,
    /// 替换输入框的光标位置（同一时刻只有一个框聚焦，共用一份）
    pub(crate) replace_cursor: usize,
    /// 可保存 / 复用 / 导入导出的替换规则集（磁盘上是与 `config.toml` 同目录的
    /// `replace_rules.json`，见 [`ReplaceRuleSet`]）。
    pub(crate) replace_rule_sets: Vec<ReplaceRuleSet>,
    /// 替换面板里当前选中的规则集（[`MainWindow::replace_rule_sets`] 的下标）。
    pub(crate) replace_rule_selected: Option<usize>,
    /// 规则集清单是否展开（面板里的轻量下拉，不引入新控件族）
    pub(crate) replace_rules_open: bool,
    /// 规则集最近一次操作提示（`(是否成功, 文案)`），形状与 `replace_status` 一致
    pub(crate) replace_rules_status: Option<(bool, String)>,
    pub(crate) export_template_focus: FocusHandle,
    pub(crate) export_template_focused: bool,
    pub(crate) export_template_cursor: usize,
    /// 整轨时间轴调整：当前方式（0=未选 / 1=平移 / 2=缩放 / 3=铺满）
    pub(crate) retime_op: u8,
    /// 平移量（毫秒，可负）
    pub(crate) retime_shift_ms: i64,
    /// 缩放比例（百分比，100 = 不变）
    pub(crate) retime_scale_pct: u32,
    /// 铺满目标总时长（秒）
    pub(crate) retime_target_secs: u32,
    pub(crate) subtitle_search_focused: bool,
    pub(crate) subtitle_search_cursor: usize,
    /// 当前搜索命中的字幕在 `state.segments` 中的下标序列。
    /// 虚拟列表按此序列渲染，因此过滤后行高与滚动条仍然正确。
    pub(crate) subtitle_filter: Vec<usize>,
    /// `subtitle_filter` 的缓存有效性依据：`(搜索关键字, segments_revision)`。
    /// 两者都没变时直接复用上次算好的下标序列，避免每帧重跑子串匹配。
    pub(crate) subtitle_filter_key: Option<(String, u64)>,
    /// 字幕搜索的加速索引与增量收窄缓存（见 [`SubtitleSearchCache`]）。
    ///
    /// 与 `subtitle_filter_key` 分工：键不变时的逐帧复用靠上面那个键；这份索引专治
    /// **逐字输入**——旧实现对每一句、每一个按键都重跑一次 `to_lowercase()`，
    /// 几万句规模下每敲一个字就是一次成规模的堆分配，输入明显发顿。
    pub(crate) subtitle_search_cache: SubtitleSearchCache,
    /// 低置信句下标的缓存：键为 `(阈值, segments_revision)`。
    ///
    /// 与 `glossary_bad_cache` 同一套理由：剪辑台的字幕清单每帧渲染可视行，
    /// 逐帧重扫「哪些句 confidence 低于阈值」纯属浪费；阈值或字幕变了才重算一次。
    /// 阈值作为缓存键的一部分是必须的——用户在 `config.toml` 调了
    /// `whisper_low_confidence` 后，高亮必须立刻跟着变。
    pub(crate) low_confidence_cache: Option<((u64, u64), Vec<usize>)>,
    /// 术语表疑似未命中句下标的缓存：键为 `(术语表文本, segments_revision)`。
    /// 与 `subtitle_filter` 同理——播放时每 40ms 重绘一次，逐帧对上千句 × 术语条数
    /// 跑子串匹配会白白掉帧。
    ///
    /// 值存 `Arc<[usize]>`：命中路径只克隆引用计数。原来的 `Vec<usize>` 每次命中
    /// 都要深拷贝一遍，等于把省下的遍历又按元素逐个拷了回来。
    pub(crate) glossary_bad_cache: GlossaryBadCache,
    /// 剪辑台渲染用的「派生统计快照」缓存：键为 [`DerivedSnapshotKey`]。
    ///
    /// 与上面三个缓存同一范式（`segments_revision` + 会改动结果的配置项），
    /// 只是把原先散在渲染路径上、各自 O(n) 的量收成一份，见 [`DerivedSnapshot`]。
    pub(crate) derived_snapshot_cache: Option<(DerivedSnapshotKey, Arc<DerivedSnapshot>)>,
    /// 播放命中句的缓存（上次命中的片段表下标 + 失效依据），
    /// 见 [`MainWindow::cached_active_segment_index`]。
    pub(crate) active_segment_cache: Option<ActiveSegmentCache>,
    /// 就地编辑状态：`Some((段落index, 是否为译文))`
    pub(crate) inline_edit_target: Option<(usize, bool)>,
    /// 就地编辑缓冲区文本
    pub(crate) inline_edit_buffer: String,
    /// 就地编辑光标位置（字符偏移）
    pub(crate) inline_edit_cursor: usize,
    /// 就地编辑焦点句柄
    pub(crate) inline_edit_focus: FocusHandle,
    /// 自定义调色板展开状态
    pub(crate) custom_color_picker_open: bool,
    /// 设置界面的当前子选项卡 (0: 硬件与推理, 1: 大模型与 AI 润色, 2: 组件与模型下载)
    pub(crate) settings_tab: usize,
    /// 全局字幕样式卡片是否收起折叠（默认展开，点击可折叠以给单句精修腾出空间）
    pub(crate) subtitle_style_collapsed: bool,
    /// 窗口尺寸与位置变化监听订阅句柄
    pub(crate) bounds_subscription: Option<Subscription>,
    /// 缓存的色相圆环图像
    #[allow(dead_code)]
    pub(crate) cached_color_wheel: Option<Arc<RenderImage>>,
    /// 调色板当前亮度乘数 (0.2 ~ 1.0)
    pub(crate) color_picker_brightness: f32,
    /// 上次点击字幕清单项的时间记录 (seg_idx, is_trans, click_instant)，用于双击判定
    pub(crate) last_subtitle_click: Option<(usize, bool, std::time::Instant)>,
    /// 在线翻译 API 配置卡片的编辑缓冲（改完即写 config.toml）
    pub(crate) api_base_input: String,
    pub(crate) api_model_input: String,
    pub(crate) api_key_input: String,
    /// 翻译语言下拉框是否展开（剪辑台翻译卡里，目标语言从一排胶囊改成下拉）。
    pub(crate) translate_lang_open: bool,
    pub(crate) api_base_focus: FocusHandle,
    pub(crate) api_model_focus: FocusHandle,
    pub(crate) api_key_focus: FocusHandle,
    /// 本地翻译模型路径的编辑缓冲（`config.paths.llm_model` 的界面镜像）
    pub(crate) local_model_input: String,
    pub(crate) local_model_focus: FocusHandle,
    /// 本地模型路径的即时校验提示：`None` = 无提示 / 路径有效
    pub(crate) local_model_hint: Option<(bool, String)>,
    /// 当前聚焦的配置输入框；`None` 表示所有输入框都未聚焦
    pub(crate) focused_api_field: Option<ApiField>,
    /// 单行输入共用的光标位置（同一时刻只有一个框聚焦，无需每框一个）
    pub(crate) line_edit_cursor: usize,
    /// 样式排版参数当前聚焦编辑的字段
    pub(crate) focused_style_field: Option<StyleField>,
    /// 样式排版参数正在编辑输入的文本缓冲
    pub(crate) style_field_input: String,
    /// 样式排版参数输入框焦点句柄
    pub(crate) style_field_focus: FocusHandle,
    /// API Key 是否明文显示（默认掩码，避免录屏/截图泄露）
    pub(crate) api_key_visible: bool,
    /// 在线翻译接口连通性自检状态
    pub(crate) is_probing_translate: bool,
    /// `(是否成功, 提示文案)`；`None` 表示尚未测试过
    pub(crate) translate_probe_msg: Option<(bool, String)>,
    /// 视频库的筛选条件（关键字 + 未翻译/已翻译）。
    ///
    /// 放在界面层而不是 `AppState`：它纯粹是「当前这一页怎么显示」，与工程数据无关，
    /// 也不该被持久化到 `config.toml`（用户下次打开想看到全部）。
    pub(crate) library_filter: crate::utils::library_query::LibraryFilter,
    /// 视频库排序方式
    pub(crate) library_sort: crate::utils::library_query::LibrarySort,
    /// 筛选关键字输入框的焦点与光标（与字幕搜索同法）
    pub(crate) library_search_focus: FocusHandle,
    pub(crate) library_search_focused: bool,
    pub(crate) library_search_cursor: usize,
    /// `query()` 的结果缓存：键为 `(筛选, 排序, 记录数, 最近一条 id)`。
    ///
    /// 为什么不每帧重算：卡片列表没有虚拟化，40 条 × 多次大小写转换的字符串匹配
    /// 看着不多，但播放/编辑期间每帧都在重绘。记录数 + 首条 id 足以侦测「库变了」。
    pub(crate) library_query_cache: Option<(LibraryQueryKey, Vec<i64>)>,
    /// 上次运行崩溃的提示（启动时检测一次；`None` 表示没有）。
    ///
    /// 与 `notice` 分开：`notice` 是「刚刚做了一件事」的一次性反馈，会被后续操作
    /// 覆盖；崩溃提示要一直挂到用户点掉它，否则启动后随便点个按钮就看不见了。
    pub(crate) crash_notice: Option<String>,
    /// 上次崩溃的完整记录（含堆栈）。
    ///
    /// 为什么除了展示用的 `crash_notice` 还要留原始记录：横幅上的「复制报告」要把
    /// **堆栈**一起给出去（`crash_report::build_report`），而那需要原始字段；
    /// 只留格式化后的摘要就得在渲染期反解析自己的输出。
    pub(crate) crash_record: Option<crate::utils::crash_report::CrashRecord>,
    /// 视频库批量导出的选中任务 id 集合。
    ///
    /// 历史库里一条条点「导出字幕」很费手，尤其是「同一部片子重跑了几版、
    /// 想一次性全导出来比对」的场景。选中集只存 id（不存下标），列表刷新后
    /// 顺序变化也不会错位；真正导出时再按 id 现取字幕，避免常驻内存。
    pub(crate) library_selected: HashSet<i64>,
    /// 批量导出进行中：按钮置灰并显示进度，防止重复触发并发写盘。
    pub(crate) library_export_busy: bool,
    /// 命令面板 (P1-A10) 是否打开。`Some` 时渲染居中模态面板。
    /// 面板是「非阻塞式冒泡」的轻量浮层而非确认框那种阻塞模态，因此与
    /// `confirm_dialog` 等并存时由渲染顺序决定盖在谁上面（见 `Render::render`）。
    pub(crate) command_palette: Option<CommandPaletteState>,
    /// 快捷键速查面板 (F1) 是否打开。与命令面板同级的轻量浮层：只影响渲染，
    /// 不碰任何工程状态，关掉即回到原样。
    pub(crate) shortcuts_help_open: bool,
    /// 「模型与组件」页三个分块的展开状态，下标 = [`ModelManagerTab`] 的判别值。
    ///
    /// 早先这里是「当前选中哪个专区」的单值：三个专区只能看一个，想对比
    /// Whisper 模型与公用组件得来回点。现在三块同页铺开、各自独立收起，
    /// 默认全展开——「一页看全」正是改成这样的理由。
    pub(crate) model_blocks_open: [bool; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ModelManagerTab {
    #[default]
    SenseVoice,
    Whisper,
    Common,
}

/// 剪辑台行首「低置信」标记的判据（纯函数，便于单测）。
///
/// # 为什么只认「有值且低于阈值」
///
/// `confidence == None` 的语义是「这条链路根本不产生逐句置信度」（SenseVoice 全程
/// 如此、Whisper 也只在质检开启时才有 token 概率），**不是**「识别得不好」。
/// 若把 `None` 也算低置信，SenseVoice 用户打开剪辑台会看到密密麻麻一整片红点，
/// 这个提示立刻变成噪声，用户很快就学会无视它——那才是真正丢掉了这个功能。
///
/// 判据与转写页质检卡的「低置信」一栏严格同源（同一条 `c < threshold`、
/// 同一个阈值来源 `pipeline.whisper_low_confidence`），保证同一份数据在两个页面
/// 给出同一个数字，不会「这页 3 句、那页 5 句」。
pub(crate) fn low_confidence_indices(
    segments: &[crate::subtitle::Segment],
    threshold: f64,
) -> Vec<usize> {
    segments
        .iter()
        .filter(|s| s.confidence.is_some_and(|c| c < threshold))
        .map(|s| s.index)
        .collect()
}

/// 是否存在「译文与原文不同」的句子——字幕清单双语布局与翻译卡收尾文案的同一判据。
///
/// 口径与改造前散在渲染路径上的三处内联实现**逐字一致**：译文去掉首尾空白后非空，
/// 且不等于 `display_text().trim()`。两侧都必须 `trim()`：机翻常把原文原样回填、
/// 只在首尾多几个空格或换行；不 trim 就会把它当成独立译文，清单每行白白高一截，
/// 用户还会看到上下两行一模一样的字。
///
/// 抽成纯函数是为了让派生快照与渲染路径共用同一份判定，且能在单测里钉死 trim 口径
/// （见 `mod tests`）。
pub(crate) fn has_distinct_translation(segments: &[Segment]) -> bool {
    segments.iter().any(|s| {
        if let Some(t) = &s.translation {
            !t.trim().is_empty() && t.trim() != s.display_text().trim()
        } else {
            false
        }
    })
}

/// 是否存在非空译文（不论目标语言）。与 `AppState::translated_count() > 0` 同义
/// （计数 > 0 等价于至少一句 `has_translation`）；质检报告的「未翻译」开关取它。
pub(crate) fn has_any_translation(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.has_translation())
}

/// 已带译文、但**不是**当前目标语言的句数（切换目标语言后的提示文案用）。
pub(crate) fn translated_other_lang_count(segments: &[Segment], target_lang: &str) -> usize {
    segments
        .iter()
        .filter(|s| s.has_translation() && !s.translation_matches(target_lang))
        .count()
}

/// 统计用的媒体总时长（秒）：优先工程记录值（`total_duration`），没有则退回转写
/// 探测值（`transcribe_duration`）；两者都为 0 时 `spoken_ratio()` 返回 0，界面照常
/// 显示而不出 NaN。
///
/// 抽成单一入口是为了让**缓存键**与**统计口径**用同一个输入：两处各写一遍
/// 「谁优先」，迟早出现「缓存按 A 判失效、统计卡按 B 算数字」的错位。
fn stats_media_secs(state: &AppState) -> f64 {
    if state.total_duration > 0.0 {
        state.total_duration
    } else {
        state.transcribe_duration
    }
}
/// 秒 → `(分, 秒)` 的整数拆分（状态栏 / 转写页进度显示的**唯一**换算口径）。
///
/// 抽出来是因为同一条换算原本在 `components/status_bar.rs` 与 `views/transcribe.rs`
/// 里各写了两遍（「另一处改了、这一处忘了」的漂移高发区）。
///
/// 口径与改造前的显示逐字一致：
/// - **截断**而非四舍五入——进度显示不提前跳秒；
/// - 负数 / `NaN` 夹到 0（时长未就绪、时钟回拨都不该 panic，也不该显示负值）；
/// - 超过一小时按**分钟累计**（`3661.0 → (61, 1)`）：两处界面都只有 `mm:ss`，
///   没有 `HH` 位，折成小时反而会改观感。
pub(crate) fn split_mm_ss(seconds: f64) -> (u32, u32) {
    // `as u32` 是饱和转换（`NaN` / 负值都落到 0），这里显式兜住非有限值与非正值，
    // 再夹到 u32 量程：任何输入都不会 panic 或回绕。
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds.min(u32::MAX as f64) as u32
    } else {
        0
    };
    (total / 60, total % 60)
}

/// 秒 → `"mm:ss"` 文本（[`split_mm_ss`] 的薄封装，调用点可直接落进 `format!`）。
pub(crate) fn format_mm_ss(seconds: f64) -> String {
    let (m, s) = split_mm_ss(seconds);
    format!("{m:02}:{s:02}")
}

/// 「可打印单字符」按键的插入核（[`apply_line_edit`] 的默认分支）。
///
/// 抽成纯函数是为了单测：`apply_line_edit` 需要 `&mut App`（lib 单测里构造不出
/// 平台上下文），而这条分支不碰 `App`。
///
/// 判据一次迭代完成：**恰好一个字符**才算可打印输入——多字符键名（`space` /
/// `f5` / `arrowup`）走不到插入路径，与旧 `chars().count() == 1` 判据等价，
/// 但不再有「先数后取」两步之间那个 `unwrap()`。控制字符同样不插入。
fn insert_printable_key(buffer: &mut String, cursor: &mut usize, key_raw: &str) -> bool {
    let mut chars = key_raw.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return false;
    };
    if ch.is_control() {
        return false;
    }
    let mut buf: Vec<char> = buffer.chars().collect();
    // 光标越界（缓冲被别处整体替换过）时夹到尾部：`Vec::insert` 不接受 `at > len`
    let at = (*cursor).min(buf.len());
    buf.insert(at, ch);
    *buffer = buf.into_iter().collect();
    *cursor = at + 1;
    true
}

/// 把一次按键应用到单行文本缓冲区的光标处（自绘输入框的共用编辑核心）。
///
/// 自绘输入框拿不到系统 IME 组合态，因此这里实现的是「可打印字符直插 + 退格/删除/
/// 左右/Home/End + Ctrl+A/C/V」这套最小可用集合；中文请借助粘贴或「弹窗编辑」。
/// 返回 `true` 表示该按键已被消费（含纯光标移动），调用方据此决定是否 `cx.notify()`。
pub(crate) fn apply_line_edit(
    buffer: &mut String,
    cursor: &mut usize,
    event: &KeyDownEvent,
    cx: &mut App,
) -> bool {
    let key_raw = event.keystroke.key.as_str();
    let key_lower = key_raw.to_ascii_lowercase();
    let key = key_lower.as_str();
    let total = buffer.chars().count();
    *cursor = (*cursor).min(total);

    if key == "escape" {
        buffer.clear();
        *cursor = 0;
        return true;
    }

    if event.keystroke.modifiers.control {
        match key {
            "a" => {
                *cursor = total;
                return true;
            }
            "u" | "k" => {
                buffer.clear();
                *cursor = 0;
                return true;
            }
            "backspace" | "back" | "\x08" | "delete" | "del" | "\x7f" => {
                buffer.clear();
                *cursor = 0;
                return true;
            }
            "c" => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(buffer.clone()));
                return true;
            }
            "v" => {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        let flat = text.trim().replace(['\r', '\n'], "");
                        let mut chars: Vec<char> = buffer.chars().collect();
                        let insert: Vec<char> = flat.chars().collect();
                        let inserted = insert.len();
                        chars.splice(*cursor..*cursor, insert);
                        *buffer = chars.into_iter().collect();
                        *cursor += inserted;
                    }
                }
                return true;
            }
            _ => return false,
        }
    }

    match key {
        "backspace" | "back" | "\x08" => {
            if *cursor > 0 && total > 0 {
                let mut chars: Vec<char> = buffer.chars().collect();
                chars.remove(*cursor - 1);
                *buffer = chars.into_iter().collect();
                *cursor -= 1;
                return true;
            }
            true
        }
        "delete" | "del" | "\x7f" => {
            if *cursor < total {
                let mut chars: Vec<char> = buffer.chars().collect();
                chars.remove(*cursor);
                *buffer = chars.into_iter().collect();
                return true;
            }
            true
        }
        "left" | "arrowleft" => {
            if *cursor > 0 {
                *cursor -= 1;
            }
            true
        }
        "right" | "arrowright" => {
            if *cursor < total {
                *cursor += 1;
            }
            true
        }
        "home" => {
            *cursor = 0;
            true
        }
        "end" => {
            *cursor = total;
            true
        }
        // 可打印单字符插入：判据与插入都在 `insert_printable_key`（纯函数、有单测），
        // 不再「先 `chars().count() == 1` 再 `chars().next().unwrap()`」两步走。
        _ => insert_printable_key(buffer, cursor, key_raw),
    }
}

/// 在主窗口上注册「系统级关窗」脏数据守卫。
///
/// Alt+F4、任务栏右键「关闭窗口」、点系统标题栏关闭按钮在 gpui 里都收敛到
/// `WM_CLOSE`（`platform/windows/events.rs` 的 `handle_close_msg`），而
/// `Window::on_window_should_close(&self, cx: &App, f: impl Fn(&mut Window, &mut App) -> bool)`
/// 是它的安全封装：回调返回 `false` 时平台层直接 `Some(0)` 短路，不再调用
/// `DefWindowProcW`，窗口保持打开（返回 `true` 才真正关闭）。
///
/// 回调接到与 `components/titlebar.rs` 自绘关闭按钮**完全相同**的判定上，
/// 避免两套关窗逻辑漂移。
pub fn install_window_close_guard(window: &Window, root: &Entity<MainWindow>, cx: &mut App) {
    let root = root.downgrade();
    window.on_window_should_close(cx, move |_window, cx| {
        root.update(cx, |view, cx| view.flush_segments_before_close(cx))
            .unwrap_or(true)
    });
}

/// 剪辑台渲染所需的「派生统计快照」：一次遍历算完所有逐帧只读的派生量。
///
/// # 为什么要有它
///
/// 剪辑台渲染路径上有一批「按字幕条数 O(n)、每帧都要算、结果只随数据或配置变」的
/// 量：双语判定（原先在三个地方各算一遍）、整篇统计（每句还要逐字符扫全文）、
/// 翻译进度、质检报告。三千到三万句时它们加起来是每帧 10~20ms，而播放时每 40ms
/// 就要重绘一次全窗——不缓存就是一路掉帧。这里把所有量收进一份快照，按
/// [`DerivedSnapshotKey`] 失效，渲染路径只读。
///
/// # 语义来源
///
/// 每个字段都由对应模块的既有纯函数现算（见 [`build_derived_snapshot`]），界面层
/// 不再内联任何判据；单测把快照与这些函数**逐字段**比对，语义不会因为「换了份
/// 实现」而漂移（见 `mod tests` 的 `derived_snapshot_matches_direct_calls`）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedSnapshot {
    /// 是否存在「译文与原文不同」的句子，见 [`has_distinct_translation`]。
    ///
    /// 字幕清单「上下并列显示 / 双语显示」的标题与行高、翻译卡收尾文案都取它：
    /// 只有真有独立译文时才把清单切成双语行高，否则机翻回填的「译文 == 原文」
    /// 会让每行白白高一截。
    pub(crate) has_distinct_translation: bool,
    /// 是否存在非空译文（任意语言），见 [`has_any_translation`]。
    /// 质检的「未翻译」开关与清单子标头的「单语显示 (译文同原文)」取它。
    pub(crate) has_any_translation: bool,
    /// 是否存在「译文与原文一字不差」的句子，见
    /// [`crate::ui::actions::has_copied_translations`]。
    ///
    /// 翻译卡据此决定要不要给出「只清复制译文」入口。这条判据要按翻译管线的复制
    /// 口径逐句算（大小写 / 空白归一后再比），面板每帧现算就是每帧一次 O(n)，
    /// 因此与其它派生量放在一起、共用同一把失效键。
    pub(crate) has_copied_translation: bool,
    /// 当前目标语言下实际译出的句数（`actions::translated_out_count`）。
    pub(crate) translated_out: usize,
    /// 应有译文的句数（源文非空的句数，`actions::expected_translation_count`）。
    pub(crate) expected_translation: usize,
    /// 已带译文但**不是**当前目标语言的句数（切换目标语言后的提示用）。
    pub(crate) other_lang_translated: usize,
    /// 整篇统计（`subtitle::stats::compute`）。
    pub(crate) stats: SubtitleStats,
    /// 转写质检报告（`subtitle::segment::quality_report`，术语违规已填）。
    /// 存 `Arc` 是为了「按帧取用」不必深拷贝它内部的四个 `Vec`。
    pub(crate) quality_report: Arc<QualityReport>,
    /// 时间轴「按窗口取字幕」用的排序索引，见 [`TimelineIndex`]。
    ///
    /// 与上面几项共用同一个失效键（`segments_revision` + 配置）：时间轴每帧都要问
    /// 一次「视口 ±15 秒内有哪些片段」，全量扫几千~几万句就是每帧一次 O(n)；
    /// 索引只在片段表或配置变了时重建一次，逐帧只付两次二分。
    pub(crate) timeline: TimelineIndex,
}

/// 时间轴「按窗口取字幕」用的排序索引（随 [`DerivedSnapshot`] 一起缓存）。
///
/// # 为什么要有它
///
/// 时间轴每帧都要问一次「视口 ±15 秒内有哪些片段」（见
/// [`timeline_visible_positions`]）。原实现是遍历整张片段表再过滤，几千~几万句时
/// 就是每帧一次 O(n)，播放时 40ms 一帧纯属白烧。索引只在 `segments_revision`
/// 或配置变化时重建一次（与派生快照同键，不另开一份缓存），逐帧只付
/// 「两次二分 + 扫窗口内那几条」。
///
/// # 为什么是三个数组
///
/// - `by_start`：`start` 升序的片段表下标。相交判据里的 `start <= 窗口右界` 在它
///   上面是一个前缀，二分即可定位；排序**稳定**，`start` 相同的段保持存储顺序。
/// - `prefix_max_end`：`by_start` 前缀内 `end` 的最大值，单调不减。`end >= 窗口左界`
///   在 `by_start` 顺序里**并不**单调（长句可以跨越整个窗口），但「前缀里已经出现过
///   `end >= 窗口左界`」是单调的，因此在它上面二分能安全跳过左侧无关段；重叠 / 乱序
///   的片段表也不会漏句（等价性由单测逐点钉住）。
/// - `by_index`：`Segment::index` 升序的片段表下标。时间轴的「选中句必须始终可见」
///   这一支与时间无关，窗口扫描覆盖不到它；没有这张索引就得每帧全表找一遍选中句。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineIndex {
    /// 按 `start` 升序（稳定）排列的片段表下标。
    pub(crate) by_start: Vec<usize>,
    /// `by_start[..=i]` 里 `end` 的最大值（单调不减）。
    pub(crate) prefix_max_end: Vec<f64>,
    /// 按 `Segment::index` 升序（稳定）排列的片段表下标。
    pub(crate) by_index: Vec<usize>,
}

/// 建 [`TimelineIndex`]：一次 O(n log n)，之后每帧只 O(log n + 窗口内条数)。
///
/// 抽成自由函数（不吃 `AppState`）是为了能在无 GUI 的单测里钉住三条性质：
/// `by_start` 按 `start` 稳定升序、`prefix_max_end` 单调不减、`by_index` 按
/// `Segment::index` 升序且是 `0..n` 的一个排列——这三条是窗口查询能安全二分的全部前提。
pub(crate) fn build_timeline_index(segments: &[Segment]) -> TimelineIndex {
    let mut by_start: Vec<usize> = (0..segments.len()).collect();
    // 稳定排序：`start` 相同的段保持存储顺序，扫出来的下标集合因此与「按存储顺序
    // 全量过滤」的旧实现天然对齐（顺序由调用方再排回来，见查询函数末尾）。
    by_start.sort_by(|&a, &b| {
        segments[a]
            .start
            .partial_cmp(&segments[b].start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // 前缀最大 end：初值取负无穷，`f64::max` 会把 NaN 当「忽略」，因此这条序列
    // 永远是单调不减的实数序列，可以放心二分（NaN 的段本来就判不中，见查询函数）。
    let mut prefix_max_end: Vec<f64> = Vec::with_capacity(segments.len());
    let mut running = f64::NEG_INFINITY;
    for &pos in &by_start {
        running = running.max(segments[pos].end);
        prefix_max_end.push(running);
    }
    let mut by_index: Vec<usize> = (0..segments.len()).collect();
    by_index.sort_by_key(|&pos| segments[pos].index);
    TimelineIndex {
        by_start,
        prefix_max_end,
        by_index,
    }
}

/// 时间轴窗口查询：返回「与 `[current_time - margin, current_time + margin]` 相交的，
/// 外加选中句」这些片段在**存储顺序**下的下标。
///
/// # 与原实现逐条等价
///
/// 渲染路径原来的判据是
/// `selected == Some(seg.index) || cur ∈ [seg.start, seg.end] || cur ∈ [seg.start - margin, seg.end + margin]`
/// （`render_multitrack_timeline`）。中间那条被 ±margin 那条包含（`margin >= 0`），
/// 所以等价于 `start <= cur + margin && end >= cur - margin`，再或上「选中句」。
/// 本函数给的就是同一个集合，且按存储顺序返回——旧实现正是把过滤结果按存储顺序画出来的，
/// 因此可见集合与绘制顺序都不变。
///
/// # 怎么做到不全量扫
///
/// `by_start` 上 `start <= cur + margin` 是一个前缀，二分定位右界；前缀里
/// `prefix_max_end < cur - margin` 的部分整体不可能命中，二分跳过左界（注意**不能**
/// 在 `by_start` 顺序上直接二分 `end`：重叠句会让 `end` 不再单调，那会漏句）。
/// 两者之间线性扫一遍：正常（按时间有序的）字幕表里这段就是窗口内那几条，
/// 而不是每帧几千~几万句。
pub(crate) fn timeline_visible_positions(
    segments: &[Segment],
    timeline: &TimelineIndex,
    current_time: f64,
    selected: Option<usize>,
    margin: f64,
) -> Vec<usize> {
    let left = current_time - margin;
    let right = current_time + margin;
    // 右界：`start > right` 的段整体落在窗口右侧，不可能相交。
    let prefix_end = timeline
        .by_start
        .partition_point(|&pos| segments[pos].start <= right);
    // 左界：越过它的段满足「前缀里出现过 end >= left」，之前的段必然 end < left。
    // 钳到 `prefix_end` 之内是防御性写法：万一有 end < start 的倒置段，
    // 直接切片会 panic（那种段本来也判不中，钳一下只是不让它越界）。
    let scan_from = timeline
        .prefix_max_end
        .partition_point(|&max_end| max_end < left)
        .min(prefix_end);
    let mut visible: Vec<usize> = Vec::new();
    for &pos in &timeline.by_start[scan_from..prefix_end] {
        if segments[pos].end >= left {
            visible.push(pos);
        }
    }
    if let Some(id) = selected {
        // 「选中句恒可见」这一支与时间无关：按 index 二分取等值区间。同一个 index
        // 重复出现时与旧实现同口径（全部可见），这里刻意不去重。
        let from = timeline
            .by_index
            .partition_point(|&pos| segments[pos].index < id);
        let to = timeline
            .by_index
            .partition_point(|&pos| segments[pos].index <= id);
        visible.extend_from_slice(&timeline.by_index[from..to]);
    }
    // 绘制顺序必须是存储顺序（旧实现就是按存储顺序遍历的）：排的是下标而不是片段，
    // 顺带把「选中句同时落在窗口内」造成的重复项去掉——集合与顺序都不变。
    visible.sort_unstable();
    visible.dedup();
    visible
}

/// 术语表原文的内容指纹：`(UTF-8 字节长度, FNV-1a 64 位哈希)`。
///
/// # 为什么键里存指纹而不是原文
///
/// 键里放 `String` 的话，每个构造键的调用点都得先 `glossary.clone()`——`derived_snapshot()`
/// 每帧被多处消费，等于每帧白拷好几份可能上 KB 的术语表原文，只为做一次内容比较。
/// 指纹是定长 `Copy` 值，进键零堆分配。
///
/// # 为什么与「按字符串相等」等价
///
/// 内容不变 ⇒ 长度与哈希都不变（纯函数），必命中；内容变了（哪怕字节长度不变）
/// 哈希几乎必然变，必重算。算法与 `src/utils/frame_cache.rs` 的 `stable_hash`
/// **逐字等价**（那边是私有 fn，本次改动范围不允许改它，因此这里放一份等价实现，
/// 双方注释互相指认；将来若要统一，应把它提为 `pub(crate)` 后删掉这份）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlossaryFingerprint {
    /// 原文的 UTF-8 字节长度。
    pub(crate) len: usize,
    /// FNV-1a 64 位哈希值。
    pub(crate) hash: u64,
}

/// 计算 [`GlossaryFingerprint`]：FNV-1a 64 位，纯函数、跨进程稳定。
pub(crate) fn glossary_fingerprint(text: &str) -> GlossaryFingerprint {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in text.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    GlossaryFingerprint {
        len: text.len(),
        hash,
    }
}

/// [`DerivedSnapshot`] 的失效键：**所有**会改变快照内容的输入都在这里。
///
/// 与三个既有缓存（`subtitle_filter` / `glossary_bad_cache` / `low_confidence_cache`）
/// 同一范式：逐句内容只认 `segments_revision`（任何改 `segments` 的入口都必须递增
/// 它，见 `AppState::bump_segments_revision`），其余是「不随 revision 变化的配置 /
/// 探测输入」。逐帧只做一次结构体比较，比重新遍历字幕表便宜几个数量级。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DerivedSnapshotKey {
    /// 字幕内容的修订号（内容或长度变化的唯一失效依据）。
    pub(crate) segments_revision: u64,
    /// 翻译目标语言：`translated_out` / `other_lang_translated` 的判据。
    pub(crate) target_lang: String,
    /// 低置信阈值（`pipeline.whisper_low_confidence`）的位模式：`quality_report`
    /// 的 `c < 阈值`。用 `to_bits` 与 `low_confidence_cache` 同法（浮点直接进键会
    /// 踩 `-0.0` / `NaN` 的比较语义）。
    pub(crate) low_confidence_bits: u64,
    /// 术语表原文的**内容指纹**：`quality_report.glossary_violations` 的判据。
    /// 按原文而不是解析后的条目比对：多算一次无所谓，漏算会出现「改了术语表、
    /// 违规高亮不动」。存指纹而非原文见 [`GlossaryFingerprint`]：取键零 clone。
    pub(crate) glossary_fingerprint: GlossaryFingerprint,
    /// 样式里的单行字数上限：`stats` 的「单行长句」门槛。
    pub(crate) max_chars_per_line: u32,
    /// 统计用的媒体总时长（秒，取 [`stats_media_secs`]）的位模式。
    ///
    /// 这条**不能**省：它来自时长探测 / 工程记录，探测完成时 `segments` 一行没动
    /// （revision 不变），但「有语音时长占比」等数字必须立刻跟着变。
    pub(crate) media_secs_bits: u64,
}

/// 播放命中句的缓存：上次命中的**片段表下标**与失效依据。
///
/// 为什么只缓存一条、不做二分，见
/// [`MainWindow::cached_active_segment_index`]。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveSegmentCache {
    /// 建缓存时的 `segments_revision`：内容一变即整体失效。
    revision: u64,
    /// 上一帧命中的片段在 `segments` 里的下标。
    index: usize,
}

/// 一次算完剪辑台渲染需要的全部派生量。
///
/// **口径全部照抄原调用点**，这里只做「把既有函数收拢到一处」的重排：统计走
/// `stats::compute`、质检走 `quality_report`（术语违规由同一份条目现算，与
/// `cached_glossary_violations` 同源）、翻译进度走 `actions` 的两个计数函数、
/// 双语判定走 [`has_distinct_translation`]。
///
/// 「未翻译」开关取「整篇有任意非空译文」（与 `AppState::translated_count() > 0`
/// 同义）：整篇没译文是「还没开始翻译」，不是漏译——与转写页质检卡的原判据一致。
///
/// 抽成自由函数（不吃 `AppState`）是为了能在无 GUI 上下文的单测里逐字段对照结果：
/// `MainWindow` 在 lib 单测里构造不出来。
pub(crate) fn build_derived_snapshot(
    segments: &[Segment],
    media_secs: f64,
    max_chars_per_line: usize,
    target_lang: &str,
    low_confidence_threshold: f64,
    glossary_entries: &[(String, String)],
) -> DerivedSnapshot {
    // 质检的「未翻译」开关：整篇只要有一句非空译文就算「已经翻译过」，
    // 与 `AppState::translated_count() > 0` 完全同义。
    let check_untranslated = has_any_translation(segments);
    DerivedSnapshot {
        has_distinct_translation: has_distinct_translation(segments),
        has_any_translation: check_untranslated,
        has_copied_translation: has_copied_translations(segments),
        translated_out: translated_out_count(segments, target_lang),
        expected_translation: expected_translation_count(segments),
        other_lang_translated: translated_other_lang_count(segments, target_lang),
        stats: crate::subtitle::stats::compute(segments, media_secs, max_chars_per_line),
        quality_report: Arc::new(crate::subtitle::segment::quality_report(
            segments,
            glossary_entries,
            low_confidence_threshold,
            check_untranslated,
        )),
        // 时间轴窗口索引与上面几项共用同一份快照、同一把失效键，不另开缓存：
        // 它的唯一输入就是 `segments`，revision 一动就整体重建。
        timeline: build_timeline_index(segments),
    }
}

/// 派生快照的缓存命中判定：键一致直接复用上次那份 `Arc`，否则现算并换入。
///
/// 抽成自由函数（而不是内联在 `MainWindow` 方法里）是为了能在无 GUI 上下文的
/// 单测里钉死「同键不重算、换键必重算」两条性质——`MainWindow` 在 lib 单测里
/// 构造不出来。
pub(crate) fn cached_derived_snapshot(
    cache: &mut Option<(DerivedSnapshotKey, Arc<DerivedSnapshot>)>,
    key: DerivedSnapshotKey,
    build: impl FnOnce() -> DerivedSnapshot,
) -> Arc<DerivedSnapshot> {
    if let Some((cached_key, snapshot)) = cache.as_ref() {
        if cached_key == &key {
            return snapshot.clone();
        }
    }
    let snapshot = Arc::new(build());
    *cache = Some((key, snapshot.clone()));
    snapshot
}

/// 术语表违规下标缓存的键：`(术语表原文指纹, segments_revision)`。
///
/// 键用**原文的指纹**而不是解析后的条目：用户改术语表的行为都会改这串文本，
/// 而解析结果一旦因为上限截断等原因与原文不同步，就会出现「改了术语表、高亮不动」；
/// 也不存原文本身——与 [`DerivedSnapshotKey`] 共用 [`GlossaryFingerprint`]，
/// 命中与失效路径都不再 clone 术语表文本（与按原文相等等价的论证见指纹类型文档）。
pub(crate) type GlossaryBadKey = (GlossaryFingerprint, u64);

/// 术语表违规下标缓存：值存 `Arc<[usize]>`，命中时只克隆引用计数。
pub(crate) type GlossaryBadCache = Option<(GlossaryBadKey, Arc<[usize]>)>;

/// 术语表违规下标的缓存取数：键一致直接复用上次那个 `Arc`，否则现算并换入。
///
/// 抽成自由函数（不吃 `AppState`）是为了能在无 GUI 上下文的单测里钉死两条性质：
/// 「同键命中不重算、复用同一个 `Arc`」与「术语表文本或 `segments_revision` 变了必重算」。
/// 键里存的是[内容指纹](GlossaryFingerprint)而不是原文：原实现每次调用都
/// `glossary.clone()` 去拼键，等于白拷一份可能上 KB 的字符串，只为了做一次比较；
/// 指纹按值进键，命中与失效路径都不碰堆。与按原文相等等价的论证见该类型文档。
pub(crate) fn glossary_violations_cached(
    cache: &mut GlossaryBadCache,
    glossary: &str,
    segments_revision: u64,
    compute: impl FnOnce() -> Vec<usize>,
) -> Arc<[usize]> {
    // 指纹无论命中与否都只算一次：命中路径也靠它判等，失效路径直接复用这个值。
    let fingerprint = glossary_fingerprint(glossary);
    if let Some((cached_key, cached)) = cache.as_ref() {
        // 键判定与改造前等价：术语表内容指纹 + `segments_revision` 都相同才命中。
        if cached_key.1 == segments_revision && cached_key.0 == fingerprint {
            return Arc::clone(cached);
        }
    }
    let result: Arc<[usize]> = compute().into();
    // 新键只存定长指纹，不 clone 术语表原文（每帧最多一次，现在连那一次也省了）。
    *cache = Some(((fingerprint, segments_revision), Arc::clone(&result)));
    result
}

impl MainWindow {
    pub fn new(state: AppState, cx: &mut Context<Self>) -> Self {
        let metrics_rx = crate::utils::SystemMonitor::spawn_background_monitor();
        let text_focus = cx.focus_handle();
        let subtitle_search_focus = cx.focus_handle();
        let root_focus = cx.focus_handle();
        let api_base_focus = cx.focus_handle();
        let api_model_focus = cx.focus_handle();
        let api_key_focus = cx.focus_handle();
        let local_model_focus = cx.focus_handle();
        let inline_edit_focus = cx.focus_handle();
        let api_base_input = state.config.translate.api_base.clone();
        let api_model_input = state.config.translate.api_model.clone();
        let api_key_input = state.config.translate.api_key.clone();
        // 本地翻译模型路径：界面缓冲取自配置（`paths.llm_model`，可能是相对或绝对路径）
        let local_model_input = state.config.paths.llm_model.clone();
        // 预览框宽度：配置里存过就用存的，否则 `preview_box_w` 留空，
        // 由 `MainWindow::subtitle_box_w()` 按「单行最大字数 × 预览字号」自动推算
        let preview_box_w_initial = state.config.subtitle_style.preview_box_w;
        // 导出内容模式是长期偏好：从配置恢复，避免每次重启都跳回默认「双语」。
        let export_mode_initial = state.export_mode_from_config();
        // 导出格式同样是长期偏好，也必须在 `state` 被 move 进结构体之前取出。
        let export_format_initial =
            EditorExportFormat::from_config_str(&state.config.ui.export_format);
        // 启动即绑定全局子进程作业对象：此后所有转写/转码/预览子进程都挂在它名下，
        // 本进程一旦消失（正常退出、关窗、崩溃、被任务管理器结束），Windows 会连根
        // 清掉整棵进程树。这是唯一能覆盖「父进程被强杀」的兜底 —— 单靠退出路径里的
        // kill 逻辑在强杀场景下根本没有机会执行。非 Windows 平台为空实现。
        crate::utils::child_registry::ensure_job_object();
        // 三块默认全展开：改成一页的本意就是「一眼看全」，默认收起又变回分页了。
        // 想缩短页面的用户自己点「收起」即可（状态只在本次会话内保持）。
        let model_blocks_open = [true, true, true];
        // 替换规则集是独立 JSON（与 config.toml 同目录）：读失败不拦启动，把错误留给
        // 替换面板的提示行——规则集读不到不该让整个界面起不来。
        let (replace_rule_sets, replace_rules_status) = match load_replace_rule_sets_default() {
            Ok(sets) => (sets, None),
            Err(err) => (Vec::new(), Some((false, err))),
        };
        let replace_rule_selected = if replace_rule_sets.is_empty() {
            None
        } else {
            Some(0)
        };
        let mut window = Self {
            state,
            metrics_rx,
            text_focus,
            is_text_focused: false,
            is_extracting_frame: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_extract_time: Arc::new(std::sync::Mutex::new(None)),
            last_drag_extract: std::time::Instant::now(),
            play_tick_generation: 0,
            completion_dialog: None,
            benchmark_dialog: None,
            confirm_dialog: None,
            update_modal_open: false,
            notice: None,
            translate_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            model_download_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            update_download_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            // 导出格式也是长期偏好（与 `editor_export_mode` 同理）：从配置恢复，
            // 否则每次启动都跳回「剪映草稿」，用户得重选一遍。
            editor_export_format: export_format_initial,
            editor_export_mode: export_mode_initial,
            glossary_status: None,
            is_export_dropdown_open: false,
            ready_qc_open: false,
            outline_dropdown_open: false,
            bg_style_dropdown_open: false,
            library_sort_dropdown_open: false,
            preview_box_w: preview_box_w_initial,
            preview_drag: None,
            monitor_subtitle_drag: None,
            monitor_snap_x: false,
            monitor_snap_y: false,
            monitor_subtitle_selected: false,
            editor_split_w: None,
            editor_split_drag: None,
            subtitle_panel: EditorSubtitlePanel::default(),
            text_cursor_pos: 0,
            cached_live_image: None,
            cached_static_image: None,
            segments_flush_ticker: false,
            // 从 1 起：0 表示「还从未请求过扫描」，与派发序号同一口径。
            model_presence_scan_gen: std::sync::atomic::AtomicU64::new(1),
            cached_fallback_frame: None,
            subtitle_list_scroll: UniformListScrollHandle::new(),
            batch_queue_scroll: ScrollHandle::new(),
            queue_sidebar_open: true,
            subtitle_list_followed_sel: None,
            library_thumbs: HashMap::new(),
            library_thumb_inflight: HashSet::new(),
            library_sizes: HashMap::new(),
            subtitle_search: String::new(),
            subtitle_search_focus,
            root_focus,
            replace_panel_open: false,
            replace_find: String::new(),
            replace_with: String::new(),
            replace_case_sensitive: false,
            replace_include_translation: false,
            replace_status: None,
            replace_find_focus: cx.focus_handle(),
            replace_with_focus: cx.focus_handle(),
            focused_replace_field: None,
            replace_cursor: 0,
            replace_rule_sets,
            replace_rule_selected,
            replace_rules_open: false,
            replace_rules_status,
            export_template_focus: cx.focus_handle(),
            export_template_focused: false,
            export_template_cursor: 0,
            retime_op: 0,
            retime_shift_ms: 0,
            retime_scale_pct: 100,
            retime_target_secs: 600,
            subtitle_search_focused: false,
            subtitle_search_cursor: 0,
            subtitle_filter: Vec::new(),
            subtitle_filter_key: None,
            subtitle_search_cache: SubtitleSearchCache::new(),
            glossary_bad_cache: None,
            low_confidence_cache: None,
            derived_snapshot_cache: None,
            active_segment_cache: None,
            inline_edit_target: None,
            inline_edit_buffer: String::new(),
            inline_edit_cursor: 0,
            inline_edit_focus,
            last_subtitle_click: None,
            api_base_input,
            api_model_input,
            api_key_input,
            api_base_focus,
            api_model_focus,
            api_key_focus,
            local_model_input,
            local_model_focus,
            local_model_hint: None,
            focused_api_field: None,
            line_edit_cursor: 0,
            focused_style_field: None,
            style_field_input: String::new(),
            style_field_focus: cx.focus_handle(),
            api_key_visible: false,
            translate_lang_open: false,
            is_probing_translate: false,
            translate_probe_msg: None,
            library_filter: crate::utils::library_query::LibraryFilter::default(),
            library_sort: crate::utils::library_query::LibrarySort::default(),
            library_search_focus: cx.focus_handle(),
            library_search_focused: false,
            library_search_cursor: 0,
            library_query_cache: None,
            crash_notice: None,
            crash_record: None,
            library_selected: HashSet::new(),
            library_export_busy: false,
            command_palette: None,
            shortcuts_help_open: false,
            model_blocks_open,
            custom_color_picker_open: false,
            settings_tab: 0,
            subtitle_style_collapsed: false,
            bounds_subscription: None,
            cached_color_wheel: None,
            color_picker_brightness: 1.0,
        };

        // 恢复上次的批量队列：队列原本只在内存里，关窗 / 崩溃就全丢，用户排好的
        // 几十条与「已完成」的进度都得重来。
        window.state.restore_queue();
        // 检测上次运行是否崩溃过：panic 钩子只把堆栈写进日志，没有任何机制告诉用户，
        // 于是「崩了、重开、能用」的 bug 永远没人上报。
        if let Some(record) = Self::detect_crash_record() {
            window.crash_notice = Some(Self::crash_notice_text(&record));
            window.crash_record = Some(record);
        }

        // 若启动已载入历史视频工程，立即触发首帧提取，并按硬件策略补代理
        if window.state.selected_file.is_some() {
            window.trigger_extract_frame(cx);
            window.ensure_preview_proxy(cx);
            window.ensure_waveform(cx);
        }

        window
    }

    /// 检测上次运行的崩溃并生成一条待显示的中性提示（无崩溃则 `None`）。
    ///
    /// 抽成关联函数而不是内联：启动路径上已经够长，而这段逻辑与窗口状态无关
    /// （只读日志目录），单独可读、也可在别处复用。
    fn detect_crash_record() -> Option<crate::utils::crash_report::CrashRecord> {
        let log_dir = crate::utils::AppConfig::resolve_path("logs");
        crate::utils::crash_report::detect_previous_crash(&log_dir)
    }

    /// 崩溃横幅上那句话。
    ///
    /// 这里以前写「导出诊断可一并附上日志」，但诊断报告里**并不含日志正文**
    /// （它只采环境/路径/可写性，且有「同样输入 → 逐字节相同输出」的契约，
    /// 塞进日志就破坏了可复现性）。用户照着那句话去导出，拿到的报告里没有堆栈
    /// ——报障时最关键的那段恰好缺了。现在横幅自己带「复制报告」按钮（见
    /// `status_bar::render_crash_banner`），这句话只负责说清「发生了什么、去哪找堆栈」。
    fn crash_notice_text(record: &crate::utils::crash_report::CrashRecord) -> String {
        format!(
            "{}　（堆栈在程序目录的 logs 文件夹里，也可点右侧「复制报告」直接带走）",
            crate::utils::crash_report::short_summary(record)
        )
    }

    /// 「离开当前工程」前的脏数据守卫：关窗（系统关窗路径 Alt+F4 / 任务栏右键 /
    /// 系统关闭按钮，与自绘标题栏的关闭按钮共用）与**切工程**（历史库里点「校对」、
    /// 开工新一次转写）都走这一条。
    ///
    /// `segments_dirty` 的落库是去抖的（切句 / 跳转 / 播放 / 导出时才 flush），
    /// 用户改完字幕直接关窗会丢掉最后一段未落库的编辑，所以这里先同步 flush 一次。
    /// 只在「本次确实尝试保存了脏数据、且保存失败」时拦下关窗：若 `db_write_error`
    /// 是更早一次失败留下的旧值，不能拿它挡住一次与保存无关的正常关闭。
    ///
    /// 返回 `true` 表示可以继续（关窗 / 切工程）。`components/titlebar.rs` 的关闭按钮、
    /// `views/library.rs` 的「校对」、`actions::start_processing_for` 都调用本函数，
    /// 各条离开路径收敛到这里，不再各写一份判定——少写一处，那条路上未落库的编辑
    /// 就会被静默丢掉。
    pub(crate) fn flush_segments_before_close(&mut self, cx: &mut Context<Self>) -> bool {
        let had_dirty = self.state.segments_dirty;
        self.state.flush_segments_if_dirty();
        // F1：导入字幕的工程没有库主键，flush **无处可写**（不是「已保存」）。
        // 以前这条路径被当成保存成功，于是关窗/切工程把一整份校对静默清零。
        // 这里第一次拦下并给出出路（先导出），第二次放行 = 用户明确放弃修改。
        if had_dirty && self.state.active_task_id.is_none() && self.state.segments_dirty {
            if !self.state.draft_close_warned {
                self.state.draft_close_warned = true;
                self.state.db_write_error = Some(
                    "导入的字幕还不是一个工程，改动没有可写回的库记录：直接关闭会丢失本次编辑。\
                     请先「导出」保存字幕；再次关闭将放弃这些修改。"
                        .to_string(),
                );
                tracing::warn!("导入字幕有未保存编辑，已阻止关闭并给出保存出路");
                cx.notify();
                return false;
            }
            self.state.draft_close_warned = false;
            self.state.db_write_error = None;
            tracing::warn!("用户二次确认：放弃导入字幕的未保存编辑");
            return true;
        }
        if had_dirty && self.state.db_write_error.is_some() {
            // 取舍：**阻止关闭**而不是「记日志后照关」。字幕写库失败通常是持久性
            // 原因（磁盘满 / 数据库只读），一旦关窗这次编辑就永久丢了；留在窗口里，
            // 跨页常驻的 `db_write_error` 横幅会告诉用户「改动尚未写入历史库」。
            tracing::error!(
                error = ?self.state.db_write_error,
                "关窗前字幕落库失败，已阻止关闭以避免丢失未保存的编辑"
            );
            // 触发重绘，让错误横幅立刻可见
            cx.notify();
            return false;
        }
        true
    }

    /// 按当前搜索关键字重算字幕清单的可见行下标。
    ///
    /// 结果按 `(关键字, segments_revision)` 缓存：播放时界面每 40ms 重绘一次，
    /// 若每帧都对上千条字幕重跑子串匹配，滚动与播放都会白白掉帧。
    ///
    /// 用户**逐字输入**时的代价也被压掉了（那正是此前每敲一个字卡一下的来源）：
    /// 小写化后的搜索文本按修订号预建在 [`SubtitleSearchCache`] 里，改键只做逐句
    /// `contains`（逐句零分配）；新键是上一个键的延长时直接在上一份结果上收窄，
    /// 连全表都不必扫。结果与逐句调用 `matched_indices` 完全等价（见 `mod tests`
    /// 里与旧逻辑内联副本的对照测试）。
    pub(crate) fn refresh_subtitle_filter(&mut self) {
        let revision = self.state.segments_revision;
        let segment_count = self.state.segments.len();
        // 快路径：键与修订号都没变、这串下标也仍落在当前片段表内（见
        // [`filter_cache_usable`]）。撤销/重做会整体替换 `segments` 并重排序号，
        // 沿用越界下标会让虚拟列表量不到行高，整片清单塌成空白且刷不出来。
        //
        // 改造前这里要为每个键 clone 一份搜索串，播放期间逐帧一次堆分配；
        // 现在只做一次字符串内容比较。
        if matches!(
            self.subtitle_filter_key.as_ref(),
            Some((key, rev)) if *rev == revision && key == &self.subtitle_search
        ) && filter_cache_usable(&self.subtitle_filter, segment_count)
        {
            return;
        }
        // 索引按修订号重建（同一次编辑会话里的连续按键都命中同一份，零重建）；
        // 随后归一化关键字：能收窄就在上一份结果上收窄，否则全量扫描一次。
        self.subtitle_search_cache
            .sync(&self.state.segments, revision);
        self.subtitle_search_cache.set_needle(&self.subtitle_search);
        // 收窄还有个前提：上一份结果的下标都还在表内。片段表在别处被换过时
        // （修订号没跟上），宁可退回全量扫描，也不要在半截旧结果上做交集。
        let mut matched = if filter_cache_usable(&self.subtitle_filter, segment_count)
            && self.subtitle_search_cache.can_narrow(revision)
        {
            self.subtitle_search_cache.narrow(&self.subtitle_filter)
        } else {
            self.subtitle_search_cache.scan()
        };
        // 兜底：万一匹配结果本身越界（片段表在别处被换过），宁可退成「不过滤」，
        // 也不能把非法下标交给虚拟列表——空白列表比多显示几行难排查得多。
        if !indices_cover_segments(&matched, segment_count) {
            matched = (0..segment_count).collect();
        }
        self.subtitle_filter = matched;
        self.subtitle_filter_key = Some((self.subtitle_search.clone(), revision));
        self.subtitle_search_cache.commit(revision);
    }

    /// 术语表疑似未命中的句下标（按 `(术语表文本, segments_revision)` 缓存）。
    ///
    /// 术语表为空时直接返回空（零开销）；否则每帧最多重算一次，而不是在虚拟列表的
    /// 逐行闭包里现算——那会对「可视行 × 术语条数」重复匹配，纯属浪费。
    ///
    /// 返回 `Arc<[usize]>`：命中只克隆引用计数；键里存的是术语表原文的内容指纹
    /// （[`GlossaryFingerprint`]，与派生快照共用），不再像原实现那样每次调用都
    /// `glossary.clone()` 拼键 + `Vec::clone()` 深拷贝结果。
    pub(crate) fn cached_glossary_violations_arc(&mut self) -> Arc<[usize]> {
        let state = &self.state;
        let glossary = state.config.translate.glossary.as_str();
        let revision = state.segments_revision;
        glossary_violations_cached(&mut self.glossary_bad_cache, glossary, revision, || {
            state.glossary_violations()
        })
    }

    /// 兼容入口：仍返回 owned `Vec`。
    ///
    /// `actions.rs`（质检表的 `glossary_violations: Vec<usize>` 字段）与 `editor.rs`
    /// （逐帧转 `HashSet`）都按 `Vec` 消费，而这两个文件不在本次改动范围内；
    /// 这里保留原名与返回类型，内部走同一个 Arc 缓存——省掉的是「每帧 clone 术语表
    /// 文本」这条固定开销，最后一次 `to_vec()` 仍在（内容与顺序完全不变）。
    pub(crate) fn cached_glossary_violations(&mut self) -> Vec<usize> {
        self.cached_glossary_violations_arc().to_vec()
    }

    /// 低置信句下标（按 `(阈值, segments_revision)` 缓存）。
    ///
    /// 只收 `confidence` 有值的句子——`None` 的语义是「这条链路不产生置信度」
    /// （SenseVoice 全程如此），把它当低置信会让整片字幕标红，复核提示直接失去意义。
    /// 因此这里刻意**不**复用 `QualityReport`（那个还带术语/未翻译/碎片句三项，
    /// 剪辑台的高亮只关心「识别可能不准」这一件事），只取同一判据：`c < 阈值`。
    ///
    /// 阈值走配置（`pipeline.whisper_low_confidence`，默认 -0.35），与转写页质检卡
    /// 同源——同一份数据在两页给出同一个数字，不会「这页 3 句、那页 5 句」。
    pub(crate) fn cached_low_confidence(&mut self) -> Vec<usize> {
        let threshold = self.state.config.pipeline.whisper_low_confidence;
        let key = (threshold.to_bits(), self.state.segments_revision);
        if let Some((cached_key, cached)) = self.low_confidence_cache.as_ref() {
            if cached_key == &key {
                return cached.clone();
            }
        }
        let result = low_confidence_indices(&self.state.segments, threshold);
        self.low_confidence_cache = Some((key, result.clone()));
        result
    }

    /// 剪辑台渲染所需的派生统计快照（按 [`DerivedSnapshotKey`] 失效）。
    ///
    /// 渲染路径上所有「按字幕条数 O(n)、每帧重算、结果只随数据或配置变」的量都从
    /// 这里取：命中时只付一次键比较 + `Arc` 克隆（不是深拷贝——`QualityReport`
    /// 里是四个 `Vec`，逐帧克隆等于把省下的遍历又还回去）；未命中才扫一遍字幕表。
    pub(crate) fn derived_snapshot(&mut self) -> Arc<DerivedSnapshot> {
        // `max_chars_per_line` 是 u32、`stats::compute` 收 usize：显式转换，不用
        // `as`——虽然 u32→usize 在支持的目标平台上无损，但没必要留这个坑。
        let max_chars = usize::try_from(self.state.config.subtitle_style.max_chars_per_line)
            .unwrap_or(usize::MAX);
        let key = DerivedSnapshotKey {
            segments_revision: self.state.segments_revision,
            target_lang: self.state.translate_target_lang.clone(),
            low_confidence_bits: self.state.config.pipeline.whisper_low_confidence.to_bits(),
            glossary_fingerprint: glossary_fingerprint(&self.state.config.translate.glossary),
            max_chars_per_line: self.state.config.subtitle_style.max_chars_per_line,
            media_secs_bits: stats_media_secs(&self.state).to_bits(),
        };
        let state = &self.state;
        cached_derived_snapshot(&mut self.derived_snapshot_cache, key, || {
            // 术语表条目只在真正重算时才解析：逐帧解析一遍等于又添一处固定开销，
            // 而它只在这条失效路径上被需要。
            let entries = state.config.translate.glossary_entries();
            build_derived_snapshot(
                &state.segments,
                stats_media_secs(state),
                max_chars,
                &state.translate_target_lang,
                state.config.pipeline.whisper_low_confidence,
                &entries,
            )
        })
    }

    /// 当前播放时间命中的片段表下标（带缓存，见 [`ActiveSegmentCache`]）。
    ///
    /// 命中：上次那一条仍处在同一 `segments_revision` 下、且 `current_time` 仍落在
    /// 它的半开区间 `[start, end)` 内 → 一次区间比较，零扫描。播放时每 40ms 重绘
    /// 一帧，而时间基本都停在同一句里，命中率接近 100%。
    ///
    /// 未命中（跨句 / 跳转 / 时间轴被改）：回落 `AppState::get_active_segment()`
    /// 重新定位——判据的单一事实来源仍然是 `AppState`，这里只负责把它记住。回落
    /// 用**指针同一性**反查下标：`get_active_segment` 返回的就是 `segments` 里某个
    /// 元素的引用，`position(<同一指针>)` 必然命中，不必把它的 `find` 条件再抄一遍
    /// （抄一遍就多了一处会与 `AppState` 漂移的判据）。代价是跨句那一帧多扫一次表，
    /// 播放时每句只付一次。
    ///
    /// 这里**不**假设 `segments` 按 `start` 有序、也不做二分：拆句 / 合句 / 整轨
    /// 重定时 / 手动微调都会改相邻句的时间，`plan_time_edit` 只把区间夹在**它当时
    /// 认定的邻居**之间，顺序本身没有任何地方强制。缓存只回答「上一帧命中的那一条
    /// 是否仍覆盖当前时刻」，任何依赖顺序的推断都会在编辑后选错句。
    pub(crate) fn cached_active_segment_index(&mut self) -> Option<usize> {
        let t = self.state.current_time;
        if let Some(cache) = self.active_segment_cache {
            if cache.revision == self.state.segments_revision {
                if let Some(seg) = self.state.segments.get(cache.index) {
                    if editor::is_playing_at(seg.start, seg.end, t) {
                        return Some(cache.index);
                    }
                }
            }
        }
        let seg = self.state.get_active_segment()?;
        let index = self
            .state
            .segments
            .iter()
            .position(|candidate| std::ptr::eq(candidate, seg))?;
        self.active_segment_cache = Some(ActiveSegmentCache {
            revision: self.state.segments_revision,
            index,
        });
        Some(index)
    }

    /// 把配置输入框的编辑缓冲写回 `config.toml`（在线接口三项 + 本地模型路径）。
    /// 逐键落盘一个几百字节的 TOML 成本可忽略，换来的是「改完即生效、不会丢」。
    pub(crate) fn commit_api_field(&mut self, field: ApiField, cx: &mut Context<Self>) {
        match field {
            ApiField::Base => {
                self.state.config.translate.api_base = self.api_base_input.trim().to_string()
            }
            ApiField::Model => {
                self.state.config.translate.api_model = self.api_model_input.trim().to_string()
            }
            ApiField::Key => {
                self.state.config.translate.api_key = self.api_key_input.trim().to_string()
            }
            // 本地模型路径的所有副作用都收在 `set_local_model_path`：输入框逐键提交、
            // 「浏览…」选完文件、「恢复默认」清空，三条界面入口共用同一份
            // 落盘 / 校验 / 刷新逻辑，不会出现两套实现漂移。
            ApiField::LocalModelPath => {
                let path = self.local_model_input.clone();
                self.set_local_model_path(path, cx);
                return;
            }
        }
        // 落盘统一走 [`AppState::persist_config`]：此前这里 clone 一份配置丢到后台
        // 线程 `let _ = save_to_file(...)`，写盘失败彻底静默——用户逐键改完 API
        // 地址/密钥，重启发现又变回去却没有任何线索。同步写几百字节的 TOML 成本
        // 可忽略（与本文件其它落盘点同口径），失败会挂到常驻提示条。
        self.state.persist_config();
    }

    /// 开始排版参数就地输入（点击或双击触发）
    pub(crate) fn start_style_field_edit(
        &mut self,
        field: StyleField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cur_val_str = match field {
            StyleField::FontSize => self.state.config.subtitle_style.font_size.to_string(),
            StyleField::LetterSpacing => {
                self.state.config.subtitle_style.letter_spacing.to_string()
            }
            StyleField::BottomMargin => self.state.config.subtitle_style.bottom_margin.to_string(),
            StyleField::LineSpacing => {
                format!("{:.1}", self.state.config.subtitle_style.line_spacing)
            }
            StyleField::MaxChars => self
                .state
                .config
                .subtitle_style
                .max_chars_per_line
                .to_string(),
        };
        self.focused_style_field = Some(field);
        self.style_field_input = cur_val_str.clone();
        self.line_edit_cursor = cur_val_str.chars().count();
        window.focus(&self.style_field_focus);
        cx.notify();
    }

    /// 确认排版参数输入并落盘保存
    pub(crate) fn commit_style_field_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(field) = self.focused_style_field.take() {
            let s = self.style_field_input.trim();
            match field {
                StyleField::FontSize => {
                    if let Ok(v) = s.parse::<u32>() {
                        self.state.config.subtitle_style.font_size = v.clamp(10, 120);
                        self.state.save_subtitle_style();
                    }
                }
                StyleField::LetterSpacing => {
                    if let Ok(v) = s.parse::<u32>() {
                        self.state.config.subtitle_style.letter_spacing = v.clamp(0, 50);
                        self.state.save_subtitle_style();
                    }
                }
                StyleField::BottomMargin => {
                    if let Ok(v) = s.parse::<u32>() {
                        self.state.config.subtitle_style.bottom_margin = v.clamp(0, 500);
                        self.state.save_subtitle_style();
                    }
                }
                StyleField::LineSpacing => {
                    if let Ok(v) = s.parse::<f32>() {
                        let clamped = ((v * 10.0).round() / 10.0).clamp(0.5, 5.0);
                        self.state.config.subtitle_style.line_spacing = clamped;
                        self.state.save_subtitle_style();
                    }
                }
                StyleField::MaxChars => {
                    if let Ok(v) = s.parse::<u32>() {
                        self.state.config.subtitle_style.max_chars_per_line = v.clamp(4, 100);
                        self.state.save_subtitle_style();
                    }
                }
            }
            self.style_field_input.clear();
            cx.notify();
        }
    }

    /// 取消排版参数输入
    pub(crate) fn cancel_style_field_edit(&mut self, cx: &mut Context<Self>) {
        self.focused_style_field = None;
        self.style_field_input.clear();
        cx.notify();
    }

    /// 写入本地大模型（离线 Qwen 的 GGUF）路径：落盘 + 即时校验 + 刷新模型就位判定。
    ///
    /// 这是**三条界面入口的唯一汇聚点**：
    /// 1. 在路径输入框里逐键敲 / 粘贴（`commit_api_field` → 这里；
    ///    `Ctrl+V` 走 `apply_line_edit`，与逐键同一条路）；
    /// 2. 点「浏览…」用系统对话框选文件（`choose_llm_model_file` → 这里）；
    /// 3. 点「恢复默认」清空路径（传空串 → 这里，落回内置 Qwen）。
    ///
    /// 三个副作用缺一不可，集中在此避免两套实现漂移：
    /// - **落盘**：`config.toml` 的 `paths.llm_model`，下次启动仍生效；
    /// - **校验**：`validate_llm_model_path` 给即时提示，但**不阻止保存**——
    ///   用户可能先把路径填好、盘稍后才挂载，硬拦会让他没法先填；
    /// - **刷新就位判定**：`refresh_model_presence` 重扫「qwen-llm / llama-cpp」
    ///   是否命中新路径，否则性能页的模型管理卡与剪辑台翻译提示还按旧路径显示。
    ///
    /// 空串 = 恢复内置默认模型：用户把输入框清空就是在表达这个意图，
    /// 比起让 `paths.llm_model` 变成空串（那时 `is_present` 恒假、翻译必失败），
    /// 回退到默认路径更符合直觉。
    /// 展开「模型与组件」页的某个分块（跳转用）。
    ///
    /// 使用场景：用户点右侧抽屉里的模型卡片或「识别引擎」时，目标条目在
    /// 「模型与组件」页里。分块可折叠之后，若目标块恰好是收起的，跳过去只会看到
    /// 一个标题——所以跳转必须顺带把它展开，否则「点了没反应」。
    ///
    /// 刻意**不滚动**页面：滚动位置由布局决定，这里只管可见性。
    pub(crate) fn expand_model_block(&mut self, block: ModelManagerTab) {
        self.model_blocks_open[block as usize] = true;
    }

    pub(crate) fn set_local_model_path(&mut self, raw: String, cx: &mut Context<Self>) {
        let path = raw.trim().to_string();
        // 界面缓冲同步成规范化后的值：粘贴带首尾空格的路径时，输入框不该留着空格。
        self.local_model_input = path.clone();
        if path.is_empty() {
            self.state.config.paths.llm_model = String::new();
            self.local_model_hint = Some((false, "未配置模型路径".to_string()));
        } else {
            self.state.config.paths.llm_model = path.clone();
            self.local_model_hint = crate::utils::config::validate_llm_model_path(&path)
                .err()
                .map(|e| (false, e))
                .or_else(|| Some((true, "路径有效".to_string())));
        }
        // 模型换了，重算「已就位」判定（否则模型管理卡还按旧路径显示）
        self.state.refresh_model_presence();
        self.state.save_translate_config();
        cx.notify();
    }

    /// 渲染单个 API 配置输入框（在线翻译接口的基址 / 模型名 / 密钥）。
    ///
    /// 自绘输入框拿不到系统 IME 组合态，因此这里只实现「可打印字符直插 + 退格/方向/
    /// Home/End + Ctrl+A/C/V」这套最小集合（见 [`apply_line_edit`]）；中文靠粘贴。
    pub(crate) fn render_api_input(
        &mut self,
        id: &'static str,
        field: ApiField,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = match field {
            ApiField::Base => self.api_base_focus.clone(),
            ApiField::Model => self.api_model_focus.clone(),
            ApiField::Key => self.api_key_focus.clone(),
            ApiField::LocalModelPath => self.local_model_focus.clone(),
        };
        let is_focused = self.focused_api_field == Some(field);
        // 密钥默认掩码显示，避免录屏/截图把密钥带出去
        let masked = field == ApiField::Key && !self.api_key_visible;
        let raw = match field {
            ApiField::Base => self.api_base_input.clone(),
            ApiField::Model => self.api_model_input.clone(),
            ApiField::Key => self.api_key_input.clone(),
            ApiField::LocalModelPath => self.local_model_input.clone(),
        };
        let char_count = raw.chars().count();
        let cursor = self.line_edit_cursor.min(char_count);
        let placeholder = match field {
            ApiField::Base => "https://api.deepseek.com/v1",
            ApiField::Model => "deepseek-chat",
            ApiField::Key => "sk-...（留空则读环境变量 VOICE2WORD_API_KEY）",
            ApiField::LocalModelPath => "models/llm/qwen2.5-0.5b-instruct-q4_k_m.gguf",
        };
        let display: String = if masked {
            "•".repeat(char_count)
        } else {
            raw
        };
        let before: String = display.chars().take(cursor).collect();
        let after: String = display.chars().skip(cursor).collect();

        let has_content = char_count > 0;
        let input_box = primitives::text_input(is_focused, 200.0)
            .id(id)
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&focus);
                    if this.focused_api_field != Some(field) {
                        this.focused_api_field = Some(field);
                        this.line_edit_cursor = match field {
                            ApiField::Base => this.api_base_input.chars().count(),
                            ApiField::Model => this.api_model_input.chars().count(),
                            ApiField::Key => this.api_key_input.chars().count(),
                            ApiField::LocalModelPath => this.local_model_input.chars().count(),
                        };
                    }
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let buffer = match field {
                    ApiField::Base => &mut this.api_base_input,
                    ApiField::Model => &mut this.api_model_input,
                    ApiField::Key => &mut this.api_key_input,
                    ApiField::LocalModelPath => &mut this.local_model_input,
                };
                let cursor = &mut this.line_edit_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    cx.stop_propagation();
                    this.commit_api_field(field, cx);
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child(placeholder)
            } else {
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(display)
            });

        div()
            .flex()
            .items_center()
            .gap_1p5()
            .w_full()
            .child(div().flex_1().min_w(px(0.0)).child(input_box))
            .children(has_content.then(|| {
                primitives::chip_clickable("清空", false, false)
                    .id(SharedString::from(format!("clear-{id}")))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match field {
                            ApiField::Base => this.api_base_input.clear(),
                            ApiField::Model => this.api_model_input.clear(),
                            ApiField::Key => this.api_key_input.clear(),
                            ApiField::LocalModelPath => this.local_model_input.clear(),
                        };
                        this.line_edit_cursor = 0;
                        this.commit_api_field(field, cx);
                        cx.notify();
                    }))
            }))
            .child(
                primitives::chip_clickable("粘贴", false, false)
                    .id(SharedString::from(format!("paste-{id}")))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(item) = cx.read_from_clipboard() {
                            if let Some(text) = item.text() {
                                let flat = text.trim().replace(['\r', '\n'], "");
                                match field {
                                    ApiField::Base => this.api_base_input = flat,
                                    ApiField::Model => this.api_model_input = flat,
                                    ApiField::Key => this.api_key_input = flat,
                                    ApiField::LocalModelPath => this.local_model_input = flat,
                                };
                                this.line_edit_cursor = match field {
                                    ApiField::Base => this.api_base_input.chars().count(),
                                    ApiField::Model => this.api_model_input.chars().count(),
                                    ApiField::Key => this.api_key_input.chars().count(),
                                    ApiField::LocalModelPath => {
                                        this.local_model_input.chars().count()
                                    }
                                };
                                this.commit_api_field(field, cx);
                                cx.notify();
                            }
                        }
                    })),
            )
            .into_any_element()
    }
}

impl MainWindow {
    /// 打开 / 关闭命令面板 (P1-A10)。`Ctrl+K` 与面板内的再次 `Ctrl+K` 都走这里。
    ///
    /// 打开时把焦点交给面板自己的搜索框：面板一出现就该能直接打字，不该再要求
    /// 用户点一下输入框。焦点句柄随面板一起创建、随面板一起丢弃（关掉即失焦），
    /// 所以不会像常驻输入框那样出现「面板没了焦点还挂着」的错位外观。
    pub(crate) fn toggle_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            self.close_command_palette(cx);
            return;
        }
        // 两层浮层不叠加：Ctrl+K 在速查面板上按下时，语义就是「换成命令面板」。
        self.shortcuts_help_open = false;
        let focus = cx.focus_handle();
        self.command_palette = Some(CommandPaletteState {
            query: String::new(),
            cursor: 0,
            selected: 0,
            focus: focus.clone(),
            scroll: ScrollHandle::new(),
        });
        window.focus(&focus);
        cx.notify();
    }

    /// 收起命令面板（Esc / Ctrl+K / 点遮罩 / 执行命令后都收敛到这里）。
    pub(crate) fn close_command_palette(&mut self, cx: &mut Context<Self>) {
        if self.command_palette.take().is_some() {
            cx.notify();
        }
    }

    /// 打开 / 关闭快捷键速查面板 (F1)。
    ///
    /// 命令面板开着时不抢 F1：那种状态下面板是唯一的键盘入口（见
    /// [`Self::overlay_blocks_keys`]），在它上面再叠一层速查面板只会让 Esc 到底关哪一层
    /// 变得说不清。
    pub(crate) fn toggle_shortcuts_help(&mut self, cx: &mut Context<Self>) {
        if self.command_palette.is_some() {
            return;
        }
        self.shortcuts_help_open = !self.shortcuts_help_open;
        cx.notify();
    }

    /// 收起快捷键速查面板（Esc / F1 再按一次 / 点遮罩都收敛到这里）。
    pub(crate) fn close_shortcuts_help(&mut self, cx: &mut Context<Self>) {
        if self.shortcuts_help_open {
            self.shortcuts_help_open = false;
            cx.notify();
        }
    }

    /// 是否有「独占键盘」的浮层正开着（命令面板 / 快捷键速查面板）。
    ///
    /// 两者都盖住整个窗口，浮层在时底下的界面应该是「看不见也摸不着」的。少了这道闸，
    /// 用户在速查面板上按 Delete 会静默删掉一句字幕（面板挡着屏幕，他根本看不到自己
    /// 删了什么）、按 Ctrl+Z 会静默撤销编辑、按 Ctrl+E 会直接往磁盘写一份文件。
    fn overlay_blocks_keys(&self) -> bool {
        self.command_palette.is_some() || self.shortcuts_help_open
    }

    /// 采集一份 [`PaletteContext`]：判据需要的状态位都在这里集中读取。
    ///
    /// 判据本身是纯函数（见 [`PaletteActionSpec::available`]），这里只负责把 `AppState`
    /// 上的状态位照原样搬过去，不参与任何判断，避免「界面按钮看一个条件、面板看另一个」。
    fn palette_context(&self) -> PaletteContext {
        let selected_pos = self.state.selected_segment_index.and_then(|index| {
            self.state
                .segments
                .iter()
                .position(|seg| seg.index == index)
        });
        let selected = selected_pos.and_then(|pos| self.state.segments.get(pos));
        PaletteContext {
            active_tab: self.state.active_tab,
            processing: matches!(self.state.status, ProcessStatus::Processing { .. }),
            has_source_file: self.state.transcribe_file.is_some(),
            has_segments: !self.state.segments.is_empty(),
            translating: self.state.is_translating,
            can_undo: self.state.can_undo(),
            can_redo: self.state.can_redo(),
            has_selection: selected.is_some(),
            selection_splittable: selected
                .is_some_and(|seg| seg.display_text().chars().count() > 1),
            selection_has_next: selected_pos.is_some_and(|pos| pos + 1 < self.state.segments.len()),
            selection_has_translation: selected.is_some_and(|seg| seg.has_translation()),
            has_replace_rule_set: self
                .replace_rule_selected
                .is_some_and(|pos| pos < self.replace_rule_sets.len()),
            has_media_file: self.state.selected_file.is_some(),
            clip_export_idle: !crate::ui::actions::CLIP_EXPORT_BUSY
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// 命令面板里这条命令当前是否可用。
    ///
    /// 判据来自注册表（[`PaletteActionSpec::available`]）：命令与它的可用条件写在同一条
    /// 注册项里，新增命令时不会出现「界面加了、判据忘了」。注册表里查不到时返回 `false`
    /// （安全侧：宁可不给入口，也不放行一条没有判据的命令）。
    fn palette_command_enabled(&self, command: PaletteCommand) -> bool {
        let ctx = self.palette_context();
        PALETTE_ACTIONS
            .iter()
            .find(|spec| spec.command == command)
            .is_some_and(|spec| (spec.available)(&ctx))
    }

    /// 执行一条命令面板命令。
    ///
    /// 每个分支都只调用 `MainWindow` 上已有的方法（与对应按钮/快捷键完全同源），
    /// 因此这里没有任何业务逻辑，也不该长出业务逻辑——新增命令时若发现需要在这里
    /// 写判断，说明缺的是 `actions.rs` 里的一个方法，而不是面板里的一段代码。
    ///
    /// 唯一的例外是整轨重定时的两条固定预设（±1 秒 / 铺满）：剪辑台那两个入口是
    /// 私有方法，命令面板复用同一份 `subtitle::timing` 纯函数，参数写死（面板没有滑条）。
    fn run_palette_command(&mut self, command: PaletteCommand, cx: &mut Context<Self>) {
        if !self.palette_command_enabled(command) {
            // 置灰命令被 Enter/点击触发时什么都不做，也不收起面板：
            // 用户看得到它为什么是灰的，换个条件再试即可。
            cx.notify();
            return;
        }
        // 先关面板再执行：命令可能弹确认框 / 系统文件对话框 / 完成弹窗，
        // 面板留着会盖在它们上面，用户会以为「按了没反应」。
        self.command_palette = None;
        match command {
            PaletteCommand::OpenFile => self.choose_file(cx),
            PaletteCommand::StartTranscription => self.start_processing(cx),
            PaletteCommand::CancelTranscription => self.request_cancel_processing(cx),
            PaletteCommand::ExportCurrentFormat => self.perform_editor_export(cx),
            PaletteCommand::StartTranslation => self.trigger_llm_translation(cx),
            PaletteCommand::CancelTranslation => self.cancel_llm_translation(cx),
            // 切页签与左侧导航栏走同一套副作用（剪辑台要补抽帧、视频库要刷新列表）
            PaletteCommand::SwitchToEditor => {
                self.state.active_tab = WorkspaceTab::Editor;
                self.trigger_extract_frame(cx);
            }
            PaletteCommand::SwitchToGenerate => {
                self.state.active_tab = WorkspaceTab::Generate;
            }
            PaletteCommand::SwitchToLibrary => {
                self.state.active_tab = WorkspaceTab::Library;
                self.state.refresh_recent_tasks();
            }
            PaletteCommand::SwitchToPerformance => {
                self.state.active_tab = WorkspaceTab::Performance;
            }
            PaletteCommand::Undo => {
                if self.state.undo() {
                    // 与 `shortcuts::Undo` 的挂载点一致：撤销可能整体换掉片段表，
                    // 不清这个标记虚拟列表会停在旧行号上。
                    self.subtitle_list_followed_sel = None;
                }
            }
            PaletteCommand::Redo => {
                if self.state.redo() {
                    self.subtitle_list_followed_sel = None;
                }
            }
            PaletteCommand::OpenReplace => {
                // 与 Ctrl+H 的挂载点同源：切页 + 展开 + 光标置尾（不抢焦点——面板
                // 已关，用户接下来多半是点输入框；强制抢焦点会让面板看起来「自己
                // 弹出来还锁住了键盘」）。
                self.state.active_tab = WorkspaceTab::Editor;
                self.replace_panel_open = true;
                self.replace_cursor = self.replace_find.chars().count();
            }
            PaletteCommand::ExportQcSheet => {
                self.export_review_sheet(crate::subtitle::qc::QcFormat::Csv, cx)
            }
            PaletteCommand::ExportDiagnostics => self.export_diagnostics(cx),
            // ── 剪辑台动作：与剪辑台里的按钮/入口走同一批方法，不另写业务逻辑 ──
            PaletteCommand::SplitSegment => {
                // `None`：命令面板没有「光标位置」概念，按中点劈开（与剪辑台未定位光标时一致）。
                self.state.split_selected_segment(None);
                self.trigger_extract_frame(cx);
            }
            PaletteCommand::MergeWithNext => {
                self.state.merge_selected_with_next();
                self.trigger_extract_frame(cx);
            }
            PaletteCommand::ClearTranslation => self.state.set_selected_translation(""),
            PaletteCommand::ApplyCleanup => {
                self.state.apply_cleanup();
            }
            PaletteCommand::ApplyReplaceRuleSet => {
                self.apply_selected_replace_rule_set(ReplaceRuleScope::AllSegments, cx)
            }
            PaletteCommand::ExportSelectedClip => self.export_selected_clip(cx),
            PaletteCommand::ExportSelectedClipAudio => self.export_selected_clip_audio(cx),
            PaletteCommand::ShiftTrackForward => self.apply_track_shift(
                crate::subtitle::timing::TimeShift::ShiftAll { delta: 1.0 },
                cx,
            ),
            PaletteCommand::ShiftTrackBackward => self.apply_track_shift(
                crate::subtitle::timing::TimeShift::ShiftAll { delta: -1.0 },
                cx,
            ),
            PaletteCommand::FitTrackToMedia => {
                let target_secs = self.state.total_duration;
                self.apply_track_shift(
                    crate::subtitle::timing::TimeShift::FitToDuration { target_secs },
                    cx,
                )
            }
            PaletteCommand::ApplyMinGap => self.apply_default_min_gap(cx),
        }
        cx.notify();
    }

    /// 命令面板的整轨重定时入口：把 `shift` 应用到整条时间轴。
    ///
    /// 写回步骤与剪辑台的整轨变换同源（记撤销快照 → 自纠重叠 → 置脏 → 递增修订号
    /// → 落库 → 清派生缓存键 → 给提示），但参数是**固定的预设**：命令面板没有滑条可拖，
    /// 只能给明确的「±1 秒 / 铺满」。无变动时不压撤销点，免得空操作占掉一层 `Ctrl+Z`。
    fn apply_track_shift(
        &mut self,
        shift: crate::subtitle::timing::TimeShift,
        cx: &mut Context<Self>,
    ) {
        use crate::subtitle::timing::{plan_retime, resolve_overlaps_in_place};
        if self.state.segments.is_empty() {
            return;
        }
        let plan = plan_retime(&self.state.segments, shift);
        if plan.changed == 0 && !plan.overlaps {
            self.notice = Some("整轨时间轴无需调整".to_string());
            cx.notify();
            return;
        }
        self.state.snapshot_for_undo();
        // 原地消重叠（不排序）：乱序导入的字幕不能被重排后 zip 回去。
        let mut resolved = plan.times.clone();
        resolve_overlaps_in_place(&mut resolved);
        for (seg, (start, end)) in self.state.segments.iter_mut().zip(resolved) {
            seg.start = start;
            seg.end = end;
        }
        // 改的是 `segments`，必须置脏才会落库（`bump_segments_revision` 只驱动清单缓存失效）。
        self.state.segments_dirty = true;
        self.state.bump_segments_revision();
        self.state.flush_segments_if_dirty();
        // 时间轴变了：波形/监视器的定位与质检缓存都要跟着重算。
        self.subtitle_filter_key = None;
        self.notice = Some(format!(
            "已调整整轨时间轴（{} 句变动{}）",
            plan.changed,
            if plan.overlaps {
                "，重叠已压平"
            } else {
                ""
            }
        ));
        cx.notify();
    }

    /// 命令面板的「句间最小间隔」入口：用默认间隔（40ms）消除相邻字幕相贴。
    ///
    /// 与 [`MainWindow::apply_track_shift`] 同源的写回口径；命中为 0 时不压撤销点。
    fn apply_default_min_gap(&mut self, cx: &mut Context<Self>) {
        use crate::subtitle::timing::{plan_min_gaps, DEFAULT_MIN_GAP_SECS};
        if self.state.segments.is_empty() {
            return;
        }
        let plan = plan_min_gaps(&self.state.segments, DEFAULT_MIN_GAP_SECS);
        if plan.changed == 0 && plan.shortened == 0 {
            self.notice = Some("相邻字幕间隔已满足，无需调整".to_string());
            cx.notify();
            return;
        }
        let changed = plan.changed;
        let shortened = plan.shortened;
        self.state.snapshot_for_undo();
        for (seg, (start, end)) in self.state.segments.iter_mut().zip(plan.times) {
            seg.start = start;
            seg.end = end;
        }
        self.state.segments_dirty = true;
        self.state.bump_segments_revision();
        self.state.flush_segments_if_dirty();
        self.subtitle_filter_key = None;
        self.notice = Some(format!(
            "已应用句间最小间隔（{changed} 句起点后移，{shortened} 句收短）"
        ));
        cx.notify();
    }

    /// 移动结果列表里的选中行（`↑` / `↓`，越界即夹住不回绕）。
    fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(state) = self.command_palette.as_mut() else {
            return;
        };
        let count = filter_commands(&state.query).len();
        if count == 0 {
            state.selected = 0;
            cx.notify();
            return;
        }
        let next = move_palette_selection_row(state.selected, delta, count);
        state.selected = next;
        // 选中行滚进视野，否则过滤出十几条时按 ↓ 会「选到看不见的地方」
        state.scroll.scroll_to_item(next);
        cx.notify();
    }

    /// 执行结果列表里当前选中的那条命令（`Enter`）。
    fn run_selected_palette_command(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.command_palette.as_ref() else {
            return;
        };
        let matched = filter_commands(&state.query);
        // 选中行可能因为别处改状态而落在结果集之外（渲染时是夹住的），这里同样夹一次，
        // 否则「界面上高亮第 3 条、Enter 却什么都不做」会让人以为面板坏了。
        let row = state.selected.min(matched.len().saturating_sub(1));
        let Some(&index) = matched.get(row) else {
            // 查询无结果时 Enter 不做任何事（尤其不能误触终止转写那类命令）
            cx.notify();
            return;
        };
        let command = PALETTE_ACTIONS[index].command;
        self.run_palette_command(command, cx);
    }

    /// 渲染命令面板 (P1-A10)：居中模态（复用 `modal_scrim` / `modal_card`）
    /// + 自绘搜索框（复用 `apply_line_edit`）+ 可滚动结果列表 + 选中高亮。
    ///
    /// 键盘分工：`↑/↓/Enter` 走搜索框的 `on_key_down`（这三个键没有全局绑定，
    /// 不会被动作分发先截走）；`Esc` / `Ctrl+K` 走全局动作，落点在 `Render::render`
    /// 的 `CancelOrClose` 优先级链与 `OpenCommandPalette` 上。
    pub(crate) fn render_command_palette(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (query, cursor, selected, focus, scroll) = match self.command_palette.as_ref() {
            Some(state) => (
                state.query.clone(),
                state.cursor,
                state.selected,
                state.focus.clone(),
                state.scroll.clone(),
            ),
            // 调用点已判过 `is_some()`；这里兜底成空面板，避免多一次 unwrap 风险。
            None => (String::new(), 0, 0, cx.focus_handle(), ScrollHandle::new()),
        };
        let is_focused = focus.contains_focused(window, cx);
        // 名字过滤与可用性判据都是纯函数（见 `filter_commands` 与
        // `PaletteActionSpec::available`，都有单测）；这里只负责把状态搬进判据，
        // 不可用的命令**照常显示**、置灰，而不是从列表里消失。
        let entries: Vec<(usize, bool)> = filter_commands(&query)
            .into_iter()
            .map(|index| {
                (
                    index,
                    self.palette_command_enabled(PALETTE_ACTIONS[index].command),
                )
            })
            .collect();
        let selected = selected.min(entries.len().saturating_sub(1));
        let char_count = query.chars().count();
        let cursor = cursor.min(char_count);
        let before: String = query.chars().take(cursor).collect();
        let after: String = query.chars().skip(cursor).collect();

        let search_input = primitives::text_input(is_focused, 200.0)
            .id("command-palette-input")
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(state) = this.command_palette.as_ref() {
                        window.focus(&state.focus);
                    }
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                match event.keystroke.key.as_str() {
                    "enter" => {
                        this.run_selected_palette_command(cx);
                        return;
                    }
                    "up" => {
                        this.move_palette_selection(-1, cx);
                        return;
                    }
                    "down" => {
                        this.move_palette_selection(1, cx);
                        return;
                    }
                    _ => {}
                }
                let Some(state) = this.command_palette.as_mut() else {
                    return;
                };
                let (buffer, cursor) = (&mut state.query, &mut state.cursor);
                if apply_line_edit(buffer, cursor, event, cx) {
                    // 查询词一变，原来选中的「第 N 条」已经不是同一条命令了，
                    // 回到第一条才不会让 Enter 打到用户没看见的命令上。
                    state.selected = 0;
                    state.scroll.scroll_to_item(0);
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child("输入命令名或英文别名…")
            } else {
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(query.clone())
            });

        let list = div()
            .id("command-palette-list")
            .w_full()
            .max_h(px(COMMAND_PALETTE_LIST_H))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_0_5))
            .track_scroll(&scroll)
            .children(entries.iter().enumerate().map(|(row, &(index, enabled))| {
                let spec = &PALETTE_ACTIONS[index];
                let command = spec.command;
                let is_sel = row == selected;
                let title_color = if !enabled {
                    Theme::text_disabled()
                } else if is_sel {
                    Theme::text_primary()
                } else {
                    Theme::text_secondary()
                };
                div()
                    .id(("palette-cmd", row))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(Theme::SPACE_3))
                    .px(px(Theme::SPACE_2))
                    .h(px(Theme::CTRL_H_SM))
                    .rounded(px(Theme::RADIUS_SM))
                    .bg(if is_sel {
                        Theme::tint_blue_badge()
                    } else {
                        Theme::transparent()
                    })
                    .when(enabled, |s| {
                        s.cursor_pointer().hover(|s| s.bg(Theme::bg_hover()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(state) = this.command_palette.as_mut() {
                            state.selected = row;
                        }
                        this.run_palette_command(command, cx);
                        // 行点击已在冒泡路径上处理完，别再让遮罩把面板
                        // 也当成「点了空白处」——那会把灰命令的点击变成关闭。
                        cx.stop_propagation();
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(title_color)
                                    .child(spec.zh),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child(spec.en),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .font_family("Consolas")
                            .text_color(Theme::text_muted())
                            .child(spec.hint),
                    )
            }))
            .when(entries.is_empty(), |s| {
                s.child(
                    div()
                        .px(px(Theme::SPACE_2))
                        .py(px(Theme::SPACE_3))
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_muted())
                        .child("没有匹配的命令"),
                )
            });

        primitives::modal_scrim()
            .id("command-palette-scrim")
            // 遮罩挡住底下的鼠标命中：GPUI 的点击会沿命中链从最上层往下冒泡，
            // 不 occlude 的话「点面板外面关掉面板」会顺带把底下那个按钮也点下去
            // （面板可以从任何页签打开，底下什么控件都有可能）。
            .occlude()
            // 点遮罩 = 收起面板（与 Esc 同义，符合「点外面关掉浮层」的直觉）
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_command_palette(cx);
            }))
            .child(
                primitives::modal_card(Theme::DIALOG_W, Theme::PAGE_PAD)
                    .id("command-palette-card")
                    // 卡片自身吞掉点击，否则点列表空白处会被遮罩当成「点外面」
                    .on_click(cx.listener(|_, _, _, _| {}))
                    .gap(px(Theme::SPACE_2))
                    .child(search_input)
                    .child(list)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(format!("{} 条命令", entries.len()))
                            .child("↑↓ 选择 · Enter 执行 · Esc 关闭"),
                    ),
            )
    }

    /// 任一文本输入类控件是否持有焦点。
    ///
    /// 裸键（Space / Delete）只有在该判定为 `false` 时才真正生效：GPUI 的动作在冒泡
    /// 阶段默认停止传播，若在输入框内不主动 `cx.propagate()`，空格会被动作系统吃掉，
    /// 用户就打不出空格。这里集中聚合所有「已真实接线」的输入焦点句柄，
    /// 避免每加一个裸键都要重新列一遍。
    fn any_input_focused(&self, window: &Window, cx: &App) -> bool {
        self.text_focus.contains_focused(window, cx)
            || self.subtitle_search_focus.contains_focused(window, cx)
            || self.replace_find_focus.contains_focused(window, cx)
            || self.replace_with_focus.contains_focused(window, cx)
            || self.export_template_focus.contains_focused(window, cx)
            || self.style_field_focus.contains_focused(window, cx)
            || self.inline_edit_focus.contains_focused(window, cx)
            || self.api_base_focus.contains_focused(window, cx)
            || self.api_model_focus.contains_focused(window, cx)
            || self.api_key_focus.contains_focused(window, cx)
            || self.local_model_focus.contains_focused(window, cx)
            // 视频库「筛选关键字」输入框：与字幕搜索同法。若漏掉它，用户在库里敲空格/
            // Delete 会被裸键动作（播放/暂停、删当前句）吃掉，既打不出空格又误触发操作。
            || self.library_search_focus.contains_focused(window, cx)
    }

    /// 启动「脏字幕延迟落库」定时器（进程内只起一次）。
    ///
    /// # 为什么需要它
    ///
    /// 跳转 / 切句 / 暂停播放 / 就地改一句这些高频交互现在只登记 `segments_flush_due`
    /// （见 `AppState::defer_segments_flush`），真正写库得有人来触发。拖时间轴时
    /// `seek_to` 每 40ms 一次，而一次写库要序列化整份字幕（5000 句约 10~30ms）——
    /// 逐次写就是「拖一下卡一下」。这里每 500ms 查一次「停手够久了吗」。
    ///
    /// # 为什么常驻而不是按需起停
    ///
    /// 登记点在 `AppState` 里，那里拿不到 `cx`；要按需起就得让每个调用点都带上上下文。
    /// 常驻的代价是每 500ms 一次空转检查：未登记时只是一次 `Option::is_none`，
    /// 与已经在跑的 40ms 播放 tick 相比可以忽略。
    fn ensure_segments_flush_ticker(&mut self, cx: &mut Context<Self>) {
        if self.segments_flush_ticker {
            return;
        }
        self.segments_flush_ticker = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let alive = this
                .update(cx, |this, cx| {
                    if this.state.flush_segments_if_due() {
                        // 真的写了库：界面上的「已保存 / 未保存」类提示要跟着刷新。
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 脏字幕的延迟落库定时器：首次渲染时起一次，之后常驻。
        self.ensure_segments_flush_ticker(cx);
        // 监听窗口尺寸变化（包括最大化/还原/拖拽尺寸/贴靠）：
        // Windows 上当窗口尺寸改变时，强制刷新客户区并驱动 GPUI DirectX 渲染层重绘全幅，
        // 避免出现「最大化后只渲染局部、截屏/切焦点后才全屏」的画面不同步 Bug。
        if self.bounds_subscription.is_none() {
            self.bounds_subscription = Some(cx.observe_window_bounds(window, |_this, window, cx| {
                #[cfg(target_os = "windows")]
                crate::ui::components::titlebar::win_drag::force_update_window(window);
                cx.notify();
            }));
        }
        // 必需组件（ffmpeg / whisper-cli）缺失时的自动补下载：进程内只检查一次，
        // 下载是网络活，不能压在首帧之前，但也要在用户动手之前给反馈。
        self.check_required_components_once(cx);
        if self.metrics_rx.has_changed().unwrap_or(false) {
            self.state.metrics = self.metrics_rx.borrow_and_update().clone();
        }

        // 输入框的「聚焦」外观（高亮边框 / 光标）必须跟着窗口的真实焦点走。
        // 这几个布尔量原本只在点击时被置 `true`、从不复位，于是搜索框、字幕文本编辑框、
        // 翻译 API 输入框只要被点过一次，就会永远显示成编辑态——多个输入框同时高亮，
        // 界面与实际焦点不符。每帧按真实焦点回收一次即可。
        let search_focused = self.subtitle_search_focus.contains_focused(window, cx);
        let text_editor_focused = self.text_focus.contains_focused(window, cx);
        let api_base_focused = self.api_base_focus.contains_focused(window, cx);
        let api_model_focused = self.api_model_focus.contains_focused(window, cx);
        let api_key_focused = self.api_key_focus.contains_focused(window, cx);
        let local_model_focused = self.local_model_focus.contains_focused(window, cx);
        let replace_find_focused = self.replace_find_focus.contains_focused(window, cx);
        let replace_with_focused = self.replace_with_focus.contains_focused(window, cx);
        self.export_template_focused = self.export_template_focus.contains_focused(window, cx);
        self.focused_replace_field = if replace_find_focused {
            Some(ReplaceField::Find)
        } else if replace_with_focused {
            Some(ReplaceField::With)
        } else {
            None
        };
        self.subtitle_search_focused = search_focused;
        self.is_text_focused = text_editor_focused;
        self.library_search_focused = self.library_search_focus.contains_focused(window, cx);
        self.focused_api_field = if api_base_focused {
            Some(ApiField::Base)
        } else if api_model_focused {
            Some(ApiField::Model)
        } else if api_key_focused {
            Some(ApiField::Key)
        } else if local_model_focused {
            Some(ApiField::LocalModelPath)
        } else {
            None
        };

        let tab = self.state.active_tab;
        // 布局随窗口宽度降级（窄窗口下剪辑台改为上下堆叠），这里取本帧的视口宽度下发
        let viewport_w = f32::from(window.viewport_size().width);
        // 视口高度同样下发：监视器画面按真实宽高比等比内接需要「可用高度」这一维，
        // 只给宽度的话竖屏 / 方屏视频会溢出视口。
        let viewport_h = f32::from(window.viewport_size().height);

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(Theme::bg_app())
            .text_color(Theme::text_primary())
            .track_focus(&self.root_focus)
            // 点击空白外部失焦：点击非输入框时回收高亮与光标
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.any_input_focused(window, cx)
                        || this.is_text_focused
                        || this.subtitle_search_focused
                        || this.library_search_focused
                        || this.export_template_focused
                        || this.focused_api_field.is_some()
                        || this.focused_replace_field.is_some()
                    {
                        window.focus(&this.root_focus);
                        this.is_text_focused = false;
                        this.subtitle_search_focused = false;
                        this.library_search_focused = false;
                        this.export_template_focused = false;
                        this.focused_api_field = None;
                        this.focused_replace_field = None;
                        this.focused_style_field = None;
                        cx.notify();
                    }
                    if this.outline_dropdown_open
                        || this.bg_style_dropdown_open
                        || this.library_sort_dropdown_open
                    {
                        this.outline_dropdown_open = false;
                        this.bg_style_dropdown_open = false;
                        this.library_sort_dropdown_open = false;
                        cx.notify();
                    }
                }),
            )
            // 全局文件拖放接收层：GPUI 把「从资源管理器拖文件进来」包装成一次内部
            // drag/drop，drag value 类型为 `Arc<ExternalPaths>`，挂在根节点上即可
            // 全窗口任意位置放下都能导入。
            .on_drop::<ExternalPaths>(cx.listener(
                |this, paths: &ExternalPaths, _window, cx| {
                    let dropped: Vec<PathBuf> = paths.paths().to_vec();
                    tracing::info!("根容器监听到 ExternalPaths 拖放导入: {:?}", dropped);
                    this.handle_dropped_files(&dropped, cx);
                },
            ))
            .on_drop::<Arc<ExternalPaths>>(cx.listener(
                |this, paths: &Arc<ExternalPaths>, _window, cx| {
                    let dropped: Vec<PathBuf> = paths.paths().to_vec();
                    tracing::info!("根容器监听到 Arc<ExternalPaths> 拖放导入: {:?}", dropped);
                    this.handle_dropped_files(&dropped, cx);
                },
            ))
            // 全局快捷键（F-017）：键位表在 `shortcuts::bind_default_keys` 注册，
            // 这里只负责把动作接到具体的界面行为上。
            //
            // 除 Esc / Ctrl+K / F1 这几个「浮层自己的键」外，其余监听器一律先问
            // `overlay_blocks_keys()`：命令面板与快捷键速查面板打开时，它们是唯一的
            // 键盘入口。不拦这一刀的话，在浮层里敲字会顺带触发底下的全局动作——Ctrl+Z
            // 会静默撤销字幕编辑、Ctrl+F 会把焦点抢到搜索框上让浮层从此收不到按键、
            // Delete 会删掉一句被遮住看不清的字幕。
            .on_action(
                cx.listener(|this, _: &shortcuts::TogglePlayback, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    // 转写期间预览解码会与 ASR 抢 CPU/内存带宽，此时不允许用快捷键起播
                    if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                        return;
                    }
                    this.toggle_play_preview(cx);
                }),
            )
            // 裸 Space：仅当焦点不在任何输入框时才切播。焦点在输入框里时 `cx.propagate()`
            // 让按键继续冒泡到输入组件，用户才能正常打空格；其余守卫与 Ctrl+Space 逐条对齐。
            .on_action(
                cx.listener(|this, _: &shortcuts::TogglePlaybackBare, window, cx| {
                    if this.any_input_focused(window, cx) {
                        cx.propagate();
                        return;
                    }
                    if this.overlay_blocks_keys() {
                        cx.propagate();
                        return;
                    }
                    if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                        cx.propagate();
                        return;
                    }
                    this.toggle_play_preview(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::PrevSegment, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    this.jump_prev_segment(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::NextSegment, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    this.jump_next_segment(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::SeekBackward, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    if this.state.segments.is_empty() {
                        return;
                    }
                    this.halt_preview_playback();
                    let target = (this.state.current_time - 1.0).max(0.0);
                    this.state.seek_to(target);
                    this.trigger_extract_frame(cx);
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::SeekForward, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    if this.state.segments.is_empty() {
                        return;
                    }
                    this.halt_preview_playback();
                    let target = this.state.current_time + 1.0;
                    this.state.seek_to(target);
                    this.trigger_extract_frame(cx);
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::StartTranscription, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                        return;
                    }
                    if this.state.transcribe_file.is_none() {
                        return;
                    }
                    this.start_processing(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::CancelOrClose, window, cx| {
                    // 优先级：确认弹窗 → 命令面板 → 浮层 → 导出下拉 → 搜索框 → 终止任务。
                    // Esc 在没有可取消对象时不应有任何副作用。
                    //
                    // 确认弹窗排在最前，是因为它是最上层模态，且「清空批量队列」这类
                    // 待确认操作**本身**会终止正在跑的转写：少了这一刀，用户在这个确认框上
                    // 按 Esc 想关掉弹窗，弹窗纹丝不动、转写却被穿透终止了——弹窗上的
                    // 「取消」按钮反而成了唯一的正确出口。
                    if this.confirm_dialog.take().is_some() {
                        cx.notify();
                        return;
                    }
                    // 首次启动硬件引导框：它与确认框一样是**阻塞式模态**（遮罩 `occlude()`），
                    // 但此前只有「我有 GPU / 无 GPU」两个出口，Esc 完全关不掉——想先看看
                    // 界面再决定的用户被逼着表态。这里补一刀，与「稍后再说」按钮同义：
                    // 只把向导标记为完成，不动硬件档位、不跑检测。
                    //
                    // 焦点在输入框时不生效（同裸键那套约定）：那种场合 Esc 的语义是
                    // 「清空当前输入」，不该顺手关掉整个模态。
                    if !this.state.config.gpu.hardware_init_completed
                        && !this.any_input_focused(window, cx)
                    {
                        this.state.defer_hardware_setup();
                        cx.notify();
                        return;
                    }

                    // 命令面板紧随其后（渲染顺序上它也在确认框之下、其它浮层之上）。
                    // 为什么不是最前：确认框是**阻塞式**的，出现时用户除了「确认/取消」
                    // 没有别的出口；面板则是随时可关的轻量浮层，而且面板打开时遮罩把
                    // 底下界面全挡住、用户根本点不出新的确认框，两者实际不会同时在场。
                    // 万一并存（例如某条命令间接触发了确认框），先关掉视觉上更上层的
                    // 确认框才符合「Esc 关掉最上面那层」的直觉。
                    // 为什么必须在「终止任务」之前：面板打开时 Esc 的语义是「关面板」，
                    // 少了这一刀，用户在面板里按 Esc 会连面板带正在跑的转写一起终止
                    // （面板本身并没有「终止」这个含义，属于典型的穿透误伤）。
                    if this.command_palette.is_some() {
                        this.close_command_palette(cx);
                        return;
                    }
                    // 速查面板比命令面板「更薄」（纯展示、无输入焦点），关它的优先级
                    // 也低一格，排在命令面板之后、确认框之前。
                    if this.shortcuts_help_open {
                        this.close_shortcuts_help(cx);
                        return;
                    }
                    if this.completion_dialog.take().is_some()
                        || this.benchmark_dialog.take().is_some()
                    {
                        cx.notify();
                        return;
                    }
                    if this.is_export_dropdown_open
                        || this.outline_dropdown_open
                        || this.bg_style_dropdown_open
                        || this.library_sort_dropdown_open
                    {
                        this.is_export_dropdown_open = false;
                        this.outline_dropdown_open = false;
                        this.bg_style_dropdown_open = false;
                        this.library_sort_dropdown_open = false;
                        cx.notify();
                        return;
                    }
                    // 搜索框聚焦且有关键字时，Esc 先清空过滤词。
                    // 全局键位表把 escape 绑给了本动作，而 GPUI 的动作分发早于 `on_key_down`
                    // 且命中动作后默认停止冒泡，搜索框自己的 Esc 清空分支根本收不到事件。
                    // 不在这里补一刀，用户在搜索框里按 Esc 不但清不掉过滤词，
                    // 还会顺手把正在跑的转写给终止掉（界面提示「Esc 清空」与实际行为不符）。
                    if this.subtitle_search_focus.contains_focused(window, cx)
                        && !this.subtitle_search.is_empty()
                    {
                        this.subtitle_search.clear();
                        this.subtitle_search_cursor = 0;
                        cx.notify();
                        return;
                    }
                    this.request_cancel_processing(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::ExportSubtitle, _window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    this.perform_editor_export(cx);
                }),
            )
            .on_action(cx.listener(|this, _: &shortcuts::Undo, window, cx| {
                // 就地编辑未提交时放行：动作分发早于 `on_key_down`（见本文件 Esc 分支同型
                // 注释），否则输入框拿不到这次 Ctrl+Z，而撤销已经把快照回滚；随后回车会把
                // 基于旧内容的缓冲重新提交回去，一次撤销反过来被下一次回车抵消。
                if this.any_input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                if this.overlay_blocks_keys() {
                    return;
                }
                // 键位是全局的，但撤销对象只能是字幕编辑；栈空时什么都不做，
                // 免得在没有可退操作的页面上按 Ctrl+Z 反而触发别的副作用。
                if this.state.undo() {
                    // 撤销可能整体换掉片段表与选中项，让清单重新跟随选中项定位；
                    // 不清空这个标记的话，虚拟列表会停在旧行号上不滚动。
                    this.subtitle_list_followed_sel = None;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &shortcuts::Redo, window, cx| {
                // 与 Undo 同因：输入框持有焦点时把组合键让回去，避免快照回滚与未提交的
                // 输入缓冲各行其是。
                if this.any_input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                if this.overlay_blocks_keys() {
                    return;
                }
                if this.state.redo() {
                    this.subtitle_list_followed_sel = None;
                    cx.notify();
                }
            }))
            .on_action(
                cx.listener(|this, _: &shortcuts::FocusSubtitleSearch, window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    // 搜索框在剪辑台里，先切页再聚焦；若本帧尚未挂载，聚焦调用会静默失效，
                    // 用户再按一次即可，不会误伤其他状态。
                    this.state.active_tab = WorkspaceTab::Editor;
                    this.subtitle_panel = EditorSubtitlePanel::Translate;
                    this.subtitle_search_focused = true;
                    this.subtitle_search_cursor = this.subtitle_search.chars().count();
                    window.focus(&this.subtitle_search_focus);
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::OpenCommandPalette, window, cx| {
                    // 同一个键位 toggle：打开时把焦点交给面板搜索框，再按一次收回。
                    // 打开动作不会去动 `state.status`，因此不会与「终止转写」抢语义。
                    this.toggle_command_palette(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::OpenShortcutsHelp, _window, cx| {
                    // 纯展示面板：只切一个 bool，没有输入框焦点要交接。
                    this.toggle_shortcuts_help(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::OpenReplacePanel, window, cx| {
                    if this.overlay_blocks_keys() {
                        return;
                    }
                    // Ctrl+H：与 Ctrl+F 同一套「先切页、再展开/聚焦」的写法。
                    // 展开后把焦点直接给「查找」框——用户按这个键就是想马上打字，
                    // 再要求他点一下输入框是多余的一步。
                    this.state.active_tab = WorkspaceTab::Editor;
                    this.replace_panel_open = true;
                    this.replace_cursor = this.replace_find.chars().count();
                    window.focus(&this.replace_find_focus);
                    cx.notify();
                }),
            )
            // 裸 Delete：删除当前选中句（与编辑器「删除」按钮同源）。焦点在输入框时放行，
            // 否则在字幕文本框里按 Delete 会被动作系统吃掉，退格/删除就打不出来。
            .on_action(
                cx.listener(|this, _: &shortcuts::DeleteActiveSegment, window, cx| {
                    if this.any_input_focused(window, cx) {
                        cx.propagate();
                        return;
                    }
                    if this.overlay_blocks_keys() {
                        cx.propagate();
                        return;
                    }
                    // 流水线进行中删句只改内存：完成时整表替换会把这次删除无声吞掉，
                    // 用户却以为已经删了。与 Ctrl+Enter（StartTranscription）同型，
                    // 处理中一律不放行。
                    if should_ignore_delete_in_status(&this.state.status) {
                        return;
                    }
                    this.state.delete_selected_segment();
                    this.trigger_extract_frame(cx);
                    cx.notify();
                }),
            )
            // 1. 顶部自定义标题栏 (极简沉浸式，包含窗口拖拽与控制按钮)
            .child(self.render_titlebar(cx, window.is_maximized()))
            // 1.5 全局失败提示条：`ProcessStatus::Failed` 的产生点遍布导出 / 预览 /
            // 视频库等各处，而底部状态栏只在「语音转写」页存在。失败时就地在这一条
            // 跨页常驻的横幅上显示，否则用户在其它页面只会看到「点了没反应」。
            // 非失败态渲染空元素，不影响任何现有布局。
            .child(self.render_error_banner(cx))
            // 字幕写库失败：与转写错误分开显示，因为它要常驻到用户处理为止
            .child(self.render_db_error_banner(cx))
            // 设置落盘失败：与字幕写库失败同型，常驻到用户处理为止
            .child(self.render_config_error_banner(cx))
            // 1.6 一次性中性提示条：成功类反馈（如「已删除该记录」）与错误条同处一列，
            // 但配色中性——操作成功不该被误读成出错。两者同时存在时错误条在上。
            .child(self.render_crash_banner(cx))
            .child(self.render_notice_banner(cx))
            // 2. 工作区（导航已上移到顶部标题栏的横向标签条，这里不再占一列）。
            // 省下的 180px 左栏宽度全部还给内容区。
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(match tab {
                        WorkspaceTab::Editor => self
                            .render_editor_layout(viewport_w, viewport_h, window, cx)
                            .into_any_element(),
                        WorkspaceTab::Generate => {
                            self.render_generate_layout(cx).into_any_element()
                        }
                        WorkspaceTab::Library => self.render_library_layout(cx).into_any_element(),
                        WorkspaceTab::Performance => {
                            self.render_performance_layout(cx).into_any_element()
                        }
                    }),
            );

        // 注意：每一层都必须 `let root = ...` 回写。GPUI 的 `.child()` 返回**新的**
        // `Div`，原值不动；少了赋值这一步，弹窗就永远不会出现在渲染树里
        // （基准测试弹窗此前正是这样被静默丢掉的）。
        let root = if let Some(info) = self.completion_dialog.clone() {
            root.child(self.render_completion_dialog(info, cx))
        } else {
            root
        };

        let root = if let Some(info) = self.benchmark_dialog.clone() {
            root.child(self.render_benchmark_dialog(info, cx))
        } else {
            root
        };

        // 命令面板 (P1-A10)：排在确认框之下、其余浮层之上。
        // 它是「可随时关掉的轻量浮层」，而确认框是阻塞式模态——把面板放在确认框
        // 之前，两者万一并存时确认框仍然盖在最上层，与 Esc 的优先级顺序一致。
        let root = if self.command_palette.is_some() {
            root.child(self.render_command_palette(window, cx))
        } else {
            root
        };

        // 快捷键速查面板 (F1)：与命令面板同级的轻量浮层。两层不会同时出现
        // （见 `toggle_shortcuts_help` / `toggle_command_palette`），这里的先后顺序
        // 只是兜底——万一并存，命令面板仍然拿焦点。
        let root = if self.shortcuts_help_open {
            root.child(self.render_shortcuts_help(cx))
        } else {
            root
        };

        // 首次打开软件硬件配置向导（用户未完成初始选择时居中弹窗，提供「我有GPU」与「我没有GPU」选择）
        let root = if !self.state.config.gpu.hardware_init_completed {
            root.child(self.render_hardware_setup_modal(cx))
        } else {
            root
        };

        // 软件更新详情弹窗
        let root = if self.update_modal_open {
            root.child(self.render_update_modal(cx))
        } else {
            root
        };

        // 二次确认弹窗排在最后：它是阻塞式的，应盖在其它弹窗之上
        if let Some(info) = self.confirm_dialog.clone() {
            root.child(self.render_confirm_dialog(info, cx))
        } else {
            root
        }
    }
}

/// 字幕清单缓存是否仍可用：只要求每个下标都落在 `0..segment_count` 内，不要求覆盖全集，
/// 空下标集（含 `segment_count == 0`）同样视为可用。
///
/// 为什么不直接把调用点换回 [`indices_cover_segments`]：那个函数语义是「下标覆盖整个片段
/// 表」，而过滤态缓存的是全集的一个真子集，两者语义不同；即使某天实现恰好一致，也用一个
/// 独立命名的本地函数把「不越界即可复用」的语义钉在调用点上，避免将来任一侧收紧语义时
/// 静默改掉过滤缓存的复用行为（那会让播放期间逐帧重跑上千句的子串匹配）。
fn filter_cache_usable(indices: &[usize], segment_count: usize) -> bool {
    indices.iter().all(|&i| i < segment_count)
}

/// 字幕搜索关键字的归一化口径：先去掉首尾空白，再整体小写化。
///
/// 与 [`crate::subtitle::matched_indices`] 内部的写法**必须逐字一致**（trim 之后
/// 才 lower）：Unicode 小写化会改变字符数（`İ` U+0130 → `i` + U+0307），反过来
/// 先小写再 trim 在首尾空白上就会得出不同的串。两处口径只要漂移一个字符，
/// 索引扫描的结果就会与逐句匹配悄悄分家。
pub(crate) fn normalize_search_needle(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// 一句字幕的小写化搜索文本。
///
/// 原文与译文分成两栏而不是拼成一整串：旧判据是「原文命中 **或** 译文命中」，
/// 拼串会造出跨字段的假命中（原文尾部接上译文头部凑出一个本不存在的词）。
struct SearchHaystack {
    text: String,
    translation: Option<String>,
}

impl SearchHaystack {
    /// 与 `matched_indices` 的逐句判据同义：任一侧 `contains` 即命中。
    fn matches(&self, needle: &str) -> bool {
        self.text.contains(needle)
            || self
                .translation
                .as_deref()
                .is_some_and(|t| t.contains(needle))
    }
}

/// 字幕搜索的加速索引 + 增量收窄缓存（纯数据结构，可脱离 GPUI 直接单测）。
///
/// # 为什么值得单独做一个结构
///
/// [`crate::subtitle::matched_indices`] 每调用一次就对**每一句**跑一次
/// `to_lowercase()`：几万句时用户每敲一个字都要重新分配几万个字符串。这里把
/// 「小写化」摊到每次 `segments_revision` 变更后重建一次（一次 O(n) 分配），之后
/// 每个关键字的扫描只读预建好的字符串、逐句零分配；若新关键字是上一次关键字的
/// **延长**（逐字输入最常见的情形），还能直接在上一份结果上收窄，连全表都不必扫。
///
/// 结果与旧实现逐字等价，见 `mod tests` 里与旧逻辑内联副本的对照。
pub(crate) struct SubtitleSearchCache {
    /// 索引对应的修订号；与 `segment_count` 共同构成索引有效性依据
    revision: u64,
    /// 建索引时的片段数
    segment_count: usize,
    /// 逐句的小写化搜索文本（下标与 `segments` 一一对应）
    haystacks: Vec<SearchHaystack>,
    /// 上一次 commit 的**归一化**关键字（判断能否收窄）
    last_needle: String,
    /// `last_needle` 对应的修订号；`None` 表示还没有可用的上一次结果
    last_needle_revision: Option<u64>,
    /// 归一化关键字的复用缓冲：逐字输入时不重复申请
    needle_buf: String,
}

impl SubtitleSearchCache {
    pub(crate) fn new() -> Self {
        Self {
            // 与任何真实修订号都不相等的初值：首次 sync 必然重建
            revision: u64::MAX,
            segment_count: 0,
            haystacks: Vec::new(),
            last_needle: String::new(),
            last_needle_revision: None,
            needle_buf: String::new(),
        }
    }

    /// 按 `(segments_revision, 片段数)` 同步索引；真的重建了返回 `true`。
    ///
    /// 两个判据缺一不可：撤销/重做会整体换掉片段表并重排下标，只看片段数会拿着
    /// 旧文本扫新表；片段数也要看——万一某个调用点漏了递增修订号，至少表长变了
    /// 还能重建一次，不至于永久扫一份对不上的索引。
    pub(crate) fn sync(&mut self, segments: &[Segment], revision: u64) -> bool {
        if self.revision == revision && self.segment_count == segments.len() {
            return false;
        }
        self.revision = revision;
        self.segment_count = segments.len();
        self.haystacks = segments
            .iter()
            .map(|seg| SearchHaystack {
                text: seg.display_text().to_lowercase(),
                translation: seg.translation.as_deref().map(str::to_lowercase),
            })
            .collect();
        // 换表之后旧的「上一次结果」不再可信：禁止这次收窄，强制全量扫描一遍。
        self.last_needle_revision = None;
        true
    }

    /// 把原始关键字归一化进复用缓冲（口径见 [`normalize_search_needle`]）。
    pub(crate) fn set_needle(&mut self, raw: &str) {
        self.needle_buf.clear();
        self.needle_buf.push_str(&normalize_search_needle(raw));
    }

    /// 当前归一化关键字。
    pub(crate) fn needle(&self) -> &str {
        &self.needle_buf
    }

    /// 现在这份关键字能否在**上一次结果**上收窄。
    ///
    /// 只有「同修订号 + 新关键字以上一次关键字为前缀」两条同时成立才允许：关键字
    /// 延长后，命中集合必然是上一次命中集合的**子集**（`contains` 对前缀单调）。
    ///
    /// 前缀判定必须显式 `starts_with`，不能想当然：Unicode 里有上下文相关的小写
    /// 映射（希腊语词尾 Σ→ς），`"ΟΣ"` 延长成 `"ΟΣΑ"` 后归一化结果并不是简单前缀
    /// （`"ος"` vs `"οσα"`），这时必须老实全量重扫，否则会漏句。
    pub(crate) fn can_narrow(&self, revision: u64) -> bool {
        self.last_needle_revision == Some(revision)
            && self.needle_buf.starts_with(&self.last_needle)
    }

    /// 全量扫描（关键字为空即全部下标，与旧实现同义）。
    pub(crate) fn scan(&self) -> Vec<usize> {
        let needle = self.needle();
        if needle.is_empty() {
            return (0..self.segment_count).collect();
        }
        (0..self.haystacks.len())
            .filter(|&pos| self.haystacks[pos].matches(needle))
            .collect()
    }

    /// 在上一份结果上收窄（调用前必须先确认 [`Self::can_narrow`]）。
    ///
    /// 收窄只是少扫几句，**判定仍逐句重做**：`can_narrow` 只保证「新结果 ⊆ 旧结果」，
    /// 不保证旧结果里每一句都仍然命中。下标越界（片段表被别处换过）一律丢弃，
    /// 调用方还有一层「越界即退成不过滤」的兜底。
    pub(crate) fn narrow(&self, prev: &[usize]) -> Vec<usize> {
        let needle = self.needle();
        // 上一次关键字也是空 ⇒ 上一次结果就是全集，直接原样返回。
        if needle.is_empty() {
            return prev.to_vec();
        }
        prev.iter()
            .copied()
            .filter(|&pos| self.haystacks.get(pos).is_some_and(|h| h.matches(needle)))
            .collect()
    }

    /// 记录本次关键字与修订号，供下一次判断能否收窄。
    pub(crate) fn commit(&mut self, revision: u64) {
        self.last_needle.clear();
        self.last_needle.push_str(&self.needle_buf);
        self.last_needle_revision = Some(revision);
    }
}

impl Default for SubtitleSearchCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 规则集文件（与 `config.toml` 同目录）：独立 JSON，不进共享配置。
///
/// 为什么不塞进 `config.toml`：配置是**共享**文件，而规则集属于用户自己攒下来的
/// 编辑资产（要能整份拷给别人导入），且 `src/utils/config.rs` 不在本次改动范围内。
pub(crate) const REPLACE_RULES_FILE: &str = "replace_rules.json";

/// 一条「查找 → 替换」规则（**字面量**，语义完全按 [`crate::subtitle::edit`]）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReplaceRule {
    /// 查找文本（空串在应用时跳过：那是 `replace_all` 的硬禁区）
    pub(crate) find: String,
    /// 替换文本（允许空串＝删除）
    pub(crate) replace: String,
}

/// 一组有名字的替换规则：保存 / 复用 / 导入导出的单位。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReplaceRuleSet {
    /// 规则集名称（同时是去重键：同名即覆盖，不许空名）
    pub(crate) name: String,
    /// 按顺序应用的规则列表（前一条的输出是后一条的输入）
    pub(crate) rules: Vec<ReplaceRule>,
}

/// 规则集的应用范围（界面上的「全部句 / 当前选中句」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaceRuleScope {
    /// 全部字幕
    AllSegments,
    /// 仅当前选中的那一句（`AppState::selected_segment_index` 指向的句）
    SelectedSentence,
}

/// 一次规则集应用的结果汇总（多条 `ReplaceReport` 的合并版）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReplaceRuleReport {
    /// 真有命中的规则条数（空 `find` 与 `find == replace` 都不计）
    pub(crate) rules_hit: usize,
    /// 主文本里替换掉的出现次数（全部规则累计）
    pub(crate) text_hits: usize,
    /// 译文里替换掉的出现次数（全部规则累计）
    pub(crate) translation_hits: usize,
    /// 被改动的句序号（`Segment::index`，升序去重，口径同 `ReplaceReport::changed`）
    pub(crate) changed: Vec<usize>,
}

impl ReplaceRuleReport {
    /// 主文本与译文命中次数之和。
    pub(crate) fn total_hits(&self) -> usize {
        self.text_hits + self.translation_hits
    }

    /// 一处都没改。调用方据此决定「不压撤销点、不置脏、不落库」——
    /// 压一个什么都没做的撤销点，用户按 Ctrl+Z 只会觉得撤销坏了。
    pub(crate) fn is_empty(&self) -> bool {
        self.total_hits() == 0
    }

    /// 并入一条规则的结果。
    fn absorb(&mut self, report: crate::subtitle::edit::ReplaceReport) {
        if report.is_empty() {
            return;
        }
        self.rules_hit += 1;
        self.text_hits += report.text_hits;
        self.translation_hits += report.translation_hits;
        self.changed.extend(report.changed);
    }
}

/// 规则集文件路径：与 `config.toml` 同目录（项目根）。
///
/// 用 `AppConfig::app_root_dir()` 而不是 `current_dir()`：后者在「从任意目录启动」
/// 时会把文件散落到别处，用户下次启动就找不回自己的规则集了。
pub(crate) fn replace_rules_path() -> PathBuf {
    crate::utils::AppConfig::app_root_dir().join(REPLACE_RULES_FILE)
}

/// 导出为**扁平** JSON（与 `AppConfig::export_custom_presets_json` 同一风格：
/// 顶层数组、每条自包含），便于整份拷给别的机器导入。
pub(crate) fn replace_rule_sets_to_json(sets: &[ReplaceRuleSet]) -> String {
    // 序列化 `&[ReplaceRuleSet]` 本身不会失败；兜底只为让签名保持简单。
    serde_json::to_string_pretty(sets).unwrap_or_else(|_| "[]".to_string())
}

/// 解析磁盘上 / 导入进来的规则集 JSON。
///
/// 逐条校验、能救则救（与 `AppConfig::import_custom_presets_json` 同一口径）：
/// - 不是合法 JSON、或顶层不是数组 → `Err`（提示用户看文件来源）；
/// - 单条名字为空 → 跳过这一条，不整批失败；
/// - 缺 `rules` / 缺 `replace` → 按空值补齐（空规则集、空替换＝删除）；
/// - 单条规则缺 `find`（或不是字符串）→ 跳过这条规则；
/// - 未知字段忽略。
pub(crate) fn replace_rule_sets_from_json(raw: &str) -> Result<Vec<ReplaceRuleSet>, String> {
    let parsed: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| format!("规则集文件解析失败：{e}。请使用「导出」得到的 JSON 文件。"))?;
    let array = parsed
        .as_array()
        .ok_or_else(|| "规则集文件格式不对：顶层应该是 JSON 数组（[]）。".to_string())?;
    let mut sets = Vec::new();
    for entry in array {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }
        let rules = entry
            .get("rules")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|rule| {
                        // `find` 是规则的唯一必需字段：没有它就无从匹配。
                        let find = rule.get("find").and_then(|v| v.as_str())?.to_string();
                        Some(ReplaceRule {
                            find,
                            replace: rule
                                .get("replace")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        sets.push(ReplaceRuleSet { name, rules });
    }
    Ok(sets)
}

/// 从指定路径读取规则集；文件不存在视为「还没有规则集」（`Ok(空)`，不是错误）。
pub(crate) fn load_replace_rule_sets(
    path: &std::path::Path,
) -> Result<Vec<ReplaceRuleSet>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("规则集文件读取失败（{}）：{e}", path.display())),
    };
    replace_rule_sets_from_json(&raw)
}

/// 读取默认路径（项目根下的 [`REPLACE_RULES_FILE`]）。
pub(crate) fn load_replace_rule_sets_default() -> Result<Vec<ReplaceRuleSet>, String> {
    load_replace_rule_sets(&replace_rules_path())
}

/// 写回指定路径；父目录不存在会先建出来。
///
/// **失败必须原样交给调用方**：规则集是用户攒下来的编辑资产，静默吞掉写失败会让
/// 用户以为「保存成功了」，下次启动才发现全没写进去。
pub(crate) fn save_replace_rule_sets(
    path: &std::path::Path,
    sets: &[ReplaceRuleSet],
) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("规则集目录创建失败（{}）：{e}", dir.display()))?;
        }
    }
    std::fs::write(path, replace_rule_sets_to_json(sets))
        .map_err(|e| format!("规则集写入失败（{}）：{e}", path.display()))
}

/// 写回默认路径（项目根下的 [`REPLACE_RULES_FILE`]）。
pub(crate) fn save_replace_rule_sets_default(sets: &[ReplaceRuleSet]) -> Result<(), String> {
    save_replace_rule_sets(&replace_rules_path(), sets)
}

/// 按名字查规则集（增删改查里的「查」）。
pub(crate) fn find_replace_rule_set<'a>(
    sets: &'a [ReplaceRuleSet],
    name: &str,
) -> Option<&'a ReplaceRuleSet> {
    sets.iter().find(|s| s.name == name)
}

/// 新增或按名覆盖（增 / 改），返回该规则集在表里的下标。
///
/// 名字是去重键：同名覆盖而不是追加，否则每次导入同一份文件都会多出一份副本。
/// 空名直接报错——那会让「按名选中 / 覆盖 / 删除」全部失去意义。
pub(crate) fn upsert_replace_rule_set(
    sets: &mut Vec<ReplaceRuleSet>,
    set: ReplaceRuleSet,
) -> Result<usize, String> {
    let name = set.name.trim().to_string();
    if name.is_empty() {
        return Err("规则集名称不能为空".to_string());
    }
    let set = ReplaceRuleSet { name, ..set };
    match sets.iter().position(|s| s.name == set.name) {
        Some(pos) => {
            sets[pos] = set;
            Ok(pos)
        }
        None => {
            sets.push(set);
            Ok(sets.len() - 1)
        }
    }
}

/// 按名字删除（删）；返回是否真的删掉了。
pub(crate) fn remove_replace_rule_set(sets: &mut Vec<ReplaceRuleSet>, name: &str) -> bool {
    let Some(pos) = sets.iter().position(|s| s.name == name) else {
        return false;
    };
    sets.remove(pos);
    true
}

/// 把一整套规则**按顺序**应用到全部字幕。
///
/// 顺序有语义：前一条规则的输出就是后一条规则的输入（`a→b, b→c` 与反序结果不同）。
/// 每条规则都调 [`crate::subtitle::edit::replace_all`]——大小写口径、译文保护、
/// `display_text` 落点、空 `find` / `find == replace` 的禁区全部沿用那一份实现，
/// 这里一行判据都不复制。
pub(crate) fn apply_replace_rule_set(
    segments: &mut [Segment],
    set: &ReplaceRuleSet,
    options: &crate::subtitle::edit::ReplaceOptions,
) -> ReplaceRuleReport {
    let mut report = ReplaceRuleReport::default();
    for rule in &set.rules {
        if rule.find.is_empty() {
            continue;
        }
        report.absorb(crate::subtitle::edit::replace_all(
            segments,
            &rule.find,
            &rule.replace,
            options,
        ));
    }
    report.changed.sort_unstable();
    report.changed.dedup();
    report
}

/// 只把规则应用到**指定位置**的句子（「应用到选中句」）。
///
/// 逐句、逐规则调用同一份 `replace_all`（单元素切片）：这样「应用到选中句」与
/// 「应用到全部」在每一句上的结果逐字一致，不另写一套匹配逻辑。越界下标直接跳过
/// （选中项可能已被前一次编辑删掉）。
pub(crate) fn apply_replace_rule_set_to_indices(
    segments: &mut [Segment],
    indices: &[usize],
    set: &ReplaceRuleSet,
    options: &crate::subtitle::edit::ReplaceOptions,
) -> ReplaceRuleReport {
    let mut report = ReplaceRuleReport::default();
    for &pos in indices {
        if pos >= segments.len() {
            continue;
        }
        for rule in &set.rules {
            if rule.find.is_empty() {
                continue;
            }
            report.absorb(crate::subtitle::edit::replace_all(
                std::slice::from_mut(&mut segments[pos]),
                &rule.find,
                &rule.replace,
                options,
            ));
        }
    }
    report.changed.sort_unstable();
    report.changed.dedup();
    report
}

/// 结果列表选中行移动 `delta` 格后的新位置（纯函数，便于单测）。
///
/// 修的是旧写法里那条悬空判据：`count == 0` 时 `count - 1` 会算术回绕成
/// `usize::MAX`，此前全靠调用点的 `if count == 0` 提前返回才没踩到。把守卫收进
/// 函数本体后，任何调用路径都不会碰到回绕，语义也一并钉死：
/// - 空结果集恒回 0；
/// - 两端撞住即停、不回绕（`↑` 在第一行 / `↓` 在最后一行都是空操作）；
/// - 脏的 `selected`（过滤后列表缩短）先夹回有效行再移动；
/// - 走饱和运算，极端 `delta` 在 debug 构建里也不会溢出 panic。
fn move_palette_selection_row(selected: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let last = count - 1;
    let current = selected.min(last);
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(last)
    }
}

/// 转写 / 翻译流水线进行中时，裸 Delete（`shortcuts::DeleteActiveSegment`）是否必须被忽略。
///
/// 流水线完成会用整表替换 `state.segments`，处理中删除只改内存、随后被无声覆盖，
/// 用户却以为已经删掉。判据抽成纯函数以便在无 GUI 的单测里钉死（见 `mod tests`）。
fn should_ignore_delete_in_status(status: &ProcessStatus) -> bool {
    matches!(status, ProcessStatus::Processing { .. })
}

#[cfg(test)]
mod tests {
    use super::{
        apply_replace_rule_set, apply_replace_rule_set_to_indices, build_derived_snapshot,
        build_timeline_index, cached_derived_snapshot, filter_cache_usable, filter_commands,
        find_replace_rule_set, glossary_fingerprint, glossary_violations_cached,
        has_any_translation, has_copied_translations, has_distinct_translation,
        insert_printable_key, load_replace_rule_sets, move_palette_selection_row,
        normalize_search_needle, remove_replace_rule_set, replace_rule_sets_from_json,
        replace_rule_sets_to_json, save_replace_rule_sets, should_ignore_delete_in_status,
        timeline_visible_positions, translated_other_lang_count, upsert_replace_rule_set,
        DerivedSnapshot, DerivedSnapshotKey, GlossaryBadCache, GlossaryFingerprint, PaletteCommand,
        PaletteContext, ReplaceRule, ReplaceRuleSet, SubtitleSearchCache, PALETTE_ACTIONS,
    };
    use crate::app::state::WorkspaceTab;
    /// 崩溃横幅那句话要同时说清「发生了什么」与「去哪拿堆栈」。
    ///
    /// 回归护栏：这里以前写「导出诊断可一并附上日志」，而诊断报告里并没有日志正文
    /// ——用户照着去导出，拿到的报告缺了最关键的那段堆栈。
    #[test]
    fn crash_notice_points_at_logs_and_the_copy_button() {
        let record = crate::utils::crash_report::CrashRecord {
            at: "2026-10-13 10:00:00".to_string(),
            message: "index out of bounds".to_string(),
            backtrace: "0: voice2word::foo".to_string(),
            source: std::path::PathBuf::from("logs/voice2word_20261013_100000.txt"),
        };
        let text = super::MainWindow::crash_notice_text(&record);
        assert!(text.contains("index out of bounds"), "要有崩溃原因：{text}");
        assert!(text.contains("logs"), "要指出堆栈在哪：{text}");
        assert!(text.contains("复制报告"), "要指出有现成的复制入口：{text}");
        assert!(
            !text.contains("导出诊断可一并附上日志"),
            "不能再说诊断导出里含日志（它不含）：{text}"
        );
    }
    use crate::subtitle::edit::ReplaceOptions;
    use crate::subtitle::Segment;
    use std::cell::Cell;
    use std::sync::Arc;

    /// 不过滤状态下缓存的是 `0..len` 全集：只要没越界就复用（播放期间逐帧重绘
    /// 时零成本命中）。
    #[test]
    fn full_index_set_is_reusable() {
        assert!(filter_cache_usable(&[0, 1, 2, 3], 4));
        assert!(filter_cache_usable(&[], 0));
    }

    /// 过滤态下缓存的是全集的一个真子集，同样必须能复用——否则搜索关键字还在时
    /// 播放会逐帧重跑上千句的子串匹配（这正是缓存要避免的开销）。
    #[test]
    fn filtered_subset_is_reusable() {
        assert!(filter_cache_usable(&[1, 3], 4));
        assert!(filter_cache_usable(&[2], 4));
    }

    /// 片段表在别处被换短后，旧下标越界即判失效，避免虚拟列表量不到行高而塌成空白。
    #[test]
    fn stale_indices_are_rejected() {
        assert!(!filter_cache_usable(&[0, 1, 2, 3], 2));
        assert!(!filter_cache_usable(&[5], 4));
    }

    /// 命令面板（P1-A10）：空查询显示全部命令——用户多半是先按 Ctrl+K 看一眼
    /// 有哪些命令，再决定搜什么。
    #[test]
    fn empty_query_lists_every_command() {
        assert_eq!(filter_commands("").len(), PALETTE_ACTIONS.len());
        // 纯空白等价于空查询：面板里多敲了几个空格不该把列表清空
        assert_eq!(filter_commands("   ").len(), PALETTE_ACTIONS.len());
    }

    /// 大小写不敏感：`EXPORT` / `export` / `Export` 都必须命中「导出」，
    /// 英文别名只认精确大小写的话，按住 Shift 打词的人会搜不到任何东西。
    #[test]
    fn query_is_case_insensitive() {
        let lower = filter_commands("export");
        assert!(!lower.is_empty());
        assert_eq!(filter_commands("EXPORT"), lower);
        assert_eq!(filter_commands("eXpOrT"), lower);
    }

    /// 中文名与英文别名都在匹配范围内：中文用户输「导出」、英文习惯的用户输
    /// `export`，两条路都得通。
    #[test]
    fn chinese_and_english_aliases_both_match() {
        let zh = filter_commands("导出");
        let en = filter_commands("export");
        assert!(!zh.is_empty(), "中文名「导出」应当命中");
        assert!(!en.is_empty(), "英文别名 export 应当命中");
        // 同一条命令的两个别名：两个查询都得包含「导出（当前格式）」
        let export_index = PALETTE_ACTIONS
            .iter()
            .position(|spec| spec.zh == "导出（当前格式）")
            .expect("命令集里必须有导出");
        assert!(zh.contains(&export_index));
        assert!(en.contains(&export_index));
    }

    /// 无匹配返回空列表（渲染层据此显示「没有匹配的命令」，Enter 也因此不会
    /// 误触任何命令）。
    #[test]
    fn unmatched_query_returns_nothing() {
        assert!(filter_commands("zzz-not-a-command-zzz").is_empty());
        assert!(filter_commands("转写一下这个根本不存在的命令").is_empty());
    }

    /// 返回的是 `PALETTE_ACTIONS` 的下标，且严格保持命令集顺序：
    /// 面板的选中行按该序列索引，顺序错乱会让 Enter 执行到另一条命令。
    #[test]
    fn filter_preserves_catalog_order_and_indices() {
        let all = filter_commands("");
        assert!(all.windows(2).all(|w| w[0] < w[1]), "下标必须递增");
        assert!(all.iter().all(|&i| i < PALETTE_ACTIONS.len()));

        let undo = filter_commands("undo");
        let redo = filter_commands("redo");
        let undo_index = PALETTE_ACTIONS
            .iter()
            .position(|spec| spec.zh == "撤销")
            .unwrap();
        let redo_index = PALETTE_ACTIONS
            .iter()
            .position(|spec| spec.zh == "重做")
            .unwrap();
        assert!(undo.contains(&undo_index));
        assert!(redo.contains(&redo_index));
        // 「undo」不该顺带把「重做」也匹配进来（别名没有互相包含）
        assert!(!undo.iter().any(|&i| PALETTE_ACTIONS[i].zh == "重做"));
    }

    /// 命令集本身的自洽性：英文别名一律小写（匹配前统一 `to_lowercase`，
    /// 混入大写会让 `en` 列在面板副标题里显示得不整齐），中文名不重复。
    #[test]
    fn catalog_aliases_are_wellformed() {
        let mut seen = std::collections::HashSet::new();
        for spec in PALETTE_ACTIONS {
            assert_eq!(
                spec.en,
                spec.en.to_lowercase(),
                "别名必须全小写: {}",
                spec.en
            );
            assert!(seen.insert(spec.zh), "中文名重复: {}", spec.zh);
        }
    }

    /// 一份「空工程」的判据快照：没有文件、没有字幕、没有选中。
    fn empty_context() -> PaletteContext {
        PaletteContext {
            active_tab: WorkspaceTab::Editor,
            processing: false,
            has_source_file: false,
            has_segments: false,
            translating: false,
            can_undo: false,
            can_redo: false,
            has_selection: false,
            selection_splittable: false,
            selection_has_next: false,
            selection_has_translation: false,
            has_replace_rule_set: false,
            has_media_file: false,
            clip_export_idle: true,
        }
    }

    /// 取某条命令在注册表里自带的可用性判据。
    fn available(command: PaletteCommand, ctx: &PaletteContext) -> bool {
        (PALETTE_ACTIONS
            .iter()
            .find(|spec| spec.command == command)
            .expect("命令必须在注册表里")
            .available)(ctx)
    }

    /// 注册表是唯一事实来源：每条命令都必须**恰好登记一次**，不多不少。
    #[test]
    fn every_palette_command_variant_is_registered() {
        let all = [
            PaletteCommand::OpenFile,
            PaletteCommand::StartTranscription,
            PaletteCommand::CancelTranscription,
            PaletteCommand::ExportCurrentFormat,
            PaletteCommand::StartTranslation,
            PaletteCommand::CancelTranslation,
            PaletteCommand::SwitchToEditor,
            PaletteCommand::SwitchToGenerate,
            PaletteCommand::SwitchToLibrary,
            PaletteCommand::SwitchToPerformance,
            PaletteCommand::Undo,
            PaletteCommand::Redo,
            PaletteCommand::OpenReplace,
            PaletteCommand::ExportQcSheet,
            PaletteCommand::ExportDiagnostics,
            PaletteCommand::SplitSegment,
            PaletteCommand::MergeWithNext,
            PaletteCommand::ClearTranslation,
            PaletteCommand::ApplyCleanup,
            PaletteCommand::ApplyReplaceRuleSet,
            PaletteCommand::ExportSelectedClip,
            PaletteCommand::ExportSelectedClipAudio,
            PaletteCommand::ShiftTrackForward,
            PaletteCommand::ShiftTrackBackward,
            PaletteCommand::FitTrackToMedia,
            PaletteCommand::ApplyMinGap,
        ];
        for command in all {
            let count = PALETTE_ACTIONS
                .iter()
                .filter(|spec| spec.command == command)
                .count();
            assert_eq!(count, 1, "命令 {command:?} 注册了 {count} 次");
        }
        assert_eq!(PALETTE_ACTIONS.len(), all.len(), "注册表里有未列出的命令");
    }

    /// 面板清单必须把剪辑台那批动作纳入（按中文名断言）。
    #[test]
    fn palette_includes_editor_station_actions() {
        let names = [
            "拆分当前句",
            "合并下句",
            "清除当前句译文",
            "应用清理规则",
            "应用替换规则集（全部句）",
            "导出当前句片段",
            "导出当前句音频",
            "整轨后移 1 秒",
            "整轨前移 1 秒",
            "铺满媒体总时长",
            "应用句间最小间隔",
        ];
        for name in names {
            assert!(
                PALETTE_ACTIONS.iter().any(|spec| spec.zh == name),
                "面板缺少剪辑台命令「{name}」"
            );
        }
    }

    /// 拆分 / 合并的可用性边界：没有选中、选中句太短、已是末句都不能选。
    #[test]
    fn split_and_merge_availability_boundaries() {
        let mut ctx = empty_context();
        assert!(
            !available(PaletteCommand::SplitSegment, &ctx),
            "无选中不可拆"
        );
        assert!(
            !available(PaletteCommand::MergeWithNext, &ctx),
            "无选中不可合并"
        );

        ctx.has_segments = true;
        ctx.has_selection = true;
        ctx.selection_splittable = false;
        assert!(
            !available(PaletteCommand::SplitSegment, &ctx),
            "单字句不可拆"
        );
        assert!(
            !available(PaletteCommand::MergeWithNext, &ctx),
            "末句不可合并"
        );

        ctx.selection_splittable = true;
        ctx.selection_has_next = true;
        assert!(available(PaletteCommand::SplitSegment, &ctx));
        assert!(available(PaletteCommand::MergeWithNext, &ctx));
    }

    /// 剪辑台其它命令各有前置条件：清理、替换、片段导出、重定时分别验证边界。
    #[test]
    fn editor_action_availability_requires_their_preconditions() {
        let mut ctx = empty_context();
        assert!(!available(PaletteCommand::ApplyCleanup, &ctx));
        assert!(!available(PaletteCommand::ApplyReplaceRuleSet, &ctx));
        assert!(!available(PaletteCommand::ExportSelectedClip, &ctx));
        assert!(!available(PaletteCommand::ApplyMinGap, &ctx));

        ctx.has_segments = true;
        assert!(
            available(PaletteCommand::ApplyCleanup, &ctx),
            "有字幕就能清理"
        );
        assert!(
            !available(PaletteCommand::ApplyReplaceRuleSet, &ctx),
            "没选规则集不能替换"
        );

        ctx.has_replace_rule_set = true;
        assert!(available(PaletteCommand::ApplyReplaceRuleSet, &ctx));

        // 片段导出还要「有选中 + 有源文件 + 导出空闲」
        ctx.has_selection = true;
        assert!(
            !available(PaletteCommand::ExportSelectedClip, &ctx),
            "没有源文件不能切片"
        );
        ctx.has_media_file = true;
        assert!(available(PaletteCommand::ExportSelectedClip, &ctx));
        ctx.clip_export_idle = false;
        assert!(
            !available(PaletteCommand::ExportSelectedClip, &ctx),
            "正在导出时置灰"
        );

        // 重定时只看有没有字幕
        assert!(available(PaletteCommand::ShiftTrackForward, &ctx));
        assert!(available(PaletteCommand::FitTrackToMedia, &ctx));
    }

    /// 不可用只是**置灰**，不从列表里消失：命令仍在全集里、仍能被搜索命中。
    #[test]
    fn unavailable_commands_stay_visible_in_the_list() {
        let split_index = PALETTE_ACTIONS
            .iter()
            .position(|spec| spec.command == PaletteCommand::SplitSegment)
            .expect("拆分命令必须登记");
        assert!(
            filter_commands("").contains(&split_index),
            "不可用的命令也必须留在清单里（置灰而非隐藏）"
        );
        assert!(
            filter_commands("拆分").contains(&split_index),
            "不可用的命令同样要能被搜到"
        );
    }

    /// 导出格式的持久化字符串必须**可往返**：`as_str` 写进 config.toml，
    /// 重启时 `from_config_str` 读回来必须还是同一档。
    ///
    /// 这是回归护栏——`editor_export_format` 此前从不落盘，每次启动都跳回默认的
    /// 「剪映草稿」。往返测试能拦住两类错误：新增格式时忘了加 `as_str` 分支
    /// （会撞进 `_ =>` 而悄悄落到默认档），以及 `from_config_str` 的别名表与
    /// `as_str` 取值不一致。
    #[test]
    fn export_format_config_string_roundtrips() {
        for fmt in super::EditorExportFormat::all() {
            let s = fmt.as_str();
            assert!(!s.is_empty());
            assert_eq!(
                super::EditorExportFormat::from_config_str(s),
                *fmt,
                "导出格式 {s} 未能往返"
            );
            // 大小写与首尾空白都要容错（config.toml 是用户可手改的文件）
            assert_eq!(
                super::EditorExportFormat::from_config_str(&format!("  {}  ", s.to_uppercase())),
                *fmt,
                "导出格式 {s} 应容忍大小写与空白"
            );
        }
        // 未知 / 空串安静回落到默认，而不是 panic 或让界面构造失败
        assert_eq!(
            super::EditorExportFormat::from_config_str("no-such-format"),
            super::EditorExportFormat::default()
        );
        assert_eq!(
            super::EditorExportFormat::from_config_str(""),
            super::EditorExportFormat::default()
        );
        // 持久化字符串必须两两不同，否则两档会互相覆盖
        let mut seen = std::collections::HashSet::new();
        for fmt in super::EditorExportFormat::all() {
            assert!(
                seen.insert(fmt.as_str()),
                "持久化字符串重复: {}",
                fmt.as_str()
            );
        }
    }

    /// 低置信判据：`None` **绝不**算低置信，有值时才比阈值；恰好等于阈值不算
    /// （与 `plan_rescue_spans` / `quality_report` 的 `c < threshold` 同口径）。
    ///
    /// 这条锁住剪辑台行首那个红点的显示规则：一旦有人把 `is_some_and` 写成
    /// `map_or(true, ...)` 之类的「None 也算」，SenseVoice 用户的字幕清单会整片标红。
    #[test]
    fn low_confidence_only_flags_scored_segments_below_threshold() {
        use crate::subtitle::Segment;
        let mut scored_low = Segment::new(1, 0.0, 1.0, "低");
        scored_low.confidence = Some(-0.9);
        let mut scored_ok = Segment::new(2, 1.0, 2.0, "好");
        scored_ok.confidence = Some(-0.05);
        let mut on_threshold = Segment::new(3, 2.0, 3.0, "等");
        on_threshold.confidence = Some(-0.35);
        let unscored = Segment::new(4, 3.0, 4.0, "缺"); // confidence 默认 None
        let segs = vec![scored_low, scored_ok, on_threshold, unscored];

        // 默认阈值 -0.35：只有 -0.9 命中；-0.35 恰好等于阈值，不算
        assert_eq!(
            super::low_confidence_indices(&segs, -0.35),
            vec![1],
            "只有严格低于阈值的句子才标低置信"
        );
        // None 永不命中（SenseVoice 全程），阈值放到 +10 也依然是 None 不命中
        assert_eq!(
            super::low_confidence_indices(&segs, 10.0),
            vec![1, 2, 3],
            "None 不应因为阈值放宽而被算进来"
        );
        // 空表不出错
        assert!(super::low_confidence_indices(&[], 0.0).is_empty());
    }

    /// 裸键（Space / Delete）门控的接线护栏。
    ///
    /// `any_input_focused` 聚合所有「已真实接线」的输入焦点句柄，只有它返回 `false`
    /// 时裸键动作才生效。此前漏掉了视频库的「筛选关键字」输入框（`library_search_focus`），
    /// 于是用户在库里敲空格打不出空格、敲 Delete 反而删掉当前句。
    ///
    /// 该函数依赖 `Window` / `App`，纯单测里无法构造焦点，因此这里退化为对源码文本的
    /// 结构断言：逐个确认每个输入焦点句柄都被 `contains_focused` 聚合进去。将来新增输入框
    /// 却忘了接进 `any_input_focused`，这条测试会立刻报出漏掉的名字。
    #[test]
    fn any_input_focused_aggregates_every_text_input() {
        // 归一化换行：CI（windows-latest + autocrlf=true）检出的源文件是 CRLF，
        // 而下面用 "\n" 字面量定位函数体与闭合花括号，不归一化会永远找不到。
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let start = src
            .find("fn any_input_focused")
            .expect("any_input_focused 必须存在");
        let body = &src[start..];
        let end = body
            .find("\n    }\n")
            .expect("any_input_focused 必须有闭合花括号");
        let body = &body[..end];

        // 所有「已真实接线」的输入焦点句柄；漏掉任何一个，裸键都会在对应输入框里误触发。
        for field in [
            "text_focus",
            "subtitle_search_focus",
            "replace_find_focus",
            "replace_with_focus",
            "export_template_focus",
            "style_field_focus",
            "inline_edit_focus",
            "api_base_focus",
            "api_model_focus",
            "api_key_focus",
            "local_model_focus",
            "library_search_focus",
        ] {
            assert!(
                body.contains(&format!("self.{field}.contains_focused")),
                "any_input_focused 漏掉输入焦点句柄: {field}（裸键 Space/Delete 会在该输入框里被误触发）"
            );
        }
    }

    /// 裸 Delete 在流水线进行中必须被忽略：删除只改内存，完成时的整表替换会把它
    /// 无声吞掉，用户却以为已经删了。判据是纯函数，四种状态逐一钉死。
    #[test]
    fn delete_ignored_only_while_processing() {
        use crate::app::state::ProcessStatus;

        assert!(should_ignore_delete_in_status(&ProcessStatus::Processing {
            stage: "转写".to_string(),
            progress: 0.5,
            detail: String::new(),
        }));
        assert!(!should_ignore_delete_in_status(&ProcessStatus::Idle));
        assert!(!should_ignore_delete_in_status(&ProcessStatus::Completed));
        assert!(!should_ignore_delete_in_status(&ProcessStatus::Failed(
            "失败".to_string(),
        )));
    }

    /// 撤销 / 重做的输入焦点门控接线护栏。
    ///
    /// 处理函数依赖 `Window` / `App`，纯单测里构造不出焦点，退化为源码文本断言
    /// （与 `any_input_focused_aggregates_every_text_input` 同法）：两个动作的处理器体
    /// 里都必须先判 `any_input_focused` 并 `cx.propagate()`，否则就地编辑未提交时按
    /// Ctrl+Z 会回滚快照，随后回车把基于旧内容的缓冲重新提交，撤销被下一次回车抵消。
    #[test]
    fn undo_and_redo_propagate_when_input_focused() {
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        for action in ["Undo", "Redo"] {
            // 锚在监听器签名上：文件别处的注释也出现过裸 `shortcuts::Undo`，
            // 只按动作名定位会命中注释而不是处理器。
            let marker = format!("&shortcuts::{action}, ");
            let start = src
                .find(&marker)
                .unwrap_or_else(|| panic!("{action} 动作处理器必须存在"));
            let body = &src[start..];
            let end = body
                .find(".on_action")
                .unwrap_or_else(|| panic!("{action} 处理器之后必须还有下一个 on_action"));
            let body = &body[..end];
            assert!(
                body.contains("this.any_input_focused(window, cx)"),
                "{action} 缺少输入焦点守卫（监听器参数名须为 window）"
            );
            assert!(
                body.contains("cx.propagate()"),
                "{action} 命中输入焦点时未 cx.propagate()，输入框收不到按键"
            );
        }
    }

    /// 裸 Delete 的处理中守卫必须真的接在处理器上，而不是只定义了纯函数。
    #[test]
    fn delete_handler_uses_processing_guard() {
        let src = include_str!("mod.rs").replace("\r\n", "\n");
        let start = src
            .find("&shortcuts::DeleteActiveSegment, ")
            .expect("DeleteActiveSegment 动作处理器必须存在");
        let body = &src[start..];
        let end = body
            .find("this.state.delete_selected_segment()")
            .expect("处理器必须调用 delete_selected_segment");
        assert!(
            body[..end].contains("should_ignore_delete_in_status(&this.state.status)"),
            "DeleteActiveSegment 缺少处理中守卫；处理中删除会被流水线完成时的整表替换吞掉"
        );
    }

    /// 回归（P1）：结果集为空时移动选中行不得 panic——旧实现 `count - 1` 在
    /// `count == 0` 时回绕成 `usize::MAX`，判据本身是悬空的；这里把纯函数的
    /// 语义钉死：空集恒 0，越界夹住、两端不回绕，极端 `delta` 也不溢出。
    #[test]
    fn move_palette_selection_with_empty_result_set_does_not_panic() {
        // 空结果集：任何 delta 都回到 0，不发生算术回绕
        assert_eq!(move_palette_selection_row(0, 1, 0), 0);
        assert_eq!(move_palette_selection_row(0, -1, 0), 0);
        assert_eq!(move_palette_selection_row(9, isize::MAX, 0), 0);
        assert_eq!(move_palette_selection_row(9, isize::MIN, 0), 0);

        // 非空：两端撞住即停，不回绕
        assert_eq!(move_palette_selection_row(0, -1, 3), 0);
        assert_eq!(move_palette_selection_row(2, 1, 3), 2);
        assert_eq!(move_palette_selection_row(1, -1, 3), 0);
        assert_eq!(move_palette_selection_row(1, 1, 3), 2);

        // 选中行越界（过滤后列表缩短）：先夹回有效行再移动
        assert_eq!(move_palette_selection_row(99, 0, 3), 2);
        assert_eq!(move_palette_selection_row(99, 1, 3), 2);

        // 极端 delta 也不 panic（饱和运算，debug 构建同样安全）
        assert_eq!(move_palette_selection_row(1, isize::MAX, 3), 2);
        assert_eq!(move_palette_selection_row(1, isize::MIN, 3), 0);
    }

    /// 回归（P2）：可打印单字符按键必须一步完成「判据 + 插入 + 光标前移」，
    /// 且只认**恰好一个**非控制字符——多字符键名（`space` / `f5`）与控制字符
    /// 都不得进入缓冲区。
    #[test]
    fn apply_line_edit_single_char_key_inserts_and_advances_cursor() {
        // 插到光标处，光标前移一格
        let mut buf = String::from("ab");
        let mut cursor = 1usize;
        assert!(insert_printable_key(&mut buf, &mut cursor, "中"));
        assert_eq!(buf, "a中b");
        assert_eq!(cursor, 2);

        // 空缓冲：插到 0 号位
        let mut buf = String::new();
        let mut cursor = 0usize;
        assert!(insert_printable_key(&mut buf, &mut cursor, "x"));
        assert_eq!(buf, "x");
        assert_eq!(cursor, 1);

        // 光标越界（缓冲被别处整体替换过）：夹到尾部而不是 panic
        let mut buf = String::from("ab");
        let mut cursor = 99usize;
        assert!(insert_printable_key(&mut buf, &mut cursor, "c"));
        assert_eq!(buf, "abc");
        assert_eq!(cursor, 3);

        // 多字符键名不是可打印输入
        let mut buf = String::from("ab");
        let mut cursor = 1usize;
        assert!(!insert_printable_key(&mut buf, &mut cursor, "space"));
        assert!(!insert_printable_key(&mut buf, &mut cursor, "f5"));
        assert!(!insert_printable_key(&mut buf, &mut cursor, ""));
        assert_eq!(buf, "ab", "拒收的键不得改动缓冲区");
        assert_eq!(cursor, 1, "拒收的键不得移动光标");

        // 控制字符（漏网到默认分支的）同样不插入
        let mut buf = String::from("ab");
        let mut cursor = 1usize;
        assert!(!insert_printable_key(&mut buf, &mut cursor, "\u{1}"));
        assert_eq!(buf, "ab");
        assert_eq!(cursor, 1);
    }

    /// 派生快照的测试用键：字段与生产代码一致，值只为验证失效行为。
    fn sample_derived_key(revision: u64, target: &str) -> DerivedSnapshotKey {
        DerivedSnapshotKey {
            segments_revision: revision,
            target_lang: target.to_string(),
            low_confidence_bits: (-0.35f64).to_bits(),
            glossary_fingerprint: glossary_fingerprint(""),
            max_chars_per_line: 20,
            media_secs_bits: 60.0f64.to_bits(),
        }
    }

    /// 带指定的术语表原文构造测试用键：内部显式走一次指纹计算，测试里不出现原文克隆。
    fn sample_derived_key_with_glossary(
        revision: u64,
        target: &str,
        glossary: &str,
    ) -> DerivedSnapshotKey {
        DerivedSnapshotKey {
            glossary_fingerprint: glossary_fingerprint(glossary),
            ..sample_derived_key(revision, target)
        }
    }

    /// 测试用快照：内容不重要，重要的是「取回来的是不是上一次那一份」。
    fn sample_derived_snapshot() -> DerivedSnapshot {
        build_derived_snapshot(&[], 60.0, 20, "中文", -0.35, &[])
    }

    /// 同键连续取两次：第二次必须直接复用上一次算好的快照，不得重算。
    #[test]
    fn derived_cache_reuses_same_key_without_recompute() {
        let builds = Cell::new(0usize);
        let mut cache = None;
        let first = cached_derived_snapshot(&mut cache, sample_derived_key(7, "中文"), || {
            builds.set(builds.get() + 1);
            sample_derived_snapshot()
        });
        let second = cached_derived_snapshot(&mut cache, sample_derived_key(7, "中文"), || {
            builds.set(builds.get() + 1);
            sample_derived_snapshot()
        });
        assert_eq!(builds.get(), 1, "同键的第二次调用不得重算");
        assert!(Arc::ptr_eq(&first, &second), "同键必须返回同一份快照");
    }

    /// `segments_revision` 变了（任何改字幕的入口都会递增它）必须重算。
    #[test]
    fn derived_cache_recomputes_after_revision_bump() {
        let builds = Cell::new(0usize);
        let mut cache = None;
        let first = cached_derived_snapshot(&mut cache, sample_derived_key(7, "中文"), || {
            builds.set(builds.get() + 1);
            sample_derived_snapshot()
        });
        let second = cached_derived_snapshot(&mut cache, sample_derived_key(8, "中文"), || {
            builds.set(builds.get() + 1);
            sample_derived_snapshot()
        });
        assert_eq!(builds.get(), 2, "revision 变化必须重算");
        assert!(!Arc::ptr_eq(&first, &second));
    }

    /// 不进 `segments_revision` 的配置输入各自都必须入键：漏一项就是「改了设置，
    /// 界面数字不动」。
    #[test]
    fn derived_cache_recomputes_when_config_inputs_change() {
        let base = sample_derived_key(7, "中文");
        let variants = [
            DerivedSnapshotKey {
                target_lang: "English".to_string(),
                ..base.clone()
            },
            DerivedSnapshotKey {
                low_confidence_bits: (-0.5f64).to_bits(),
                ..base.clone()
            },
            DerivedSnapshotKey {
                glossary_fingerprint: glossary_fingerprint("苹果=Apple"),
                ..base.clone()
            },
            DerivedSnapshotKey {
                max_chars_per_line: 30,
                ..base.clone()
            },
            DerivedSnapshotKey {
                media_secs_bits: 61.0f64.to_bits(),
                ..base.clone()
            },
        ];
        for variant in variants {
            let builds = Cell::new(0usize);
            let mut cache = None;
            let first = cached_derived_snapshot(&mut cache, base.clone(), || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            });
            let second = cached_derived_snapshot(&mut cache, variant, || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            });
            assert_eq!(builds.get(), 2, "配置变化必须重算");
            assert!(!Arc::ptr_eq(&first, &second));
        }
    }

    /// 术语表**内容变了**必须重算：这里用长度不同的两份文本。
    #[test]
    fn derived_cache_recomputes_when_glossary_content_grows() {
        let mut cache = None;
        let builds = Cell::new(0usize);
        let first = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "苹果=Apple"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        let second = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "苹果=Apple\n香蕉=Banana"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        assert_eq!(builds.get(), 2, "术语表内容变化（长度不同）必须重算");
        assert!(!Arc::ptr_eq(&first, &second));
    }

    /// 术语表**内容变了**必须重算：**字节长度相同**的情况。只把长度塞进键是不够的，
    /// 等长替换（用户把某个词条改掉、长度恰好不变）必须能区分出来。
    #[test]
    fn derived_cache_recomputes_when_glossary_same_len_content_changes() {
        assert_eq!(
            "苹果=Apple".len(),
            "苹果=Appla".len(),
            "测试前提：两份文本字节长度相同"
        );
        assert_ne!(
            glossary_fingerprint("苹果=Apple"),
            glossary_fingerprint("苹果=Appla"),
            "指纹必须区分等长的不同内容（否则键会漏判）"
        );

        let mut cache = None;
        let builds = Cell::new(0usize);
        let first = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "苹果=Apple"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        let second = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "苹果=Appla"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        assert_eq!(builds.get(), 2, "等长的术语表内容变化必须重算");
        assert!(!Arc::ptr_eq(&first, &second));
    }

    /// 术语表**内容不变**必须命中：两份独立算出的指纹（同一内容的两次取键）必须判等，
    /// 否则会出现「什么都没改，快照却每帧重算」的回退。
    #[test]
    fn derived_cache_hits_when_glossary_content_unchanged() {
        let mut cache = None;
        let builds = Cell::new(0usize);
        let first = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "生成器=engine"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        let second = cached_derived_snapshot(
            &mut cache,
            sample_derived_key_with_glossary(7, "中文", "生成器=engine"),
            || {
                builds.set(builds.get() + 1);
                sample_derived_snapshot()
            },
        );
        assert_eq!(builds.get(), 1, "术语表内容不变必须命中，不得重算");
        assert!(Arc::ptr_eq(&first, &second));
    }

    /// 指纹算法与 `src/utils/frame_cache.rs::stable_hash` 同源：空输入的哈希必须是
    /// FNV-1a 的 offset basis，'a' 必须等于「basis ^ 'a' 再乘质数」——固定值钉死，
    /// 防止以后被换成 `DefaultHasher`（那个跨进程不稳定，两处缓存键就再也对不上）。
    #[test]
    fn glossary_fingerprint_matches_fnv1a_reference() {
        assert_eq!(
            glossary_fingerprint(""),
            GlossaryFingerprint {
                len: 0,
                hash: 0xcbf2_9ce4_8422_2325,
            }
        );
        let expected_a = (0xcbf2_9ce4_8422_2325u64 ^ 0x61).wrapping_mul(0x0000_0100_0000_01b3);
        assert_eq!(
            glossary_fingerprint("a"),
            GlossaryFingerprint {
                len: 1,
                hash: expected_a,
            }
        );
        assert_eq!(glossary_fingerprint("中文").len, 6, "长度按 UTF-8 字节算");
    }

    /// 快照必须与「直接调用原函数」逐字段一致——纯性能重构的底线：只要有一个字段
    /// 的口径漂了，界面上的数字就会与改造前不同。
    #[test]
    fn derived_snapshot_matches_direct_calls() {
        let mut a = Segment::new(1, 0.0, 2.0, "你好世界");
        a.translation = Some("Hi there".to_string());
        a.translation_lang = Some("English".to_string());
        a.confidence = Some(-0.5);
        // 译文与原文 trim 后相同：不算「独立译文」，但仍算「有译文」
        let mut b = Segment::new(2, 2.0, 4.0, "中");
        b.translation = Some("  中  ".to_string());
        b.translation_lang = Some("English".to_string());
        let mut c = Segment::new(3, 4.0, 6.0, "第二句有内容");
        c.translation = Some("Second".to_string());
        c.translation_lang = Some("Français".to_string());
        c.confidence = Some(-0.1);
        // 纯空白译文：既不算「有译文」，也不算「独立译文」
        let mut d = Segment::new(4, 6.0, 7.0, "   ");
        d.translation = Some("   ".to_string());
        // 有内容、没译文：质检的「未翻译」应当命中它
        let e = Segment::new(5, 8.0, 9.0, "第五句没有译文");
        let segments = vec![a, b, c, d, e];
        let media_secs = 60.0;
        let max_chars = 20usize;
        let target = "English";
        let threshold = -0.35f64;
        let entries = vec![("你好".to_string(), "Hello".to_string())];

        let snap = build_derived_snapshot(
            &segments, media_secs, max_chars, target, threshold, &entries,
        );

        assert_eq!(
            snap.stats,
            crate::subtitle::stats::compute(&segments, media_secs, max_chars)
        );
        assert_eq!(
            snap.translated_out,
            crate::ui::actions::translated_out_count(&segments, target)
        );
        assert_eq!(
            snap.expected_translation,
            crate::ui::actions::expected_translation_count(&segments)
        );
        assert_eq!(
            snap.other_lang_translated,
            translated_other_lang_count(&segments, target)
        );
        let translated_count = segments.iter().filter(|s| s.has_translation()).count();
        assert_eq!(snap.has_any_translation, translated_count > 0);
        // 双语判据：与改造前编辑器里内联的那段（三处同文）逐字对照
        let inline_reference = segments.iter().any(|s| {
            if let Some(t) = &s.translation {
                !t.trim().is_empty() && t.trim() != s.display_text().trim()
            } else {
                false
            }
        });
        assert_eq!(snap.has_distinct_translation, inline_reference);
        assert_eq!(
            snap.has_distinct_translation,
            has_distinct_translation(&segments)
        );
        // 质检报告：改造前转写页是「按空术语表算一遍、再用缓存的违规下标填进去」，
        // 与这里「直接带条目算」必须是同一份报告
        let mut expected = crate::subtitle::segment::quality_report(
            &segments,
            &[],
            threshold,
            translated_count > 0,
        );
        expected.glossary_violations =
            crate::subtitle::segment::glossary_violations(&segments, &entries);
        assert_eq!(*snap.quality_report, expected);

        // 具体数字也钉一遍，防止「两边一起错」：5 句、3 句有非空译文（含一条与原文
        // 相同的）、当前目标语言实际译出 2 句、另有 1 句法语译文、1 句未翻译。
        assert_eq!(snap.stats.segment_count, 5);
        assert_eq!(snap.stats.translated_count, 3);
        assert_eq!(snap.translated_out, 2);
        assert_eq!(snap.expected_translation, 4);
        assert_eq!(snap.other_lang_translated, 1);
        assert!(snap.has_any_translation);
        assert!(snap.has_distinct_translation);
        assert_eq!(snap.quality_report.low_confidence, vec![1]);
        assert_eq!(snap.quality_report.empty_or_short, vec![2, 4]);
        assert_eq!(snap.quality_report.untranslated, vec![5]);
        assert_eq!(snap.quality_report.glossary_violations, vec![1]);
    }

    /// 双语判据的 trim 口径：只在首尾空白上不同的译文**不算**独立译文；
    /// 纯空白译文连「有译文」都不是。
    #[test]
    fn distinct_translation_ignores_whitespace_only_differences() {
        let mut seg = Segment::new(1, 0.0, 1.0, "同一句话");
        assert!(!has_distinct_translation(std::slice::from_ref(&seg)));
        seg.translation = Some("同一句话\n".to_string());
        assert!(!has_distinct_translation(std::slice::from_ref(&seg)));
        seg.translation = Some("   ".to_string());
        assert!(!has_distinct_translation(std::slice::from_ref(&seg)));
        assert!(!has_any_translation(std::slice::from_ref(&seg)));
        seg.translation = Some("另一句话".to_string());
        assert!(has_distinct_translation(std::slice::from_ref(&seg)));
        assert!(has_any_translation(std::slice::from_ref(&seg)));
    }
    /// 「译文与原文一字不差」的派生位：与判据函数同源，且只在真有复制句时为真。
    ///
    /// 界面用它决定要不要给出「只清复制译文」入口；假阴性会让入口消失（用户只能
    /// 退回「清空全部译文」），假阳性则会让人点了没反应。
    #[test]
    fn derived_snapshot_carries_copied_translation_flag() {
        let mut copied = Segment::new(1, 0.0, 1.0, "这段译文其实是原样复制的");
        copied.translation = Some("这段译文其实是原样复制的".to_string());
        let mut real = Segment::new(2, 1.0, 2.0, "这段是真翻译");
        real.translation = Some("This one is translated".to_string());
        let with_copy = vec![copied, real];
        let snap = build_derived_snapshot(&with_copy, 60.0, 20, "中文", -0.35, &[]);
        assert!(snap.has_copied_translation);
        assert_eq!(
            snap.has_copied_translation,
            has_copied_translations(&with_copy)
        );

        // 没有译文 / 短句（`OK`→`OK` 这类本来就该原样保留）都不算复制句
        let mut short = Segment::new(3, 2.0, 3.0, "OK");
        short.translation = Some("OK".to_string());
        let snap = build_derived_snapshot(&[short], 60.0, 20, "中文", -0.35, &[]);
        assert!(!snap.has_copied_translation);
    }
    /// 回归（P3）：术语表违规下标缓存命中时必须复用同一个 `Arc`，不得重算、
    /// 更不得像原来那样 `Vec::clone()` 深拷贝（编辑台每帧都取一次这批下标）。
    #[test]
    fn glossary_violation_cache_hit_reuses_arc_without_recompute() {
        let mut cache: GlossaryBadCache = None;
        let mut computes = 0usize;
        let first = glossary_violations_cached(&mut cache, "生成器=engine", 7, || {
            computes += 1;
            vec![1, 4]
        });
        let second = glossary_violations_cached(&mut cache, "生成器=engine", 7, || {
            computes += 1;
            // 命中时这个闭包根本不该被执行：故意给一个完全不同的结果
            vec![99]
        });
        assert_eq!(computes, 1, "同一键必须命中缓存、不得重算");
        assert!(
            Arc::ptr_eq(&first, &second),
            "命中必须复用同一个 Arc（零深拷贝）"
        );
        assert_eq!(&*second, &[1usize, 4][..], "命中返回的仍是缓存内容");
        assert_eq!(second.to_vec(), vec![1, 4], "兼容入口（to_vec）内容不变");
    }

    /// 键的失效条件必须与改造前**等价**：术语表**内容**变了、或 `segments_revision`
    /// 推进了，都必须重算；否则会出现「改了术语表、琥珀色高亮不动」。
    #[test]
    fn glossary_violation_cache_invalidates_on_glossary_or_revision_change() {
        let mut cache: GlossaryBadCache = None;
        let base = glossary_violations_cached(&mut cache, "甲=乙", 1, || vec![2]);
        assert_eq!(&*base, &[2usize][..]);
        // 术语表文本变了（这里变成空表）→ 必须重算
        let emptied = glossary_violations_cached(&mut cache, "", 1, || vec![7]);
        assert_eq!(&*emptied, &[7usize][..], "术语表文本变化必须失效重算");
        assert!(!Arc::ptr_eq(&base, &emptied));
        // revision 推进 → 必须重算
        let bumped = glossary_violations_cached(&mut cache, "", 2, || vec![5]);
        assert_eq!(
            &*bumped,
            &[5usize][..],
            "segments_revision 推进必须失效重算"
        );
        assert!(!Arc::ptr_eq(&emptied, &bumped));
        // 同键再来一次 → 命中同一个 Arc（键是「原文 + revision」，两者都要一致）
        let again = glossary_violations_cached(&mut cache, "", 2, || vec![123]);
        assert!(
            Arc::ptr_eq(&bumped, &again),
            "同键（空术语表 + rev2）应命中"
        );
        // 文本回到旧值、但 revision 已是 2 → 仍然不得复用 rev1 的结果
        let reverted = glossary_violations_cached(&mut cache, "甲=乙", 2, || vec![6]);
        assert_eq!(
            &*reverted,
            &[6usize][..],
            "文本相同但 revision 变过，必须重算"
        );
    }

    /// 等价性测试用的片段表：故意让**存储顺序 ≠ 时间顺序**，并塞进重叠句、
    /// 零长度句、跨窗口左界的长句、压边的长句，以及重复的 `Segment::index`。
    fn timeline_sample_segments() -> Vec<Segment> {
        vec![
            Segment::new(7, 100.0, 130.0, "最靠后的长句：窗口右界附近"),
            Segment::new(3, 0.0, 0.0, "零长度：只在 t == 0 那一瞬可见"),
            Segment::new(11, 15.0, 15.0, "零长度：恰好落在 +15 秒边界"),
            Segment::new(1, 5.0, 10.0, "乱序出现的最早一句"),
            Segment::new(9, -20.0, -5.0, "跨窗口左界的长句（负时间）"),
            Segment::new(5, 40.0, 50.0, "远在窗口右侧"),
            Segment::new(2, 10.0, 30.0, "与本表其它句重叠"),
            Segment::new(13, 20.0, 20.0, "零长度：窗口内（index 重复）"),
            Segment::new(13, 20.0, 20.0, "零长度：窗口内（同 index 的第二份）"),
            Segment::new(4, -15.0, -15.0, "零长度：恰好落在 -15 秒边界"),
            Segment::new(6, -30.0, -15.0, "终点恰好压在窗口左界的长句"),
        ]
    }

    /// 旧的全量过滤（改造前 `render_multitrack_timeline` 里那段内联判据的参照物）：
    /// 按存储顺序收集「选中句，或与 ±margin 窗口相交」的下标。
    ///
    /// 这里刻意把三条分支逐字抄下来（含已被 ±margin 包含的 `cur ∈ [start, end]`），
    /// 它只服务于比对，不进生产代码。
    fn legacy_timeline_filter(
        segments: &[Segment],
        current_time: f64,
        selected: Option<usize>,
        margin: f64,
    ) -> Vec<usize> {
        segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                selected == Some(seg.index)
                    || (current_time >= seg.start && current_time <= seg.end)
                    || (current_time >= seg.start - margin && current_time <= seg.end + margin)
            })
            .map(|(pos, _)| pos)
            .collect()
    }

    /// 索引的三条性质：`by_start` 按 start 稳定升序、`prefix_max_end` 单调不减、
    /// `by_index` 按 `Segment::index` 升序且是 `0..n` 的排列。这三条是窗口查询
    /// 能安全二分的全部前提，乱序 / start 相同的段上也必须成立。
    #[test]
    fn timeline_index_orders_are_sorted_and_stable() {
        let segments = vec![
            Segment::new(3, 10.0, 12.0, "同 start 的后一份（存储顺序靠后）"),
            Segment::new(1, 0.0, 5.0, "最早"),
            Segment::new(2, 10.0, 11.0, "同 start 的前一份"),
            Segment::new(9, 4.0, 4.0, "零长度"),
            Segment::new(5, 0.0, 30.0, "跨窗口长句"),
        ];
        let index = build_timeline_index(&segments);

        assert_eq!(index.by_start.len(), segments.len());
        assert_eq!(index.prefix_max_end.len(), segments.len());
        // start 升序；两句 start 都是 10.0，稳定排序必须保持存储顺序（下标 0 在 2 前）
        assert_eq!(index.by_start, vec![1usize, 4, 3, 0, 2]);
        // 前缀最大 end 按 by_start 顺序累积：5.0 → 30.0 → 30.0 → 30.0 → 30.0
        assert_eq!(index.prefix_max_end, vec![5.0, 30.0, 30.0, 30.0, 30.0]);
        for pair in index.prefix_max_end.windows(2) {
            assert!(pair[1] >= pair[0], "前缀最大 end 必须单调不减：{pair:?}");
        }
        // by_index 升序，且恰好是 0..n 的一个排列
        assert_eq!(index.by_index, vec![1usize, 2, 0, 4, 3]);
        let mut perm = index.by_index.clone();
        perm.sort_unstable();
        assert_eq!(perm, (0..segments.len()).collect::<Vec<_>>());
        // 空表也得能建：渲染路径在「还没有字幕」时就会走到这里
        let empty = build_timeline_index(&[]);
        assert!(empty.by_start.is_empty());
        assert!(empty.prefix_max_end.is_empty());
        assert!(empty.by_index.is_empty());
    }

    /// 窗口查询与「旧的全量过滤」逐点等价——这是本次改造的底线：给定重叠、乱序、
    /// 零长度、恰好压在窗口边界上的片段表，扫一遍整条时间轴逐点比对两份结果
    /// （元素与顺序都要一致，因为渲染顺序就是返回顺序）。
    #[test]
    fn timeline_window_query_matches_full_scan_filter() {
        let segments = timeline_sample_segments();
        let index = build_timeline_index(&segments);
        let margin = 15.0f64;
        let selections = [
            None,
            Some(0usize),  // 表里没有这个 index：不该因此让任何一句可见
            Some(7usize),  // 远在窗口右侧的那句：选中句必须恒可见
            Some(13usize), // 重复出现的 index：旧实现两处都可见
        ];
        let mut checks = 0usize;
        let mut t = -40.0f64;
        while t <= 140.0 {
            for selected in selections {
                assert_eq!(
                    timeline_visible_positions(&segments, &index, t, selected, margin),
                    legacy_timeline_filter(&segments, t, selected, margin),
                    "t={t} selected={selected:?}"
                );
                checks += 1;
            }
            // 步长 0.5 秒：既能扫过所有片段，也能覆盖「恰好压界」的整数时刻
            t += 0.5;
        }
        assert!(checks > 1400, "扫描点数太少（{checks}），等价性覆盖不足");
    }

    /// 边界口径与旧实现一致（两侧都是闭区间）：恰好落在 ±margin 上的片段必须可见，
    /// 刚超出一点点的必须不可见——这是等价性里最容易改错的一格。
    #[test]
    fn timeline_window_edges_are_inclusive() {
        let margin = 15.0f64;
        let segments = vec![
            Segment::new(1, -15.0, -15.0, "零长度：恰好压在左界"),
            Segment::new(2, 15.0, 15.0, "零长度：恰好压在右界"),
            Segment::new(3, -15.001, -15.001, "刚超出左界"),
            Segment::new(4, 15.001, 15.001, "刚超出右界"),
            Segment::new(5, -100.0, -15.0, "终点恰好压在左界的长句"),
            Segment::new(6, 15.0, 100.0, "起点恰好压在右界的长句"),
        ];
        let index = build_timeline_index(&segments);
        // t = 0：窗口 [-15, 15]
        assert_eq!(
            timeline_visible_positions(&segments, &index, 0.0, None, margin),
            vec![0usize, 1, 4, 5],
            "压界的必须可见、越界的必须不可见，且按存储顺序"
        );
        // margin = 0 时窗口退化成一个点：上面这些段一条都不该命中
        assert!(timeline_visible_positions(&segments, &index, 0.0, None, 0.0).is_empty());
        // 空表 / 全部在窗口外：返回空而不是 panic
        assert!(
            timeline_visible_positions(&[], &build_timeline_index(&[]), 0.0, None, margin)
                .is_empty()
        );
        let far = vec![Segment::new(1, 1000.0, 1001.0, "远")];
        let far_index = build_timeline_index(&far);
        assert!(timeline_visible_positions(&far, &far_index, 0.0, None, margin).is_empty());
    }

    /// 索引必须随派生快照一起给（同一份缓存，不另开一份）：渲染路径只从快照里取，
    /// 索引与片段表一旦不同步，时间轴就会画错块。
    #[test]
    fn derived_snapshot_carries_timeline_index() {
        let segments = timeline_sample_segments();
        let snap = build_derived_snapshot(&segments, 60.0, 20, "中文", -0.35, &[]);
        assert_eq!(snap.timeline, build_timeline_index(&segments));
        assert_eq!(
            timeline_visible_positions(&segments, &snap.timeline, 20.0, Some(13), 15.0),
            timeline_visible_positions(
                &segments,
                &build_timeline_index(&segments),
                20.0,
                Some(13),
                15.0
            ),
            "走快照里的索引与现建索引必须给出同一批下标"
        );
    }

    // ── 字幕搜索：索引扫描 / 增量收窄与旧实现逐字等价（第 4 项提速的底线）──

    /// 改造前 `matched_indices` 的**逐字内联副本**（等价性参照物）。
    ///
    /// 刻意抄一份而不是直接调库里那份：这个测试要钉死的正是「改造前后结果逐字
    /// 相同」，参照物必须是改造前那段代码本身。另有一条测试把这份副本与
    /// `crate::subtitle::matched_indices` 对齐，防止副本自己腐烂。
    fn reference_matched_indices(segments: &[Segment], needle: &str) -> Vec<usize> {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return (0..segments.len()).collect();
        }
        segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                seg.display_text().to_lowercase().contains(&needle)
                    || seg
                        .translation
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .map(|(pos, _)| pos)
            .collect()
    }

    /// 复刻 `MainWindow::refresh_subtitle_filter` 的决策流程（不含 GPUI）。
    ///
    /// `published` 就是 `(subtitle_filter_key, subtitle_filter)` 这对字段：等价性
    /// 测试必须以**同一套决策**驱动索引，否则测的就不是真正上线的那条路径。
    fn drive_filter(
        cache: &mut SubtitleSearchCache,
        published: &mut (Option<(String, u64)>, Vec<usize>),
        segments: &[Segment],
        revision: u64,
        raw: &str,
    ) -> Vec<usize> {
        drive_filter_traced(cache, published, segments, revision, raw).0
    }

    /// [`drive_filter`] 的带痕版本：第二个返回值表示本次是否真的走了收窄路径。
    /// 没有它，「收窄路径被覆盖到」就无从断言（等价性测试可能全在走全量扫描，
    /// 看着绿，实际什么都没测到）。
    fn drive_filter_traced(
        cache: &mut SubtitleSearchCache,
        published: &mut (Option<(String, u64)>, Vec<usize>),
        segments: &[Segment],
        revision: u64,
        raw: &str,
    ) -> (Vec<usize>, bool) {
        let segment_count = segments.len();
        if matches!(
            published.0.as_ref(),
            Some((key, rev)) if *rev == revision && key == raw
        ) && filter_cache_usable(&published.1, segment_count)
        {
            // 键没变：与 `refresh_subtitle_filter` 一样直接复用，谈不上收窄。
            return (published.1.clone(), false);
        }
        cache.sync(segments, revision);
        cache.set_needle(raw);
        // 与 `refresh_subtitle_filter` 同判据：旧下标越界时必须退回全量扫描。
        let narrowed =
            filter_cache_usable(&published.1, segment_count) && cache.can_narrow(revision);
        let mut matched = if narrowed {
            cache.narrow(&published.1)
        } else {
            cache.scan()
        };
        if !crate::subtitle::indices_cover_segments(&matched, segment_count) {
            matched = (0..segment_count).collect();
        }
        published.1 = matched.clone();
        published.0 = Some((raw.to_string(), revision));
        cache.commit(revision);
        (matched, narrowed)
    }

    /// 覆盖大小写 / 中英混排 / 只有译文命中 / 上下文相关小写映射的样本。
    fn search_sample_segments() -> Vec<Segment> {
        let mut segs = vec![
            Segment::new(1, 0.0, 1.0, "Transformer 架构入门"),
            Segment::new(2, 1.0, 2.0, "transformer 全部小写"),
            Segment::new(3, 2.0, 3.0, "今天天气很好"),
            Segment::new(4, 3.0, 4.0, "İstanbul 的土耳其字母"),
            Segment::new(5, 4.0, 5.0, "ΟΔΟΣ ΑΘΗΝΑ"),
            Segment::new(6, 5.0, 6.0, "ΟΔΟΣΑ ΝΙΚΗ"),
        ];
        segs[1].translation = Some("TRANSFORMER 大写译文".to_string());
        segs[2].translation = Some("The weather is nice today".to_string());
        segs[3].translation = Some("transformer 只出现在译文里".to_string());
        segs
    }

    /// 全量扫描必须与旧实现逐项一致：ASCII 大小写、中文、纯译文命中、无命中、
    /// 首尾空白关键字（trim 口径）全都要对上。
    #[test]
    fn search_scan_matches_inlined_reference() {
        let segments = search_sample_segments();
        let mut cache = SubtitleSearchCache::new();
        assert!(cache.sync(&segments, 1), "首次同步必须真的建索引");
        for key in [
            "transformer",
            "TRANSFORMER",
            "架构",
            "weather",
            "只有译文",
            "  npm  ",
            "不存在的词",
            "İSTANBUL",
            "ΟΔΟΣΑ",
        ] {
            cache.set_needle(key);
            assert_eq!(
                cache.scan(),
                reference_matched_indices(&segments, key),
                "关键字「{key}」的扫描结果与旧实现不一致"
            );
        }
    }

    /// 逐字输入（每次把关键字延长一个字符）必须与旧实现逐项一致——这是第 4 项
    /// 提速的正确性底线：收窄只能少扫，不能少给。
    #[test]
    fn search_typing_narrows_to_same_result_as_reference() {
        let segments = search_sample_segments();
        let revision = 7;
        let mut cache = SubtitleSearchCache::new();
        let mut published: (Option<(String, u64)>, Vec<usize>) = (None, Vec::new());
        for typed in ["transformer", "全小写", "İstanbul", "ΟΔΟΣ"] {
            let mut key = String::new();
            let mut narrowed_at_least_once = false;
            for ch in typed.chars() {
                key.push(ch);
                let (got, narrowed) =
                    drive_filter_traced(&mut cache, &mut published, &segments, revision, &key);
                // 从第二个字符起，键是上一个键的延长：除了首字符那一步，全都该走收窄
                if key.chars().count() > 1 {
                    assert!(narrowed, "「{key}」是上一个键的延长，应当走收窄路径");
                }
                narrowed_at_least_once |= narrowed;
                assert_eq!(
                    got,
                    reference_matched_indices(&segments, &key),
                    "逐字输入到「{key}」时与旧实现不一致"
                );
            }
            assert!(
                narrowed_at_least_once,
                "「{typed}」的逐字输入至少应有一次走收窄路径（否则这条测试没测到收窄）"
            );
            // 清空关键字：回到「不过滤」，且下一次输入重新开始
            let cleared = drive_filter(&mut cache, &mut published, &segments, revision, "");
            assert_eq!(cleared, (0..segments.len()).collect::<Vec<_>>());
        }
    }

    /// 内联参照物本身必须与生产实现对齐：副本一旦腐烂，上面的等价性测试就成了
    /// 自证清白。
    #[test]
    fn search_reference_matches_production_matched_indices() {
        let segments = search_sample_segments();
        for key in [
            "transformer",
            "架构",
            "weather",
            "",
            "  ",
            "ΟΔΟΣΑ",
            "没这句",
        ] {
            assert_eq!(
                reference_matched_indices(&segments, key),
                crate::subtitle::matched_indices(&segments, key),
                "内联参照物与 `matched_indices` 在「{key}」上分家了"
            );
        }
    }

    /// 非「延长」的改键（退格、中间插入、清空后重打）必须退回全量扫描，
    /// 结果依旧逐项等价。
    #[test]
    fn search_non_extension_edits_match_reference() {
        let segments = search_sample_segments();
        let revision = 12;
        let mut cache = SubtitleSearchCache::new();
        let mut published: (Option<(String, u64)>, Vec<usize>) = (None, Vec::new());
        for key in [
            "transformer",
            "transforme",
            "transfor",
            "transXformer",
            "transX",
            "t",
            "",
            "transformer",
        ] {
            let got = drive_filter(&mut cache, &mut published, &segments, revision, key);
            assert_eq!(
                got,
                reference_matched_indices(&segments, key),
                "改键到「{key}」时与旧实现不一致"
            );
        }
    }

    /// 修订号推进（用户编辑了字幕）后必须重建索引：结果反映新文本，而不是旧索引
    /// 或上一份结果；同一修订号下改多少个键都只建一次索引。
    #[test]
    fn search_index_rebuilds_after_revision_bump() {
        let mut segments = search_sample_segments();
        let key = "新加的句子";
        let mut cache = SubtitleSearchCache::new();
        let mut published: (Option<(String, u64)>, Vec<usize>) = (None, Vec::new());
        assert!(drive_filter(&mut cache, &mut published, &segments, 1, key).is_empty());

        segments.push(Segment::new(9, 9.0, 10.0, "新加的句子在这里"));
        let got = drive_filter(&mut cache, &mut published, &segments, 2, key);
        assert_eq!(got, reference_matched_indices(&segments, key));
        assert_eq!(got, vec![segments.len() - 1], "必须命中新加的那一句");

        for other in ["新加", "新加的句", "在这里"] {
            drive_filter(&mut cache, &mut published, &segments, 2, other);
            assert!(
                !cache.sync(&segments, 2),
                "同一修订号下改键不该重建索引（否则提速就白做了）"
            );
        }
    }

    /// 上下文相关的小写映射（土耳其 İ、希腊语词尾 Σ）不能靠「前缀即收窄」蒙混：
    /// 归一化后不是前缀时必须退回全量扫描，且结果仍与旧实现一致。
    #[test]
    fn search_context_sensitive_case_folding_matches_reference() {
        let segments = search_sample_segments();
        let revision = 21;
        let mut cache = SubtitleSearchCache::new();
        let mut published: (Option<(String, u64)>, Vec<usize>) = (None, Vec::new());
        for key in ["ΟΔΟ", "ΟΔΟΣ"] {
            let got = drive_filter(&mut cache, &mut published, &segments, revision, key);
            assert_eq!(got, reference_matched_indices(&segments, key));
        }
        // 追加 Α 后 Σ 从「词尾」变「词中」：归一化从「…ος」变成「…οσα」，
        // 与上一次关键字不再是前缀关系，必须放弃收窄。
        cache.set_needle("ΟΔΟΣΑ");
        assert!(
            !cache.can_narrow(revision),
            "归一化后不是前缀时必须放弃收窄（否则会漏掉 ΟΔΟΣΑ 那一句）"
        );
        let got = drive_filter(&mut cache, &mut published, &segments, revision, "ΟΔΟΣΑ");
        assert_eq!(got, reference_matched_indices(&segments, "ΟΔΟΣΑ"));
        assert_eq!(got, vec![5], "必须命中「ΟΔΟΣΑ ΝΙΚΗ」那一句");
    }

    /// 空关键字 / 纯空白关键字 = 不过滤（全部下标），归一化口径与旧实现同义。
    #[test]
    fn search_empty_needle_lists_every_segment() {
        let segments = search_sample_segments();
        let mut cache = SubtitleSearchCache::new();
        for key in ["", "   ", "\t"] {
            cache.sync(&segments, 1);
            cache.set_needle(key);
            assert_eq!(cache.scan(), (0..segments.len()).collect::<Vec<_>>());
            assert_eq!(
                normalize_search_needle(key),
                key.trim().to_lowercase(),
                "归一化必须与旧实现同口径"
            );
        }
        assert_eq!(normalize_search_needle("  AbC  "), "abc");
        // `İ` 小写化会展开成两个字符（i + U+0307）：归一化口径必须原样保留这种展开，
        // 不能自作主张做「只映射单字符」的简化。
        assert_eq!(normalize_search_needle("İ").chars().count(), 2);
    }

    // ── 替换规则集（第 11 项）：JSON 读写 / 增删改查 / 按顺序应用 ──

    /// JSON 往返逐字段一致，且必须是**扁平**结构（顶层数组、规则直接挂载）。
    #[test]
    fn replace_rule_sets_json_roundtrip_is_flat() {
        let sets = vec![
            ReplaceRuleSet {
                name: "口头禅".to_string(),
                rules: vec![
                    ReplaceRule {
                        find: "呃".to_string(),
                        replace: String::new(),
                    },
                    ReplaceRule {
                        find: "那个".to_string(),
                        replace: String::new(),
                    },
                ],
            },
            ReplaceRuleSet {
                name: "术语".to_string(),
                rules: vec![ReplaceRule {
                    find: "transformer".to_string(),
                    replace: "变换器".to_string(),
                }],
            },
        ];
        let json = replace_rule_sets_to_json(&sets);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("导出必须是合法 JSON");
        assert!(parsed.is_array(), "顶层必须是数组");
        assert!(
            parsed.as_array().expect("数组")[0]
                .get("rules")
                .is_some_and(|r| r.is_array()),
            "规则必须直接挂在规则集上（扁平），不能再套一层"
        );
        assert_eq!(replace_rule_sets_from_json(&json).expect("往返"), sets);
    }

    /// 顶层不是数组 / 不是 JSON → 明确报错，让调用方能提示用户看文件来源。
    #[test]
    fn replace_rule_sets_from_json_rejects_garbage() {
        assert!(replace_rule_sets_from_json("{\"name\":\"x\"}").is_err());
        assert!(replace_rule_sets_from_json("这不是 JSON").is_err());
    }

    /// 单条损坏不整批失败：空名跳过、缺 `find` 的规则跳过、缺 `replace` 按空串补齐。
    #[test]
    fn replace_rule_sets_from_json_salvages_partial_entries() {
        let raw = r#"[
            {"name": "   ", "rules": [{"find": "a", "replace": "b"}]},
            {"name": "保留", "rules": [{"find": "a"}, {"replace": "b"}, {"find": "c", "replace": "d"}]}
        ]"#;
        let sets = replace_rule_sets_from_json(raw).expect("能救则救");
        assert_eq!(sets.len(), 1, "空名的条目应被跳过");
        assert_eq!(sets[0].name, "保留");
        assert_eq!(sets[0].rules.len(), 2);
        assert_eq!(sets[0].rules[0].replace, "");
        assert_eq!(sets[0].rules[1].find, "c");
        assert_eq!(sets[0].rules[1].replace, "d");
    }

    /// 增 / 改：同名覆盖（不追加副本）、新名追加，并返回落点下标；空名报错。
    #[test]
    fn replace_rule_set_upsert_replaces_same_name_and_appends_new() {
        let mut sets: Vec<ReplaceRuleSet> = Vec::new();
        let first = ReplaceRuleSet {
            name: "A".to_string(),
            rules: vec![ReplaceRule {
                find: "1".to_string(),
                replace: "2".to_string(),
            }],
        };
        assert_eq!(upsert_replace_rule_set(&mut sets, first).expect("新增"), 0);
        assert_eq!(
            upsert_replace_rule_set(
                &mut sets,
                ReplaceRuleSet {
                    name: "B".to_string(),
                    rules: Vec::new(),
                }
            )
            .expect("新增"),
            1
        );
        let updated = ReplaceRuleSet {
            name: "A".to_string(),
            rules: vec![ReplaceRule {
                find: "x".to_string(),
                replace: "y".to_string(),
            }],
        };
        assert_eq!(
            upsert_replace_rule_set(&mut sets, updated.clone()).expect("覆盖"),
            0
        );
        assert_eq!(sets.len(), 2, "同名必须覆盖而不是追加");
        assert_eq!(find_replace_rule_set(&sets, "A"), Some(&updated));
        assert!(upsert_replace_rule_set(
            &mut sets,
            ReplaceRuleSet {
                name: "   ".to_string(),
                rules: Vec::new(),
            }
        )
        .is_err());
    }

    /// 删：按名删除并返回是否命中；删不存在的名字不改动任何东西。
    #[test]
    fn replace_rule_set_remove_by_name() {
        let mut sets = vec![
            ReplaceRuleSet {
                name: "A".to_string(),
                rules: Vec::new(),
            },
            ReplaceRuleSet {
                name: "B".to_string(),
                rules: Vec::new(),
            },
        ];
        assert!(remove_replace_rule_set(&mut sets, "A"));
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].name, "B");
        assert!(!remove_replace_rule_set(&mut sets, "A"));
    }

    /// 多条规则**按顺序**应用：前一条的输出是后一条的输入。
    #[test]
    fn replace_rules_apply_in_order() {
        let mut segs = vec![Segment::new(1, 0.0, 1.0, "a b c")];
        let set = ReplaceRuleSet {
            name: "链式".to_string(),
            rules: vec![
                ReplaceRule {
                    find: "a".to_string(),
                    replace: "b".to_string(),
                },
                ReplaceRule {
                    find: "b".to_string(),
                    replace: "c".to_string(),
                },
            ],
        };
        let report = apply_replace_rule_set(&mut segs, &set, &ReplaceOptions::default());
        assert_eq!(segs[0].text, "c c c", "顺序反过来结果就不是这个");
        assert_eq!(report.rules_hit, 2);
        assert_eq!(report.total_hits(), 3, "1 处 + 2 处");
        assert_eq!(report.changed, vec![1]);

        // 空 find 与 find == replace 的规则都不算命中（与 `replace_all` 同禁区）
        let noop = ReplaceRuleSet {
            name: "空操作".to_string(),
            rules: vec![
                ReplaceRule {
                    find: String::new(),
                    replace: "x".to_string(),
                },
                ReplaceRule {
                    find: "c".to_string(),
                    replace: "c".to_string(),
                },
            ],
        };
        let report = apply_replace_rule_set(&mut segs, &noop, &ReplaceOptions::default());
        assert!(report.is_empty(), "无命中时报告必须是空的");
    }

    /// 应用到选中句：只有指定下标被改，越界下标跳过；报告里的句序号口径不变。
    #[test]
    fn replace_rules_apply_to_selected_indices_only() {
        let mut segs = vec![
            Segment::new(1, 0.0, 1.0, "aaa"),
            Segment::new(2, 1.0, 2.0, "aaa"),
            Segment::new(3, 2.0, 3.0, "aaa"),
        ];
        let set = ReplaceRuleSet {
            name: "s".to_string(),
            rules: vec![ReplaceRule {
                find: "a".to_string(),
                replace: "b".to_string(),
            }],
        };
        let report = apply_replace_rule_set_to_indices(
            &mut segs,
            &[1, 99],
            &set,
            &ReplaceOptions::default(),
        );
        assert_eq!(segs[0].text, "aaa", "没选中的句子一字不动");
        assert_eq!(segs[1].text, "bbb");
        assert_eq!(segs[2].text, "aaa");
        assert_eq!(report.changed, vec![2]);
        assert_eq!(report.total_hits(), 3);
    }

    /// 磁盘往返：写进去再读回来逐字段一致；文件不存在视为空表（不是错误）。
    #[test]
    fn replace_rules_save_load_roundtrip_on_disk() {
        let dir = std::env::temp_dir().join(format!("v2w_rules_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("replace_rules.json");
        assert!(
            load_replace_rule_sets(&path)
                .expect("缺文件应视为空表")
                .is_empty(),
            "首次运行还没有规则集，不该当成错误"
        );
        let sets = vec![ReplaceRuleSet {
            name: "清理".to_string(),
            rules: vec![ReplaceRule {
                find: "嗯".to_string(),
                replace: String::new(),
            }],
        }];
        save_replace_rule_sets(&path, &sets).expect("父目录不存在也要建出来再写");
        assert_eq!(load_replace_rule_sets(&path).expect("读回"), sets);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写失败必须**报错给调用方**（父路径被一个同名文件占住 → 建目录失败）。
    #[test]
    fn replace_rules_save_reports_error_to_caller() {
        let dir = std::env::temp_dir().join(format!("v2w_rules_err_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let blocker = dir.join("plain_file");
        std::fs::write(&blocker, b"x").expect("占位文件");
        let err = save_replace_rule_sets(&blocker.join("replace_rules.json"), &[])
            .expect_err("父路径是文件时保存必须失败");
        assert!(err.contains("规则集"), "错误文案要说清是什么失败：{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
