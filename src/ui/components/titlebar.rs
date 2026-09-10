//! 顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)

use gpui::prelude::*;
use gpui::*;

use crate::app::state::WorkspaceTab;
use super::super::theme::Theme;
use super::super::MainWindow;

#[cfg(target_os = "windows")]
mod win_drag {
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn ReleaseCapture() -> i32;
        fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    pub fn drag_window() {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd != 0 {
                ReleaseCapture();
                const WM_NCLBUTTONDOWN: u32 = 0x00A1;
                const HTCAPTION: usize = 2;
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
            }
        }
    }
}

impl MainWindow {
    /// 渲染顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)
    pub(crate) fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_file_name = match self.state.active_tab {
            WorkspaceTab::Generate => self.state.transcribe_file.as_ref().map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("未命名文件")
            }),
            WorkspaceTab::Editor => self.state.selected_file.as_ref().map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("未命名文件")
            }),
            WorkspaceTab::Library => None,
            WorkspaceTab::Performance => None,
        };

        div()
            .id("custom-titlebar")
            .w_full()
            .h(px(38.0))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            // 左侧：Logo 与应用标题
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded_md()
                            .bg(Theme::accent_mint())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0x09090b))
                            .child("V"),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_primary())
                            .child("Voice2Word"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_muted())
                            .child("· 智能字幕与音视频工作台"),
                    ),
            )
            // 中间：当前打开的文件名或状态 + 原生无边框拖拽响应区
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |_, _, _| {
                        #[cfg(target_os = "windows")]
                        win_drag::drag_window();
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .children(if let Some(name) = current_file_name {
                                vec![
                                    div()
                                        .w(px(6.0))
                                        .h(px(6.0))
                                        .rounded_full()
                                        .bg(Theme::accent_mint())
                                        .into_any_element(),
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(Theme::text_secondary())
                                        .child(name.to_string())
                                        .into_any_element(),
                                ]
                            } else {
                                vec![
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(Theme::text_muted())
                                        .child("未载入媒体")
                                        .into_any_element(),
                                ]
                            }),
                    ),
            )
            // 右侧：原生窗口控制按钮组 (最小化 / 最大化 / 关闭)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    // 最小化
                    .child(
                        div()
                            .id("titlebar-btn-min")
                            .w(px(40.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_, _, window, _| {
                                window.minimize_window();
                            }))
                            .child("—"),
                    )
                    // 最大化 / 还原
                    .child(
                        div()
                            .id("titlebar-btn-max")
                            .w(px(40.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_, _, window, _| {
                                window.zoom_window();
                            }))
                            .child("▢"),
                    )
                    // 关闭
                    .child(
                        div()
                            .id("titlebar-btn-close")
                            .w(px(44.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(rgb(0xe11d48)).text_color(rgb(0xffffff)))
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.remove_window();
                                cx.quit();
                            }))
                            .child("✕"),
                    ),
            )
    }
}
