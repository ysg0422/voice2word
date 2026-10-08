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
use crate::subtitle::{indices_cover_segments, matched_indices, ExportMode};
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
}

impl EditorExportFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::JianYing => "剪映草稿 (一键直出)",
            Self::JianYingFolder => "剪映草稿 (导出到文件夹)",
            Self::Srt => "SRT 字幕 (.srt)",
            Self::Ass => "ASS 特效字幕 (.ass)",
            Self::Fcpxml => "FCPXML (达芬奇 / FCP)",
            Self::PremiereXml => "Premiere XML (.xml)",
            Self::Txt => "TXT 纯文本 (.txt)",
            Self::Vtt => "VTT 网页字幕 (.vtt)",
            Self::Json => "JSON 结构化字幕 (.json)",
            Self::EbuTtD => "EBU-TT-D 广播字幕 (.ttml)",
            Self::NetflixTtal => "Netflix TTAL (.ttal)",
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
}

/// 在线翻译 API 配置卡片里的三个文本输入框，用于把按键派发到正确的缓冲区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiField {
    Base,
    Model,
    Key,
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
    ToggleTheme,
    SwitchToEditor,
    SwitchToGenerate,
    SwitchToLibrary,
    SwitchToPerformance,
    Undo,
    Redo,
}

/// 命令面板里一条命令的展示信息。
///
/// `zh` / `en` 都参与搜索：中文用户按「导出」找，英文习惯的用户按 `export` 找，
/// 只认一种别名就会有一半人搜不到。`hint` 是右侧的键位提示，没有独立键位的命令留空。
pub struct PaletteCommandSpec {
    pub command: PaletteCommand,
    /// 中文名
    pub zh: &'static str,
    /// 英文别名（小写，仅作匹配与副标题展示）
    pub en: &'static str,
    /// 右侧键位提示；空串表示这条命令只有面板入口
    pub hint: &'static str,
}

/// 命令面板的命令集。
///
/// 每条命令都直接复用 `MainWindow` 上已有的方法（见 `run_palette_command`），
/// 这里只做「名字 → 已有入口」的映射，不重写任何业务逻辑。
pub const PALETTE_COMMANDS: [PaletteCommandSpec; 13] = [
    PaletteCommandSpec {
        command: PaletteCommand::OpenFile,
        zh: "打开文件",
        en: "open file",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::StartTranscription,
        zh: "开始转写",
        en: "start transcription",
        hint: "Ctrl+Enter",
    },
    PaletteCommandSpec {
        command: PaletteCommand::CancelTranscription,
        zh: "终止转写",
        en: "stop transcription",
        hint: "Esc",
    },
    PaletteCommandSpec {
        command: PaletteCommand::ExportCurrentFormat,
        zh: "导出（当前格式）",
        en: "export subtitle",
        hint: "Ctrl+E",
    },
    PaletteCommandSpec {
        command: PaletteCommand::StartTranslation,
        zh: "开始翻译",
        en: "start translation",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::CancelTranslation,
        zh: "取消翻译",
        en: "cancel translation",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::ToggleTheme,
        zh: "切换主题",
        en: "toggle theme",
        hint: "Ctrl+T",
    },
    PaletteCommandSpec {
        command: PaletteCommand::SwitchToEditor,
        zh: "切到剪辑校对",
        en: "editor tab",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::SwitchToGenerate,
        zh: "切到语音转写",
        en: "transcribe tab",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::SwitchToLibrary,
        zh: "切到视频库",
        en: "library tab",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::SwitchToPerformance,
        zh: "切到性能设置",
        en: "performance tab",
        hint: "",
    },
    PaletteCommandSpec {
        command: PaletteCommand::Undo,
        zh: "撤销",
        en: "undo",
        hint: "Ctrl+Z",
    },
    PaletteCommandSpec {
        command: PaletteCommand::Redo,
        zh: "重做",
        en: "redo",
        hint: "Ctrl+Y",
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
    /// 结果列表里的选中行号（不是 `PALETTE_COMMANDS` 的下标）
    pub(crate) selected: usize,
    /// 搜索框的焦点句柄；面板关闭时随之丢弃
    pub(crate) focus: FocusHandle,
    /// 结果列表滚动句柄：↑/↓ 移动选中时把该行滚进视野
    pub(crate) scroll: ScrollHandle,
}

/// 命令面板结果列表的最大高度：超过就滚动，避免十几条命令把面板撑满整屏。
const COMMAND_PALETTE_LIST_H: f32 = 280.0;

/// 按查询词过滤命令面板条目，返回 [`PALETTE_COMMANDS`] 里的下标序列。
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
        return (0..PALETTE_COMMANDS.len()).collect();
    }
    PALETTE_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, spec)| {
            spec.zh.to_lowercase().contains(needle.as_str())
                || spec.en.to_lowercase().contains(needle.as_str())
        })
        .map(|(index, _)| index)
        .collect()
}

