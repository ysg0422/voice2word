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
            // 左侧：Logo + 应用名 + 四个工作台导航标签。
            //
            // 导航从原来的左侧 180px 竖栏改为横向标签条嵌在标题栏里：
            // 那 180px 全部还给内容区，右列（字幕配置 / 对照表 / 统计）因此明显变宽；
            // 且标签条直接嵌进 38px 高的标题栏，不额外占一行高度。
            // 应用名与副标题属于品牌信息，导航才是高频入口，故把它们压缩成一个 Logo + 紧凑文字。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
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
                            ),
                    )
                    // 分隔线：把品牌区与导航标签区分开
                    .child(
                        div()
                            .w(px(Theme::HAIRLINE))
                            .h(px(Theme::BADGE_H))
                            .bg(Theme::border_mid()),
                    )
                    .child(self.render_nav_tabs(cx)),
            )
            // 中间：居中最上方的搜索框 (支持 Ctrl+F 快捷键) + 两侧原生拖拽区
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
                    .child(self.render_top_search(cx)),
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

    /// 渲染位于界面正中顶部的全局搜索框 (支持 Ctrl+F 聚焦与即时过滤)
    pub(crate) fn render_top_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_focused = self.subtitle_search_focused;
        let focus = self.subtitle_search_focus.clone();
        let raw = self.subtitle_search.clone();
        let char_count = raw.chars().count();
        let cursor = self.subtitle_search_cursor.min(char_count);
        let before: String = raw.chars().take(cursor).collect();
        let after: String = raw.chars().skip(cursor).collect();
        let searching = !raw.trim().is_empty();
        let total = self.state.segments.len();
        let visible = self.subtitle_filter.len();

        div()
            .id("top-center-search-bar")
            .w(px(320.0))
            .h(px(26.0))
            .px(px(Theme::SPACE_2_5))
            .rounded(px(Theme::RADIUS_MD))
            .bg(Theme::bg_input())
            .border_1()
            .border_color(if is_focused {
                Theme::accent_mint()
            } else {
                Theme::border_mid()
            })
            .flex()
            .items_center()
            .gap_2()
            .cursor_text()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.state.active_tab != WorkspaceTab::Editor {
                        this.state.active_tab = WorkspaceTab::Editor;
                    }
                    this.subtitle_panel = crate::ui::EditorSubtitlePanel::Translate;
                    window.focus(&focus);
                    this.subtitle_search_focused = true;
                    this.subtitle_search_cursor = this.subtitle_search.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.subtitle_search.clear();
                    this.subtitle_search_cursor = 0;
                    cx.notify();
                    return;
                }
                let buffer = &mut this.subtitle_search;
                let cursor = &mut this.subtitle_search_cursor;
                if crate::ui::apply_line_edit(buffer, cursor, event, cx) {
                    cx.notify();
                }
            }))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if is_focused {
                        Theme::accent_mint()
                    } else {
                        Theme::text_muted()
                    })
                    .child("🔍"),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .child(if is_focused {
                        div()
                            .flex()
                            .items_center()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_primary())
                            .child(before)
                            .child(div().text_color(Theme::accent_mint()).child("▌"))
                            .child(after)
                    } else if char_count == 0 {
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_muted())
                            .child("搜索字幕与译文...")
                    } else {
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_primary())
                            .truncate()
                            .child(raw)
                    }),
            )
            .child(if searching {
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .font_family("Consolas")
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if visible == 0 {
                                Theme::accent_red()
                            } else {
                                Theme::text_secondary()
                            })
                            .child(format!("{}/{}", visible, total)),
                    )
                    .child(
                        div()
                            .id("top-search-clear-btn")
                            .px_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .hover(|s| s.text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.subtitle_search.clear();
                                this.subtitle_search_cursor = 0;
                                cx.notify();
                            }))
                            .child("✕"),
                    )
                    .into_any_element()
            } else {
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(Theme::bg_raised())
                    .border_1()
                    .border_color(Theme::border_mid())
                    .text_size(px(9.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_muted())
                    .child("Ctrl+F")
                    .into_any_element()
            })
    }
}
