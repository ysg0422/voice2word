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
use std::sync::Arc;
use tokio::sync::watch;

use crate::app::state::{AppState, WorkspaceTab};
use theme::Theme;

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