pub struct MainWindow {
    pub(crate) state: AppState,
    pub(crate) metrics_rx: watch::Receiver<crate::app::ResourceMetrics>,
    pub(crate) text_focus: FocusHandle,
    pub(crate) is_text_focused: bool,
    pub(crate) is_extracting_frame: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) pending_extract_time: Arc<std::sync::Mutex<Option<f64>>>,
    pub(crate) last_drag_extract: std::time::Instant,
    pub(crate) play_tick_generation: u64,
    pub(crate) completion_dialog: Option<CompletionDialogInfo>,
    pub(crate) benchmark_dialog: Option<BenchmarkDialogInfo>,
    /// 待确认的破坏性操作（清空队列 / 删除记录）。`Some` 时渲染通用确认弹窗。
    pub(crate) confirm_dialog: Option<ConfirmDialogInfo>,
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
    pub(crate) editor_export_format: EditorExportFormat,
    /// 导出内容模式：原文 / 仅译文 / 双语。对字幕文件与剪辑工程文件统一生效。
    pub(crate) editor_export_mode: ExportMode,
    /// 术语表编辑的最近状态提示（打开文件 / 应用结果）。`None` 表示没操作过。
    pub(crate) glossary_status: Option<String>,
    pub(crate) is_export_dropdown_open: bool,
    /// 字幕预览框的**手动**宽度（px）。`Some` 时以它为准并压过「单行最大字数」
    /// 推出的宽度；拖动预览条两侧把手会写入这里并落盘，`None` 则回到按字数自动推算。
    pub(crate) preview_box_w: Option<f32>,
    /// 预览条拖动会话：(基准宽度, 按下时的鼠标 x, 是否左手柄)。`Some` 表示正在拖。
    ///
    /// 记下是哪个手柄，是因为左右两侧「往外拉」的位移方向相反：右手柄往右拉
    /// 位移为正，左手柄往左拉位移为负。宽度变化要按手柄所在侧取符号，否则左手柄
    /// 往外拉会算成负增量，框反而变窄。
    pub(crate) preview_drag: Option<(f32, f32, bool)>,
    /// 右侧检查器当前显示哪张面板（样式 / 翻译），互斥切换
    pub(crate) subtitle_panel: EditorSubtitlePanel,
    pub(crate) text_cursor_pos: usize,
    /// 内嵌播放器 RenderImage 缓存：(帧版本号, 帧宽, 帧高, 已构建的 GPU 纹理图像)。
    /// 帧版本与尺寸均未变时直接复用，避免每次渲染深拷贝帧数据并重新上传纹理。
    pub(crate) cached_live_image: Option<(u64, u32, u32, Arc<RenderImage>)>,
    /// 字幕清单虚拟列表的滚动位置句柄（uniform_list 仅渲染可视行）
    pub(crate) subtitle_list_scroll: UniformListScrollHandle,
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
    pub(crate) subtitle_search_focused: bool,
    pub(crate) subtitle_search_cursor: usize,
    /// 当前搜索命中的字幕在 `state.segments` 中的下标序列。
    /// 虚拟列表按此序列渲染，因此过滤后行高与滚动条仍然正确。
    pub(crate) subtitle_filter: Vec<usize>,
    /// `subtitle_filter` 的缓存有效性依据：`(搜索关键字, segments_revision)`。
    /// 两者都没变时直接复用上次算好的下标序列，避免每帧重跑子串匹配。
    pub(crate) subtitle_filter_key: Option<(String, u64)>,
    /// 术语表疑似未命中句下标的缓存：键为 `(术语表文本, segments_revision)`。
    /// 与 `subtitle_filter` 同理——播放时每 40ms 重绘一次，逐帧对上千句 × 术语条数
    /// 跑子串匹配会白白掉帧。
    pub(crate) glossary_bad_cache: Option<((String, u64), Vec<usize>)>,
    /// 在线翻译 API 配置卡片的编辑缓冲（改完即写 config.toml）
    pub(crate) api_base_input: String,
    pub(crate) api_model_input: String,
    pub(crate) api_key_input: String,
    pub(crate) api_base_focus: FocusHandle,
    pub(crate) api_model_focus: FocusHandle,
    pub(crate) api_key_focus: FocusHandle,
    /// 当前聚焦的 API 输入框；`None` 表示三个框都未聚焦
    pub(crate) focused_api_field: Option<ApiField>,
    /// 单行输入共用的光标位置（同一时刻只有一个框聚焦，无需每框一个）
    pub(crate) line_edit_cursor: usize,
    /// API Key 是否明文显示（默认掩码，避免录屏/截图泄露）
    pub(crate) api_key_visible: bool,
    /// 在线翻译接口连通性自检状态
    pub(crate) is_probing_translate: bool,
    /// `(是否成功, 提示文案)`；`None` 表示尚未测试过
    pub(crate) translate_probe_msg: Option<(bool, String)>,
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
    let key = event.keystroke.key.as_str();
    let total = buffer.chars().count();
    *cursor = (*cursor).min(total);

    if event.keystroke.modifiers.control {
        match key {
            "a" => {
                *cursor = total;
                return true;
            }
            "c" => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(buffer.clone()));
                return true;
            }
            "v" => {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        // 单行输入框：把换行折成空格，避免粘贴多行内容撑破布局
                        let flat = text.replace(['\r', '\n'], " ");
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
        "backspace" => {
            if *cursor > 0 && total > 0 {
                let mut chars: Vec<char> = buffer.chars().collect();
                chars.remove(*cursor - 1);
                *buffer = chars.into_iter().collect();
                *cursor -= 1;
            }
            true
        }
        "delete" => {
            if *cursor < total {
                let mut chars: Vec<char> = buffer.chars().collect();
                chars.remove(*cursor);
                *buffer = chars.into_iter().collect();
            }
            true
        }
        "left" => {
            if *cursor > 0 {
                *cursor -= 1;
            }
            true
        }
        "right" => {
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
        _ => {
            if key.chars().count() == 1 {
                let ch = key.chars().next().unwrap();
                if !ch.is_control() {
                    let mut chars: Vec<char> = buffer.chars().collect();
                    chars.insert(*cursor, ch);
                    *buffer = chars.into_iter().collect();
                    *cursor += 1;
                    return true;
                }
            }
            false
        }
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

impl MainWindow {
    pub fn new(state: AppState, cx: &mut Context<Self>) -> Self {
        let metrics_rx = crate::utils::SystemMonitor::spawn_background_monitor();
        let text_focus = cx.focus_handle();
        let subtitle_search_focus = cx.focus_handle();
        let api_base_focus = cx.focus_handle();
        let api_model_focus = cx.focus_handle();
        let api_key_focus = cx.focus_handle();
        let api_base_input = state.config.translate.api_base.clone();
        let api_model_input = state.config.translate.api_model.clone();
        let api_key_input = state.config.translate.api_key.clone();
        // 预览框宽度：配置里存过就用存的，否则 `preview_box_w` 留空，
        // 由 `MainWindow::subtitle_box_w()` 按「单行最大字数 × 预览字号」自动推算
        let preview_box_w_initial = state.config.subtitle_style.preview_box_w;
        // 导出内容模式是长期偏好：从配置恢复，避免每次重启都跳回默认「双语」。
        let export_mode_initial = state.export_mode_from_config();
        // 启动即绑定全局子进程作业对象：此后所有转写/转码/预览子进程都挂在它名下，
        // 本进程一旦消失（正常退出、关窗、崩溃、被任务管理器结束），Windows 会连根
        // 清掉整棵进程树。这是唯一能覆盖「父进程被强杀」的兜底 —— 单靠退出路径里的
        // kill 逻辑在强杀场景下根本没有机会执行。非 Windows 平台为空实现。
        crate::utils::child_registry::ensure_job_object();
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
            notice: None,
            translate_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            model_download_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            editor_export_format: EditorExportFormat::default(),
            editor_export_mode: export_mode_initial,
            glossary_status: None,
            is_export_dropdown_open: false,
            preview_box_w: preview_box_w_initial,
            preview_drag: None,
            subtitle_panel: EditorSubtitlePanel::default(),
            text_cursor_pos: 0,
            cached_live_image: None,
            subtitle_list_scroll: UniformListScrollHandle::new(),
            subtitle_list_followed_sel: None,
            library_thumbs: HashMap::new(),
            library_thumb_inflight: HashSet::new(),
            library_sizes: HashMap::new(),
            subtitle_search: String::new(),
            subtitle_search_focus,
            subtitle_search_focused: false,
            subtitle_search_cursor: 0,
            subtitle_filter: Vec::new(),
            subtitle_filter_key: None,
            glossary_bad_cache: None,
            api_base_input,
            api_model_input,
            api_key_input,
            api_base_focus,
            api_model_focus,
            api_key_focus,
            focused_api_field: None,
            line_edit_cursor: 0,
            api_key_visible: false,
            is_probing_translate: false,
            translate_probe_msg: None,
            library_selected: HashSet::new(),
            library_export_busy: false,
            command_palette: None,
        };

        // 若启动已载入历史视频工程，立即触发首帧提取，并按硬件策略补代理
        if window.state.selected_file.is_some() {
            window.trigger_extract_frame(cx);
            window.ensure_preview_proxy(cx);
            window.ensure_waveform(cx);
        }

        window
    }

    /// 关窗前的脏数据守卫（系统关窗路径 Alt+F4 / 任务栏右键 / 系统关闭按钮，
    /// 与自绘标题栏的关闭按钮共用同一实现）。
    ///
    /// `segments_dirty` 的落库是去抖的（切句 / 跳转 / 播放 / 导出时才 flush），
    /// 用户改完字幕直接关窗会丢掉最后一段未落库的编辑，所以这里先同步 flush 一次。
    /// 只在「本次确实尝试保存了脏数据、且保存失败」时拦下关窗：若 `db_write_error`
    /// 是更早一次失败留下的旧值，不能拿它挡住一次与保存无关的正常关闭。
    ///
    /// 返回 `true` 表示允许关窗。`components/titlebar.rs` 的关闭按钮直接调用本函数，
    /// 两条关窗路径收敛到这里，不再各写一份判定。
    pub(crate) fn flush_segments_before_close(&mut self, cx: &mut Context<Self>) -> bool {
        let had_dirty = self.state.segments_dirty;
        self.state.flush_segments_if_dirty();
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
    pub(crate) fn refresh_subtitle_filter(&mut self) {
        let key = (self.subtitle_search.clone(), self.state.segments_revision);
        // 缓存除了要求键不变，还必须确认这串下标仍落在当前片段表内（见
        // [`filter_cache_usable`]）。撤销/重做会整体替换 `segments` 并重排序号，
        // 沿用越界下标会让虚拟列表量不到行高，整片清单塌成空白且刷不出来。
        if self.subtitle_filter_key.as_ref() == Some(&key)
            && filter_cache_usable(&self.subtitle_filter, self.state.segments.len())
        {
            return;
        }
        let mut matched = matched_indices(&self.state.segments, &self.subtitle_search);
        // 兜底：万一匹配结果本身越界（片段表在别处被换过），宁可退成「不过滤」，
        // 也不能把非法下标交给虚拟列表——空白列表比多显示几行难排查得多。
        let segment_count = self.state.segments.len();
        if !indices_cover_segments(&matched, segment_count) {
            matched = (0..segment_count).collect();
        }
        self.subtitle_filter = matched;
        self.subtitle_filter_key = Some(key);
    }

    /// 术语表疑似未命中的句下标（按 `(术语表文本, segments_revision)` 缓存）。
    ///
    /// 术语表为空时直接返回空（零开销）；否则每帧最多重算一次，而不是在虚拟列表的
    /// 逐行闭包里现算——那会对「可视行 × 术语条数」重复匹配，纯属浪费。
    pub(crate) fn cached_glossary_violations(&mut self) -> Vec<usize> {
        let key = (
            self.state.config.translate.glossary.clone(),
            self.state.segments_revision,
        );
        if let Some((cached_key, cached)) = self.glossary_bad_cache.as_ref() {
            if cached_key == &key {
                return cached.clone();
            }
        }
        let result = self.state.glossary_violations();
        self.glossary_bad_cache = Some((key, result.clone()));
        result
    }

    /// 把 API 配置输入框的编辑缓冲写回 `config.toml`。
    /// 逐键落盘一个几百字节的 TOML 成本可忽略，换来的是「改完即生效、不会丢」。
    pub(crate) fn commit_api_field(&mut self, field: ApiField) {
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
        }
        self.state.save_translate_config();
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
        };
        let is_focused = self.focused_api_field == Some(field);
        // 密钥默认掩码显示，避免录屏/截图把密钥带出去
        let masked = field == ApiField::Key && !self.api_key_visible;
        let raw = match field {
            ApiField::Base => self.api_base_input.clone(),
            ApiField::Model => self.api_model_input.clone(),
            ApiField::Key => self.api_key_input.clone(),
        };
        let char_count = raw.chars().count();
        let cursor = self.line_edit_cursor.min(char_count);
        let placeholder = match field {
            ApiField::Base => "https://api.deepseek.com/v1",
            ApiField::Model => "deepseek-chat",
            ApiField::Key => "sk-...（留空则读环境变量 VOICE2WORD_API_KEY）",
        };
        let display: String = if masked {
            "•".repeat(char_count)
        } else {
            raw
        };
        let before: String = display.chars().take(cursor).collect();
        let after: String = display.chars().skip(cursor).collect();

        primitives::text_input(is_focused, 200.0)
            .id(id)
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.focused_api_field = Some(field);
                    this.line_edit_cursor = match field {
                        ApiField::Base => this.api_base_input.chars().count(),
                        ApiField::Model => this.api_model_input.chars().count(),
                        ApiField::Key => this.api_key_input.chars().count(),
                    };
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let buffer = match field {
                    ApiField::Base => &mut this.api_base_input,
                    ApiField::Model => &mut this.api_model_input,
                    ApiField::Key => &mut this.api_key_input,
                };
                let cursor = &mut this.line_edit_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    this.commit_api_field(field);
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
            })
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

    /// 命令面板里当前是否还有可用的命令（供状态置灰与 Enter 判定复用）。
    ///
    /// 只读 `AppState` 上已有的状态位，不引入新的状态字段——面板只是「换个入口」
    /// 去调已有的方法，可用性判断必须和界面上那些按钮一致。
    fn palette_command_enabled(&self, command: PaletteCommand) -> bool {
        match command {
            // 打开文件与切主题没有任何前置条件
            PaletteCommand::OpenFile | PaletteCommand::ToggleTheme => true,
            // 与 `shortcuts::StartTranscription` 的挂载点同一套判断
            PaletteCommand::StartTranscription => {
                !matches!(self.state.status, ProcessStatus::Processing { .. })
                    && self.state.transcribe_file.is_some()
            }
            // 与 `request_cancel_processing` 的首行守卫一致
            PaletteCommand::CancelTranscription => {
                matches!(self.state.status, ProcessStatus::Processing { .. })
            }
            // 与 `perform_editor_export` 的守卫一致
            PaletteCommand::ExportCurrentFormat => !self.state.segments.is_empty(),
            // 与 `trigger_llm_translation` 的守卫一致
            PaletteCommand::StartTranslation => {
                !self.state.segments.is_empty() && !self.state.is_translating
            }
            // 与 `cancel_llm_translation` 的守卫一致
            PaletteCommand::CancelTranslation => self.state.is_translating,
            // 已经在这一页时不再重复切换，避免命令面板成了「无操作入口」
            PaletteCommand::SwitchToEditor => self.state.active_tab != WorkspaceTab::Editor,
            PaletteCommand::SwitchToGenerate => self.state.active_tab != WorkspaceTab::Generate,
            PaletteCommand::SwitchToLibrary => self.state.active_tab != WorkspaceTab::Library,
            PaletteCommand::SwitchToPerformance => {
                self.state.active_tab != WorkspaceTab::Performance
            }
            PaletteCommand::Undo => self.state.can_undo(),
            PaletteCommand::Redo => self.state.can_redo(),
        }
    }

    /// 执行一条命令面板命令。
    ///
    /// 每个分支都只调用 `MainWindow` 上已有的方法（与对应按钮/快捷键完全同源），
    /// 因此这里没有任何业务逻辑，也不该长出业务逻辑——新增命令时若发现需要在这里
    /// 写判断，说明缺的是 `actions.rs` 里的一个方法，而不是面板里的一段代码。
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
            PaletteCommand::ToggleTheme => self.state.toggle_theme(),
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
        }
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
        let current = state.selected.min(count - 1) as isize;
        let next = (current + delta).clamp(0, count as isize - 1) as usize;
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
        let command = PALETTE_COMMANDS[index].command;
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
        // 名字过滤是纯函数（见 `filter_commands`，有单测）；状态过滤在这里做，
        // 因为「哪条命令当前可用」要读 `AppState`，抽不进纯函数。
        let entries: Vec<(usize, bool)> = filter_commands(&query)
            .into_iter()
            .map(|index| {
                (
                    index,
                    self.palette_command_enabled(PALETTE_COMMANDS[index].command),
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
                let spec = &PALETTE_COMMANDS[index];
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
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
        self.subtitle_search_focused = search_focused;
        self.is_text_focused = text_editor_focused;
        self.focused_api_field = if api_base_focused {
            Some(ApiField::Base)
        } else if api_model_focused {
            Some(ApiField::Model)
        } else if api_key_focused {
            Some(ApiField::Key)
        } else {
            None
        };

        let tab = self.state.active_tab;
        // 布局随窗口宽度降级（窄窗口下剪辑台改为上下堆叠），这里取本帧的视口宽度下发
        let viewport_w = f32::from(window.viewport_size().width);

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(Theme::bg_app())
            .text_color(Theme::text_primary())
            // 全局文件拖放接收层：GPUI 把「从资源管理器拖文件进来」包装成一次内部
            // drag/drop，drag value 类型为 `Arc<ExternalPaths>`，挂在根节点上即可
            // 全窗口任意位置放下都能导入。
            .on_drop::<Arc<ExternalPaths>>(cx.listener(
                |this, paths: &Arc<ExternalPaths>, _window, cx| {
                    let dropped: Vec<PathBuf> = paths.paths().to_vec();
                    this.handle_dropped_files(&dropped, cx);
                },
            ))
            // 全局快捷键（F-017）：键位表在 `shortcuts::bind_default_keys` 注册，
            // 这里只负责把动作接到具体的界面行为上。
            //
            // 除 Esc / Ctrl+K 两个「面板自己的键」外，其余监听器一律先看
            // `command_palette` 是否打开并直接返回：面板打开时它是唯一的键盘入口，
            // 不拦这一刀的话，在面板里敲字会顺带触发底下的全局动作——Ctrl+Z 会静默
            // 撤销字幕编辑、Ctrl+F 会把焦点抢到搜索框上让面板从此收不到按键。
            .on_action(
                cx.listener(|this, _: &shortcuts::TogglePlayback, _window, cx| {
                    if this.command_palette.is_some() {
                        return;
                    }
                    // 转写期间预览解码会与 ASR 抢 CPU/内存带宽，此时不允许用快捷键起播
                    if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                        return;
                    }
                    this.toggle_play_preview(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::PrevSegment, _window, cx| {
                    if this.command_palette.is_some() {
                        return;
                    }
                    this.jump_prev_segment(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::NextSegment, _window, cx| {
                    if this.command_palette.is_some() {
                        return;
                    }
                    this.jump_next_segment(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::SeekBackward, _window, cx| {
                    if this.command_palette.is_some() {
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
                    if this.command_palette.is_some() {
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
                    if this.command_palette.is_some() {
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
                    if this.completion_dialog.take().is_some()
                        || this.benchmark_dialog.take().is_some()
                    {
                        cx.notify();
                        return;
                    }
                    if this.is_export_dropdown_open {
                        this.is_export_dropdown_open = false;
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
                    if this.command_palette.is_some() {
                        return;
                    }
                    this.perform_editor_export(cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &shortcuts::ToggleTheme, _window, cx| {
                    if this.command_palette.is_some() {
                        return;
                    }
                    this.state.toggle_theme();
                    cx.notify();
                }),
            )
            .on_action(cx.listener(|this, _: &shortcuts::Undo, _window, cx| {
                if this.command_palette.is_some() {
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
            .on_action(cx.listener(|this, _: &shortcuts::Redo, _window, cx| {
                if this.command_palette.is_some() {
                    return;
                }
                if this.state.redo() {
                    this.subtitle_list_followed_sel = None;
                    cx.notify();
                }
            }))
            .on_action(
                cx.listener(|this, _: &shortcuts::FocusSubtitleSearch, window, cx| {
                    if this.command_palette.is_some() {
                        return;
                    }
                    // 搜索框在剪辑台里，先切页再聚焦；若本帧尚未挂载，聚焦调用会静默失效，
                    // 用户再按一次即可，不会误伤其他状态。
                    this.state.active_tab = WorkspaceTab::Editor;
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
            // 1. 顶部自定义标题栏 (极简沉浸式，包含窗口拖拽与控制按钮)
            .child(self.render_titlebar(cx))
            // 1.5 全局失败提示条：`ProcessStatus::Failed` 的产生点遍布导出 / 预览 /
            // 视频库等各处，而底部状态栏只在「语音转写」页存在。失败时就地在这一条
            // 跨页常驻的横幅上显示，否则用户在其它页面只会看到「点了没反应」。
            // 非失败态渲染空元素，不影响任何现有布局。
            .child(self.render_error_banner(cx))
            // 字幕写库失败：与转写错误分开显示，因为它要常驻到用户处理为止
            .child(self.render_db_error_banner(cx))
            // 1.6 一次性中性提示条：成功类反馈（如「已删除该记录」）与错误条同处一列，
            // 但配色中性——操作成功不该被误读成出错。两者同时存在时错误条在上。
            .child(self.render_notice_banner(cx))
            // 2. 左中右专业工作台架构
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    // 左侧主导航侧边栏 (左中右之「左」)
                    .child(self.render_navigation_sidebar(cx))
                    // 中间与右侧根据当前工作台呈现
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .child(match tab {
                                WorkspaceTab::Editor => {
                                    self.render_editor_layout(viewport_w, cx).into_any_element()
                                }
                                WorkspaceTab::Generate => {
                                    self.render_generate_layout(cx).into_any_element()
                                }
                                WorkspaceTab::Library => {
                                    self.render_library_layout(cx).into_any_element()
                                }
                                WorkspaceTab::Performance => {
                                    self.render_performance_layout(cx).into_any_element()
                                }
                            }),
                    ),
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

#[cfg(test)]
mod tests {
    use super::{filter_cache_usable, filter_commands, PALETTE_COMMANDS};

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
        assert_eq!(filter_commands("").len(), PALETTE_COMMANDS.len());
        // 纯空白等价于空查询：面板里多敲了几个空格不该把列表清空
        assert_eq!(filter_commands("   ").len(), PALETTE_COMMANDS.len());
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
        let export_index = PALETTE_COMMANDS
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

    /// 返回的是 `PALETTE_COMMANDS` 的下标，且严格保持命令集顺序：
    /// 面板的选中行按该序列索引，顺序错乱会让 Enter 执行到另一条命令。
    #[test]
    fn filter_preserves_catalog_order_and_indices() {
        let all = filter_commands("");
        assert!(all.windows(2).all(|w| w[0] < w[1]), "下标必须递增");
        assert!(all.iter().all(|&i| i < PALETTE_COMMANDS.len()));

        let undo = filter_commands("undo");
        let redo = filter_commands("redo");
        let undo_index = PALETTE_COMMANDS
            .iter()
            .position(|spec| spec.zh == "撤销")
            .unwrap();
        let redo_index = PALETTE_COMMANDS
            .iter()
            .position(|spec| spec.zh == "重做")
            .unwrap();
        assert!(undo.contains(&undo_index));
        assert!(redo.contains(&redo_index));
        // 「undo」不该顺带把「重做」也匹配进来（别名没有互相包含）
        assert!(!undo.iter().any(|&i| PALETTE_COMMANDS[i].zh == "重做"));
    }

    /// 命令集本身的自洽性：英文别名一律小写（匹配前统一 `to_lowercase`，
    /// 混入大写会让 `en` 列在面板副标题里显示得不整齐），中文名不重复。
    #[test]
    fn catalog_aliases_are_wellformed() {
        let mut seen = std::collections::HashSet::new();
        for spec in PALETTE_COMMANDS {
            assert_eq!(
                spec.en,
                spec.en.to_lowercase(),
                "别名必须全小写: {}",
                spec.en
            );
            assert!(seen.insert(spec.zh), "中文名重复: {}", spec.zh);
        }
    }
}
