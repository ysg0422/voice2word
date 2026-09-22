//! Voice2Word GPUI 界面主视窗
//! 遵循 Codex / Zed 极简现代深色风格与模块化组件架构

pub mod actions;
pub mod components;
pub mod dialogs;
pub mod editor;
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

use crate::app::state::{AppState, WorkspaceTab};
use theme::Theme;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorExportFormat {
    #[default]
    JianYing,    // 剪映草稿
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
            Self::Srt,
            Self::Ass,
            Self::Fcpxml,
            Self::PremiereXml,
            Self::Txt,
            Self::Vtt,
        ]
    }
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
    pub(crate) editor_export_format: EditorExportFormat,
    pub(crate) is_export_dropdown_open: bool,
    pub(crate) is_subtitle_style_open: bool,
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
}

impl MainWindow {
    pub fn new(state: AppState, cx: &mut Context<Self>) -> Self {
        let metrics_rx = crate::utils::SystemMonitor::spawn_background_monitor();
        let text_focus = cx.focus_handle();
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
            editor_export_format: EditorExportFormat::default(),
            is_export_dropdown_open: false,
            is_subtitle_style_open: false,
            text_cursor_pos: 0,
            cached_live_image: None,
            subtitle_list_scroll: UniformListScrollHandle::new(),
            subtitle_list_followed_sel: None,
            library_thumbs: HashMap::new(),
            library_thumb_inflight: HashSet::new(),
        };

        // 若启动已载入历史视频工程，立即触发首帧提取，并按硬件策略补代理
        if window.state.selected_file.is_some() {
            window.trigger_extract_frame(cx);
            window.ensure_preview_proxy(cx);
        }

        window
    }
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.metrics_rx.has_changed().unwrap_or(false) {
            self.state.metrics = self.metrics_rx.borrow_and_update().clone();
        }

        let tab = self.state.active_tab;

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(Theme::bg_app())
            .text_color(Theme::text_primary())
            // 1. 顶部自定义标题栏 (极简沉浸式，包含窗口拖拽与控制按钮)
            .child(self.render_titlebar(cx))
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
                                WorkspaceTab::Editor => self.render_editor_layout(cx).into_any_element(),
                                WorkspaceTab::Generate => self.render_generate_layout(cx).into_any_element(),
                                WorkspaceTab::Library => self.render_library_layout(cx).into_any_element(),
                                WorkspaceTab::Performance => self.render_performance_layout(cx).into_any_element(),
                            }),
                    ),
            );

        let root = if let Some(info) = self.completion_dialog.clone() {
            root.child(self.render_completion_dialog(info, cx))
        } else {
            root
        };

        if let Some(info) = self.benchmark_dialog.clone() {
            root.child(self.render_benchmark_dialog(info, cx))
        } else {
            root
        }
    }
}
