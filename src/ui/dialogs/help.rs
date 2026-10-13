//! 快捷键速查面板 (F1)
//!
//! # 为什么要有这个面板
//!
//! 快捷键此前只有散落在界面各处的零碎提示（状态栏、按钮 title），用户在剪辑台里
//! 想确认「删除当前句到底是哪个键」时无处可查；键位表 [`shortcuts::SHORTCUT_SPECS`]
//! 又只活在代码里。F1 面板把那张表整块摊开给用户，且**内容与分组直接由键位表推导**
//! （见 [`shortcuts::grouped_shortcut_rows`]），所以面板上写的就是真正注册的键位，
//! 不会出现「照着面板按却没反应」。
//!
//! 本文件只做排版：分组、顺序、去重都在 `shortcuts` 模块里（纯函数、有单测覆盖）。

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::shortcuts;
use super::super::theme::Theme;
use super::super::MainWindow;
use super::keyboard;

/// 键位列的宽度。
///
/// 定宽（而不是让键位文字自己撑开）是为了让各行的说明文字左边界对齐成一条竖线——
/// 面板里是「键位 → 说明」的对照表，参差不齐的说明列会让眼睛失去参照。
const KEY_COL_W: f32 = 96.0;

impl MainWindow {
    /// 渲染快捷键速查面板。
    ///
    /// 两列排布：全部键位平铺成一列时卡片高度接近半个窗口（遮罩下容易顶出可视区），
    /// 按「前一半 / 后一半」切成两列后高度减半，同时保持分组顺序——阅读顺序是先
    /// 左列自上而下、再右列自上而下。
    pub(crate) fn render_shortcuts_help(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // 键盘支持：注册全局按键拦截（幂等），取回面板 + 关闭按钮的焦点句柄。
        // 本面板的 Esc 由 `ui/mod.rs` 的 `CancelOrClose` 链覆盖（`shortcuts_help_open`
        // 那一刀，排在命令面板之后），所以下面不再挂 Esc 分支。
        keyboard::ensure_handler(cx);
        let panel_focus = keyboard::panel_focus(keyboard::DialogKind::ShortcutsHelp, cx);
        let close_focus = keyboard::button_focus(
            keyboard::DialogKind::ShortcutsHelp,
            0,
            keyboard::DialogAction::ShortcutsHelpClose,
            cx,
        );

        let groups = shortcuts::grouped_shortcut_rows();
        let mid = groups.len().div_ceil(2);
        let (left_groups, right_groups) = groups.split_at(mid);

        let column = |entries: &[shortcuts::ShortcutGroup]| -> Div {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(Theme::SPACE_3))
                .children(entries.iter().map(|(group, rows)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::SPACE_1))
                        .child(primitives::section_title(*group))
                        .children(rows.iter().map(|(key, hint)| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_2))
                                .child(
                                    div()
                                        .w(px(KEY_COL_W))
                                        .flex_shrink_0()
                                        .text_size(px(Theme::TEXT_SMALL))
                                        // 键位用主文字色、说明用次级色：扫一眼面板时
                                        // 视线落在键位上，说明只是注解。
                                        .text_color(Theme::text_primary())
                                        .child(*key),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .text_color(Theme::text_secondary())
                                        .child(*hint),
                                )
                        }))
                }))
        };

        primitives::modal_scrim()
            .id("shortcuts-help-backdrop")
            // 点遮罩 = 关闭（与按 Esc / F1 同义，符合直觉）
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_shortcuts_help(cx);
            }))
            .child(
                primitives::modal_card(Theme::DIALOG_W_WIDE, Theme::PAGE_PAD)
                    .id("shortcuts-help-card")
                    .track_focus(&panel_focus)
                    // 卡片必须 `occlude()`，把遮罩从命中集合里挡掉；只挂一个空的
                    // on_click 不够——GPUI 的点击会沿命中链冒泡，空回调不中断冒泡，
                    // 点正文照样会触发遮罩的「点空白关闭」（与确认框同一个坑）。
                    .occlude()
                    .on_click(cx.listener(|_, _, _, _cx| {}))
                    .gap(px(Theme::SPACE_3))
                    .child(primitives::page_title("快捷键"))
                    .child(div().flex().gap(px(Theme::SPACE_5)).children([
                        column(left_groups).into_any_element(),
                        column(right_groups).into_any_element(),
                    ]))
                    .child({
                        // 裸键的生效条件必须写在面板上：用户看到「Space 播放/暂停」
                        // 却在搜索框里打不出空格，正是少这一句说明造成的困惑。
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child("裸键（Space / Delete / Backspace）只在焦点不在输入框时生效，打字时不会被抢走")
                    })
                    .child(
                        div().flex().justify_end().child(
                            primitives::btn_clickable(
                                "关闭",
                                primitives::BtnSize::Md,
                                primitives::BtnVariant::Secondary,
                            )
                            .id("shortcuts-help-close")
                            .track_focus(&close_focus)
                            .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_shortcuts_help(cx);
                            })),
                        ),
                    ),
            )
    }
}
