//! 顶部主导航：四个工作台入口（横向标签条，嵌在标题栏左侧）
//!
//! # 为什么从左侧栏改成顶部标签条
//!
//! 迁移前导航是左侧 180px 竖栏，横向吃掉 180px——右列（字幕配置 / 对照表 / 统计）
//! 因此常年偏窄。改成顶部横向标签条后，这 180px 全部还给内容区，右侧空间明显变宽，
//! 且不再额外占一行高度（标签条直接嵌进 38px 高的标题栏里）。

use gpui::prelude::*;
use gpui::*;

use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::WorkspaceTab;

/// 顶部导航标签的高度。略低于标题栏，上下留 2px 呼吸。
const NAV_TAB_H: f32 = 28.0;

impl MainWindow {
    /// 渲染标题栏左侧的横向导航标签条（剪辑校对 / 语音转写 / 视频库 / 性能设置）。
    ///
    /// 选中态用「抬升底色 + 薄荷色文字 + 底部指示条」：横向标签里底部指示条比左缘竖条
    /// 更贴合「当前页」的阅读方向，也和顶部工具栏的视觉语言一致。
    pub(crate) fn render_nav_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active_tab;

        div()
            .id("app-nav-tabs")
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_1))
            .child(self.nav_tab(
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
            .child(self.nav_tab(
                "nav-tab-generate",
                "语音转写",
                active == WorkspaceTab::Generate,
                cx,
                |this, cx| {
                    this.state.active_tab = WorkspaceTab::Generate;
                    cx.notify();
                },
            ))
            .child(self.nav_tab(
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
            .child(self.nav_tab(
                "nav-tab-performance",
                "性能设置",
                active == WorkspaceTab::Performance,
                cx,
                |this, cx| {
                    this.state.active_tab = WorkspaceTab::Performance;
                    cx.notify();
                },
            ))
    }

    /// 单个顶部导航标签。四项只有 id / 文案 / 目标页不同，抽成一个函数避免四处复制。
    fn nav_tab(
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
            .h(px(NAV_TAB_H))
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
            .text_size(px(Theme::TEXT_BODY))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .text_color(if selected {
                Theme::accent_mint()
            } else {
                Theme::text_secondary()
            })
            .hover(move |s| {
                if selected {
                    s
                } else {
                    s.bg(Theme::tint_neutral())
                        .text_color(Theme::text_primary())
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
            }))
            // 选中指示条：底部 2px 圆角横条
            .child(if selected {
                div()
                    .absolute()
                    .left(px(Theme::SPACE_2))
                    .right(px(Theme::SPACE_2))
                    .bottom(px(0.0))
                    .h(px(Theme::NAV_INDICATOR_W))
                    .rounded_full()
                    .bg(Theme::accent_mint())
                    .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(label)
    }
}
