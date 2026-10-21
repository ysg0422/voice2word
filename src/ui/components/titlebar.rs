//! 顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::WorkspaceTab;

#[cfg(target_os = "windows")]
pub(crate) mod win_drag {
    #[link(name = "user32")]
    extern "system" {
        fn ReleaseCapture() -> i32;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn IsZoomed(hwnd: isize) -> i32;
        fn InvalidateRect(hwnd: isize, lpRect: *const (), bErase: i32) -> i32;
        fn UpdateWindow(hwnd: isize) -> i32;
    }

    /// GPUI 内部强制重绘窗口的消息 ID (WM_USER + 5)
    pub const WM_GPUI_FORCE_UPDATE_WINDOW: u32 = 0x0405;

    /// 强制窗口刷新客户区并驱动 GPUI DirectX 渲染层重新绘制全幅画面
    pub fn force_update_window(window: &mut gpui::Window) {
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
                InvalidateRect(hwnd, std::ptr::null(), 0);
                UpdateWindow(hwnd);
                PostMessageW(hwnd, WM_GPUI_FORCE_UPDATE_WINDOW, 0, 0);
            }
        }
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

    /// 切换窗口最大化与还原状态（向 Win32 消息循环派发标准的 WM_SYSCOMMAND 系统命令）。
    /// 走系统 WM_SYSCOMMAND 才能让系统正确计算工作区（WorkArea，避让任务栏）并保持与 DefWindowProc 完全同构，
    /// 绝不能再在后面调用 GPUI 的 zoom_window()（其内部硬编码了 ShowWindowAsync(SW_MAXIMIZE)，会把还原状态再次强行打回最大化）。
    pub fn toggle_maximize(window: &mut gpui::Window) {
        let raw = match raw_window_handle::HasWindowHandle::window_handle(window) {
            Ok(handle) => handle,
            Err(_) => {
                window.zoom_window();
                return;
            }
        };
        let raw_window_handle::RawWindowHandle::Win32(handle) = raw.as_raw() else {
            window.zoom_window();
            return;
        };
        let hwnd = handle.hwnd.get();
        if hwnd != 0 {
            unsafe {
                const WM_SYSCOMMAND: u32 = 0x0112;
                const SC_MAXIMIZE: usize = 0xF030;
                const SC_RESTORE: usize = 0xF120;
                let is_zoomed = IsZoomed(hwnd) != 0;
                let cmd = if is_zoomed { SC_RESTORE } else { SC_MAXIMIZE };
                PostMessageW(hwnd, WM_SYSCOMMAND, cmd, 0);
            }
        }
    }
}

impl MainWindow {
    /// 渲染顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)
    pub(crate) fn render_titlebar(
        &self,
        cx: &mut Context<Self>,
        is_maximized: bool,
    ) -> impl IntoElement {
        let max_icon = if is_maximized { "❐" } else { "▢" };
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
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_1_5))
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
                    .min_w(px(0.0))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_mouse_down(MouseButton::Left, |event, window, _| {
                        #[cfg(target_os = "windows")]
                        {
                            if event.click_count >= 2 {
                                win_drag::toggle_maximize(window);
                            } else {
                                win_drag::drag_window(window);
                            }
                        }
                    })
                    .child(self.render_top_search(cx)),
            )
            // 右侧：原生窗口控制按钮组 (最小化 / 最大化 / 关闭) 与检查更新按钮
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .h_full()
                    .child(self.render_titlebar_update_widget(cx))
                    // 最小化
                    .child(
                        primitives::titlebar_btn("—", false)
                            .id("titlebar-btn-min")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(|_, _, window, _| {
                                window.minimize_window();
                            })),
                    )
                    // 最大化 / 还原
                    .child(
                        primitives::titlebar_btn(max_icon, false)
                            .id("titlebar-btn-max")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(|_, _, window, cx| {
                                #[cfg(target_os = "windows")]
                                win_drag::toggle_maximize(window);
                                #[cfg(not(target_os = "windows"))]
                                window.zoom_window();
                                cx.notify();
                            })),
                    )
                    // 关闭
                    .child(
                        primitives::titlebar_btn("✕", true)
                            .id("titlebar-btn-close")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
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
                Theme::accent_blue()
            } else {
                Theme::border_mid()
            })
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .cursor_text()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
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
                    .child("搜索"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
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
                            .child("搜索字幕与译文…")
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
                    .gap(px(Theme::SPACE_1))
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
                            .px(px(Theme::SPACE_1))
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
                    .px(px(Theme::SPACE_1_5))
                    .py(px(Theme::SPACE_0_5))
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

    /// 标题栏右上角更新组件（检测新版、一键直连下载、打开新版）
    pub(crate) fn render_titlebar_update_widget(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_checking = self.state.update_check_busy;
        let is_downloading = self.state.update_download_busy;
        let downloaded = self.state.update_downloaded_path.is_some();
        let has_new_version = matches!(self.state.update_check_result, Some((true, _, _)));
        let is_latest = matches!(self.state.update_check_result, Some((false, _, _)));
        let progress = self.state.update_download_progress;
        let cur_ver = crate::utils::update_check::current_version();

        let new_ver = self
            .state
            .update_release_info
            .as_ref()
            .map(|r| r.version.as_str())
            .unwrap_or("");

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(Theme::SPACE_1))
            .pr(px(Theme::SPACE_2))
            .child(if downloaded {
                div()
                    .id("titlebar-update-ready-btn")
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded_md()
                    .bg(Theme::accent_mint())
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_on_accent())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::accent_mint_deep()))
                    .child("★ 新版就绪 (点击安装)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_modal_open = true;
                        cx.notify();
                    }))
                    .into_any_element()
            } else if is_downloading {
                div()
                    .id("titlebar-update-downloading-btn")
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded_md()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(Theme::accent_blue())
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::accent_blue())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::bg_hover()))
                    .child(format!("↓ 下载中 {:.0}%", progress * 100.0))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_modal_open = true;
                        cx.notify();
                    }))
                    .into_any_element()
            } else if has_new_version {
                div()
                    .id("titlebar-update-direct-btn")
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded_md()
                    .bg(Theme::accent_mint())
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_on_accent())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::accent_mint_deep()))
                    .child(if new_ver.is_empty() {
                        "↑ 发现新版本".to_string()
                    } else {
                        format!("↑ 发现新版 v{new_ver}")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_modal_open = true;
                        cx.notify();
                    }))
                    .into_any_element()
            } else if is_latest {
                div()
                    .id("titlebar-latest-btn")
                    .px(px(Theme::SPACE_2))
                    .py(px(Theme::SPACE_0_5))
                    .rounded_md()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(Theme::border_mid())
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                    .child(format!("✓ 最新版 (v{cur_ver})"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.check_for_update(cx);
                    }))
                    .into_any_element()
            } else {
                primitives::chip_clickable(
                    if is_checking {
                        "↻ 检查中…"
                    } else {
                        "↻ 检查更新"
                    },
                    false,
                    is_checking,
                )
                .id("titlebar-check-update-btn")
                .on_click(cx.listener(|this, _, _, cx| {
                    if !this.state.update_check_busy {
                        this.check_for_update(cx);
                    }
                }))
                .into_any_element()
            })
    }
}
