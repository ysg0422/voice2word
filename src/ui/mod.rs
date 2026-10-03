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
use crate::subtitle::{indices_cover_segments, matched_indices};
use theme::Theme;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorExportFormat {
    #[default]
    JianYing,    // 剪映草稿
    JianYingFolder, // 剪映草稿（导出到自选文件夹，不写入本机草稿库）
    Srt,         // SRT 字幕
    Ass,         // ASS 特效字幕
    Fcpxml,      // FCPXML (达芬奇 / FCP)
    PremiereXml, // Premiere XML
    Txt,         // TXT 纯文本
    Vtt,         // VTT 网页字幕
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
    pub(crate) editor_export_format: EditorExportFormat,
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
            editor_export_format: EditorExportFormat::default(),
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
            subtitle_search: String::new(),
            subtitle_search_focus,
            subtitle_search_focused: false,
            subtitle_search_cursor: 0,
            subtitle_filter: Vec::new(),
            subtitle_filter_key: None,
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
        };

        // 若启动已载入历史视频工程，立即触发首帧提取，并按硬件策略补代理
        if window.state.selected_file.is_some() {
            window.trigger_extract_frame(cx);
            window.ensure_preview_proxy(cx);
            window.ensure_waveform(cx);
        }

        window
    }

    /// 按当前搜索关键字重算字幕清单的可见行下标。
    ///
    /// 结果按 `(关键字, segments_revision)` 缓存：播放时界面每 40ms 重绘一次，
    /// 若每帧都对上千条字幕重跑子串匹配，滚动与播放都会白白掉帧。
    pub(crate) fn refresh_subtitle_filter(&mut self) {
        let key = (self.subtitle_search.clone(), self.state.segments_revision);
        // 缓存除了要求键不变，还必须确认这串下标仍落在当前片段表内。
        // 撤销/重做会整体替换 `segments`，而缓存的键未必跟着变；一旦沿用越界下标，
        // 虚拟列表在取不到行时会量出行高 0（`uniform_list` 只拿第 0 行量高度），
        // 整片清单塌成空白，且键不变就一直空着刷不出来——用户看到的正是「字幕没了」。
        if self.subtitle_filter_key.as_ref() == Some(&key)
            && indices_cover_segments(&self.subtitle_filter, self.state.segments.len())
        {
            return;
        }
        let mut matched = matched_indices(&self.state.segments, &self.subtitle_search);
        // 兜底：万一匹配结果本身越界（片段表在别处被换过），宁可退成「不过滤」，
        // 也不能把非法下标交给虚拟列表——空白列表比多显示几行难排查得多。
        if !indices_cover_segments(&matched, self.state.segments.len()) {
            matched = (0..self.state.segments.len()).collect();
        }
        self.subtitle_filter = matched;
        self.subtitle_filter_key = Some(key);
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
            .on_action(cx.listener(|this, _: &shortcuts::TogglePlayback, _window, cx| {
                // 转写期间预览解码会与 ASR 抢 CPU/内存带宽，此时不允许用快捷键起播
                if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                    return;
                }
                this.toggle_play_preview(cx);
            }))
            .on_action(cx.listener(|this, _: &shortcuts::PrevSegment, _window, cx| {
                this.jump_prev_segment(cx);
            }))
            .on_action(cx.listener(|this, _: &shortcuts::NextSegment, _window, cx| {
                this.jump_next_segment(cx);
            }))
            .on_action(cx.listener(|this, _: &shortcuts::SeekBackward, _window, cx| {
                if this.state.segments.is_empty() {
                    return;
                }
                this.halt_preview_playback();
                let target = (this.state.current_time - 1.0).max(0.0);
                this.state.seek_to(target);
                this.trigger_extract_frame(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &shortcuts::SeekForward, _window, cx| {
                if this.state.segments.is_empty() {
                    return;
                }
                this.halt_preview_playback();
                let target = this.state.current_time + 1.0;
                this.state.seek_to(target);
                this.trigger_extract_frame(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &shortcuts::StartTranscription, _window, cx| {
                if matches!(this.state.status, ProcessStatus::Processing { .. }) {
                    return;
                }
                if this.state.transcribe_file.is_none() {
                    return;
                }
                this.start_processing(cx);
            }))
            .on_action(cx.listener(|this, _: &shortcuts::CancelOrClose, window, cx| {
                // 优先级：浮层 → 导出下拉 → 搜索框 → 终止任务。Esc 在没有可取消对象时不应有任何副作用。
                if this.completion_dialog.take().is_some() || this.benchmark_dialog.take().is_some() {
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
            }))
            .on_action(cx.listener(|this, _: &shortcuts::ExportSubtitle, _window, cx| {
                this.perform_editor_export(cx);
            }))
            .on_action(cx.listener(|this, _: &shortcuts::ToggleTheme, _window, cx| {
                this.state.toggle_theme();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &shortcuts::Undo, _window, cx| {
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
                if this.state.redo() {
                    this.subtitle_list_followed_sel = None;
                    cx.notify();
                }
            }))
            .on_action(
                cx.listener(|this, _: &shortcuts::FocusSubtitleSearch, window, cx| {
                    // 搜索框在剪辑台里，先切页再聚焦；若本帧尚未挂载，聚焦调用会静默失效，
                    // 用户再按一次即可，不会误伤其他状态。
                    this.state.active_tab = WorkspaceTab::Editor;
                    this.subtitle_search_focused = true;
                    this.subtitle_search_cursor = this.subtitle_search.chars().count();
                    window.focus(&this.subtitle_search_focus);
                    cx.notify();
                }),
            )
            // 1. 顶部自定义标题栏 (极简沉浸式，包含窗口拖拽与控制按钮)
            .child(self.render_titlebar(cx))
            // 1.5 全局失败提示条：`ProcessStatus::Failed` 的产生点遍布导出 / 预览 /
            // 视频库等各处，而底部状态栏只在「语音转写」页存在。失败时就地在这一条
            // 跨页常驻的横幅上显示，否则用户在其它页面只会看到「点了没反应」。
            // 非失败态渲染空元素，不影响任何现有布局。
            .child(self.render_error_banner(cx))
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
                                WorkspaceTab::Editor => self.render_editor_layout(viewport_w, cx).into_any_element(),
                                WorkspaceTab::Generate => self.render_generate_layout(cx).into_any_element(),
                                WorkspaceTab::Library => self.render_library_layout(cx).into_any_element(),
                                WorkspaceTab::Performance => self.render_performance_layout(cx).into_any_element(),
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

        // 二次确认弹窗排在最后：它是阻塞式的，应盖在其它弹窗之上
        if let Some(info) = self.confirm_dialog.clone() {
            root.child(self.render_confirm_dialog(info, cx))
        } else {
            root
        }
    }
}
