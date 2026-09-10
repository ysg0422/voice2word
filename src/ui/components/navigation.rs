//! 左侧主导航侧边栏

use gpui::prelude::*;
use gpui::*;

use crate::app::state::WorkspaceTab;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染左侧主导航侧边栏 (Left Navigation Sidebar: 左中右架构之「左」)
    pub(crate) fn render_navigation_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active_tab;
        let seg_count = self.state.segments.len();

        div()
            .id("app-navigation-sidebar")
            .w(px(180.0))
            .h_full()
            .bg(Theme::bg_sidebar())
            .border_r_1()
            .border_color(Theme::border())
            .p_3()
            .flex()
            .flex_col()
            .justify_between()
            // 顶部导航区
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    // 栏目标题
                    .child(
                        div()
                            .px_2()
                            .pb_1()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_muted())
                            .child("工作台"),
                    )
                    // 1. 剪辑校对
                    .child(
                        div()
                            .id("nav-tab-editor")
                            .h(px(36.0))
                            .px_3()
                            .rounded_lg()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap_2p5()
                            .bg(if active == WorkspaceTab::Editor {
                                rgb(0x282832)
                            } else {
                                rgba(0x00000000)
                            })
                            .border_1()
                            .border_color(if active == WorkspaceTab::Editor {
                                rgb(0x383848)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.5))
                            .font_weight(if active == WorkspaceTab::Editor {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Editor {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(move |s| {
                                if active != WorkspaceTab::Editor {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Editor;
                                this.trigger_extract_frame(cx);
                                cx.notify();
                            }))
                            .child("剪辑校对"),
                    )
                    // 2. 语音转写
                    .child(
                        div()
                            .id("nav-tab-generate")
                            .h(px(36.0))
                            .px_3()
                            .rounded_lg()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .bg(if active == WorkspaceTab::Generate {
                                rgb(0x282832)
                            } else {
                                rgba(0x00000000)
                            })
                            .border_1()
                            .border_color(if active == WorkspaceTab::Generate {
                                rgb(0x383848)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.5))
                            .font_weight(if active == WorkspaceTab::Generate {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Generate {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(move |s| {
                                if active != WorkspaceTab::Generate {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Generate;
                                cx.notify();
                            }))
                            .child("语音转写"),
                    )
                    // 3. 视频库
                    .child(
                        div()
                            .id("nav-tab-library")
                            .h(px(36.0))
                            .px_3()
                            .rounded_lg()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .bg(if active == WorkspaceTab::Library {
                                rgb(0x282832)
                            } else {
                                rgba(0x00000000)
                            })
                            .border_1()
                            .border_color(if active == WorkspaceTab::Library {
                                rgb(0x383848)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.5))
                            .font_weight(if active == WorkspaceTab::Library {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Library {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(move |s| {
                                if active != WorkspaceTab::Library {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Library;
                                this.state.refresh_recent_tasks();
                                cx.notify();
                            }))
                            .child("视频库"),
                    )
                    // 4. 性能设置
                    .child(
                        div()
                            .id("nav-tab-performance")
                            .h(px(36.0))
                            .px_3()
                            .rounded_lg()
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .bg(if active == WorkspaceTab::Performance {
                                rgb(0x282832)
                            } else {
                                rgba(0x00000000)
                            })
                            .border_1()
                            .border_color(if active == WorkspaceTab::Performance {
                                rgb(0x383848)
                            } else {
                                rgba(0x00000000)
                            })
                            .text_size(px(12.5))
                            .font_weight(if active == WorkspaceTab::Performance {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Performance {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(move |s| {
                                if active != WorkspaceTab::Performance {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Performance;
                                cx.notify();
                            }))
                            .child("性能设置"),
                    ),
            )
            // 底部操作与工程状态
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .id("nav-quick-export-btn")
                            .h(px(32.0))
                            .px_3()
                            .rounded_full()
                            .bg(if seg_count > 0 {
                                rgb(0x242430)
                            } else {
                                rgb(0x18181e)
                            })
                            .border_1()
                            .border_color(rgb(0x30303c))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(if seg_count > 0 {
                                Theme::accent_mint()
                            } else {
                                Theme::text_muted()
                            })
                            .hover(|s| s.bg(rgb(0x2e2e3c)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_subtitles(cx);
                            }))
                            .child("导出字幕"),
                    ),
            )
    }
}
