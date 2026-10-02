//! 左侧主导航侧边栏

use gpui::prelude::*;
use gpui::*;

use super::super::MainWindow;
use super::super::theme::Theme;
use crate::app::state::WorkspaceTab;

/// 导航项高度。比常规控件（32）略高，因为它是页面级入口、需要更大的点击热区。
const NAV_ITEM_H: f32 = 34.0;
/// 导航项之间的纵向间距
const NAV_ITEM_GAP: f32 = Theme::SPACE_1;

impl MainWindow {
    /// 渲染左侧主导航侧边栏 (Left Navigation Sidebar: 左中右架构之「左」)
    pub(crate) fn render_navigation_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active_tab;
        let seg_count = self.state.segments.len();

        div()
            .id("app-navigation-sidebar")
            .w(px(Theme::NAV_W))
            .flex_shrink_0()
            .h_full()
            .bg(Theme::bg_sidebar())
            .border_r_1()
            .border_color(Theme::border())
            .p(px(Theme::SPACE_3))
            .flex()
            .flex_col()
            .justify_between()
            // 顶部导航区
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(NAV_ITEM_GAP))
                    // 栏目标题：与导航项左内边距对齐（px_3 的 12px）
                    .child(
                        div()
                            .h(px(Theme::CTRL_H_XS))
                            .px(px(Theme::SPACE_3))
                            .flex()
                            .items_center()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_muted())
                            .child("工作台"),
                    )
                    .child(self.nav_item(
                        "nav-tab-editor",
                        "剪辑校对",
                        active == WorkspaceTab::Editor,
                        cx,
                        |this, cx| {
                            this.state.active_tab = WorkspaceTab::Editor;
                            this.trigger_extract_frame(cx);
                            cx.notify();
                        },
                    ))
                    .child(self.nav_item(
                        "nav-tab-generate",
                        "语音转写",
                        active == WorkspaceTab::Generate,
                        cx,
                        |this, cx| {
                            this.state.active_tab = WorkspaceTab::Generate;
                            cx.notify();
                        },
                    ))
                    .child(self.nav_item(
                        "nav-tab-library",
                        "视频库",
                        active == WorkspaceTab::Library,
                        cx,
                        |this, cx| {
                            this.state.active_tab = WorkspaceTab::Library;
                            this.state.refresh_recent_tasks();
                            cx.notify();
                        },
                    ))
                    .child(self.nav_item(
                        "nav-tab-performance",
                        "性能设置",
                        active == WorkspaceTab::Performance,
                        cx,
                        |this, cx| {
                            this.state.active_tab = WorkspaceTab::Performance;
                            cx.notify();
                        },
                    )),
            )
            // 底部：工程摘要 + 深浅主题切换（主题偏好写入 config.toml，重启保持）
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .px(px(Theme::SPACE_3))
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_muted())
                            .child(if seg_count > 0 {
                                format!("当前工程 {seg_count} 句字幕")
                            } else {
                                "尚未载入字幕".to_string()
                            }),
                    )
                    .child(
                        div()
                            .id("nav-toggle-theme")
                            .h(px(NAV_ITEM_H))
                            .px(px(Theme::SPACE_3))
                            .rounded(px(Theme::RADIUS_MD))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(Theme::bg_raised())
                            .border_1()
                            .border_color(Theme::border_mid())
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.toggle_theme();
                                cx.notify();
                            }))
                            .child(if Theme::is_light() {
                                "切换到深色主题"
                            } else {
                                "切换到浅色主题"
                            }),
                    ),
            )
    }

    /// 单个导航项。迁移前四项是逐字复制的四段 45 行代码，只有 id/文案/目标页不同，
    /// 且第一项独有 `.gap_2p5`（另三项漏了）——一处改动要同步四处。
    ///
    /// 选中态用「左缘竖条 + 抬升底色」，而不是整块高亮：企业级导航（VS Code / Figma）
    /// 普遍用左缘指示条，扫描一眼就能定位当前页，且比整块反色更克制。
    fn nav_item(
        &self,
        id: &'static str,
        label: &'static str,
        selected: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .relative()
            .h(px(NAV_ITEM_H))
            .px(px(Theme::SPACE_3))
            .rounded(px(Theme::RADIUS_MD))
            .cursor_pointer()
            .flex()
            .items_center()
            .bg(if selected {
                Theme::bg_raised()
            } else {
                Theme::transparent()
            })
            .text_size(px(Theme::TEXT_BODY_LG))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .text_color(if selected {
                Theme::text_primary()
            } else {
                Theme::text_secondary()
            })
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(Theme::tint_neutral()).text_color(Theme::text_primary())
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
            }))
            // 选中指示条：2px 圆角竖条，贴左缘居中
            .child(if selected {
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(7.0))
                    .bottom(px(7.0))
                    .w(px(Theme::NAV_INDICATOR_W))
                    .rounded_full()
                    .bg(Theme::accent_mint())
                    .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(label)
    }
}
