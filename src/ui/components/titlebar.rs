//! 顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::WorkspaceTab;

#[cfg(target_os = "windows")]
mod win_drag {
    #[link(name = "user32")]
    extern "system" {
        fn ReleaseCapture() -> i32;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    }

    /// 按下标题栏时拖动窗口本体。
    /// 必须用自身窗口句柄；原先的 GetForegroundWindow 在焦点位于其他进程窗口时会拖错窗口。
    ///
    /// 必须用 PostMessageW 而非 SendMessageW：本函数运行在鼠标事件回调内，gpui 的 App
    /// RefCell 借用仍在栈上；SendMessageW 会同步进入 DefWindowProc 的模态拖拽循环并重入
    /// 消息泵，此刻被泵到的前台任务一旦调用 Entity::update 就会 panic "RefCell already
    /// borrowed"（转写事件泵每 80ms 入队一个任务，拖动窗口时极易命中）。PostMessage 把
    /// 模态循环推迟到主消息循环无借用时执行。
    pub fn drag_window(window: &mut gpui::Window) {
        let raw = match raw_window_handle::HasWindowHandle::window_handle(window) {
            Ok(handle) => handle,
            Err(_) => return,
        };
        let raw_window_handle::RawWindowHandle::Win32(handle) = raw.as_raw() else {
            return;
        };
        let hwnd = handle.hwnd.get();
        if hwnd != 0 {
            unsafe {
                ReleaseCapture();
                const WM_NCLBUTTONDOWN: u32 = 0x00A1;
                const HTCAPTION: usize = 2;
                PostMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
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
            .h(px(Theme::TITLEBAR_H))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .justify_between()
            .px(px(Theme::CARD_PAD_SM))
            // 左侧：Logo 与应用标题
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(Theme::BADGE_H))
                            .h(px(Theme::BADGE_H))
                            .rounded(px(Theme::RADIUS_MD))
                            .bg(Theme::accent_mint())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_on_accent())
                            .child("V"),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_primary())
                            .child("Voice2Word"),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
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
                    .on_mouse_down(MouseButton::Left, |_, window, _| {
                        #[cfg(target_os = "windows")]
                        win_drag::drag_window(window);
                    })
                    .child(div().flex().items_center().gap_2().children(
                        if let Some(name) = current_file_name {
                            vec![
                                div()
                                    .w(px(Theme::DOT_SM))
                                    .h(px(Theme::DOT_SM))
                                    .rounded_full()
                                    .bg(Theme::accent_mint())
                                    .into_any_element(),
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_secondary())
                                    .child(name.to_string())
                                    .into_any_element(),
                            ]
                        } else {
                            vec![div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child("未载入媒体")
                                .into_any_element()]
                        },
                    )),
            )
            // 右侧：原生窗口控制按钮组 (最小化 / 最大化 / 关闭)
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    // 最小化
                    .child(
                        primitives::titlebar_btn("—", false)
                            .id("titlebar-btn-min")
                            .on_click(cx.listener(|_, _, window, _| {
                                window.minimize_window();
                            })),
                    )
                    // 最大化 / 还原
                    .child(
                        primitives::titlebar_btn("▢", false)
                            .id("titlebar-btn-max")
                            .on_click(cx.listener(|_, _, window, _| {
                                window.zoom_window();
                            })),
                    )
                    // 关闭
                    .child(
                        primitives::titlebar_btn("✕", true)
                            .id("titlebar-btn-close")
                            .on_click(cx.listener(|this, _, window, cx| {
                                // 关窗前的脏数据守卫：与系统关窗路径（Alt+F4 / 任务栏右键 /
                                // 系统关闭按钮，见 `install_window_close_guard`）共用同一份
                                // 实现，避免两套关窗逻辑漂移。`false` = 本次落库失败，已由
                                // 该实现记好日志并触发重绘，这里直接留着窗口让用户处理。
                                if !this.flush_segments_before_close(cx) {
                                    return;
                                }
                                window.remove_window();
                                cx.quit();
                            })),
                    ),
            )
    }
}
