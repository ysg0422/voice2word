//! 首次打开软件硬件引导对话框（我有独立显卡 / 我没有独立显卡 向导）

use gpui::prelude::*;
use gpui::*;

use super::keyboard;
use crate::ui::primitives;
use crate::ui::theme::Theme;
use crate::ui::MainWindow;

/// Tab 停靠点数量（= 渲染里用到的 index 数）。
pub(crate) const HW_SETUP_TAB_COUNT: usize = 5;

/// 硬件引导框 Tab 停靠点顺序（下标 ↔ 动作）；渲染按 index 取句柄。
///
/// 顺序即实际 Tab 顺序：GPU 卡片 → CPU 卡片 →「我有独立显卡」按钮 →
/// 「我没有独立显卡」按钮 →「稍后再说」。
///
/// 下标必须与 [`keyboard::button_focus`] 的 `index` 严格一一对应：该函数在**首次创建**句柄时
/// 就把 action 绑死，同一个 index 之后不会再改 action——要重排 Tab，只能改这张表，
/// 不能改渲染里的下标（否则同 index 会先后拿到两个不同 action）。
pub(crate) const HW_SETUP_TAB_ACTIONS: [keyboard::DialogAction; HW_SETUP_TAB_COUNT] = [
    keyboard::DialogAction::HardwareSelectGpu, // 0 GPU 卡片
    keyboard::DialogAction::HardwareSelectCpu, // 1 CPU 卡片
    keyboard::DialogAction::HardwareSelectGpu, // 2「我有独立显卡」按钮
    keyboard::DialogAction::HardwareSelectCpu, // 3「我没有独立显卡」按钮
    keyboard::DialogAction::HardwareDefer,     // 4「稍后再说」
];

impl MainWindow {
    /// 首次启动硬件选择对话框：让用户一键选择「我有独立显卡」或「我没有独立显卡」
    pub(crate) fn render_hardware_setup_modal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_gpu = self.state.config.gpu.is_gpu_tier();

        // 键盘支持：注册全局按键拦截（幂等），取回面板 + 五个 Tab 停靠点
        // （两张卡片 + 卡片内的两个按钮 +「稍后再说」）的焦点句柄。
        // 回车 = 焦点所在停靠点的动作，不在停靠点上时 = 当前档位（见 `keyboard::primary_action`）；
        // Esc 由 `ui/mod.rs` 的 `CancelOrClose` 链覆盖（未完成引导那一刀，与
        // 「稍后再说」同义），所以下面不挂 Esc 分支。
        keyboard::ensure_handler(cx);
        let panel_focus = keyboard::panel_focus(keyboard::DialogKind::HardwareSetup, cx);
        // index 与 action 全部取自 `HW_SETUP_TAB_ACTIONS`：那张表是唯一真相，重排 Tab
        // 只能改表、不能改这里的下标（`button_focus` 首次创建句柄时就把 action 绑死）。
        // 每个停靠点再钉一个 `tab_index`：两张卡片的按钮嵌在卡片内部，只看元素树文档顺序，
        // Tab 会在「卡片自己的按钮」和「下一张卡片」之间穿插；`tab_index` 把实际顺序固定成
        // GPU 卡片 → CPU 卡片 →「我有独立显卡」按钮 →「我没有独立显卡」按钮 →「稍后再说」。
        let gpu_card_focus = keyboard::button_focus(
            keyboard::DialogKind::HardwareSetup,
            0,
            HW_SETUP_TAB_ACTIONS[0],
            cx,
        )
        .tab_index(0);
        let cpu_card_focus = keyboard::button_focus(
            keyboard::DialogKind::HardwareSetup,
            1,
            HW_SETUP_TAB_ACTIONS[1],
            cx,
        )
        .tab_index(1);
        let gpu_focus = keyboard::button_focus(
            keyboard::DialogKind::HardwareSetup,
            2,
            HW_SETUP_TAB_ACTIONS[2],
            cx,
        )
        .tab_index(2);
        let cpu_focus = keyboard::button_focus(
            keyboard::DialogKind::HardwareSetup,
            3,
            HW_SETUP_TAB_ACTIONS[3],
            cx,
        )
        .tab_index(3);
        let later_focus = keyboard::button_focus(
            keyboard::DialogKind::HardwareSetup,
            4,
            HW_SETUP_TAB_ACTIONS[4],
            cx,
        )
        .tab_index(4);

        primitives::modal_scrim()
            .id("hardware-setup-scrim")
            .occlude()
            .child(
                primitives::modal_card(680.0, Theme::PAGE_PAD)
                    .id("hardware-setup-card")
                    // 焦点容器：Tab 先落到卡片，再在三个按钮之间循环。
                    .track_focus(&panel_focus)
                    .gap(px(Theme::SPACE_4))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_1))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_TITLE))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("欢迎使用 Voice2Word · 硬件模式配置"),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child("请根据你的设备硬件类型选择运行模式，系统将自动配置推理策略与模型方案："),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(Theme::SPACE_4))
                            .w_full()
                            // 卡片 1：我有独立显卡（GPU 加速）
                            .child(
                                div()
                                    .id("hardware-choice-gpu")
                                    .track_focus(&gpu_card_focus)
                                    .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .justify_between()
                                    .p(px(Theme::SPACE_4))
                                    .rounded(px(Theme::RADIUS_LG))
                                    .bg(if is_gpu {
                                        Theme::tint_mint_badge()
                                    } else {
                                        Theme::bg_panel()
                                    })
                                    .border_1()
                                    .border_color(if is_gpu {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::border()
                                    })
                                    .cursor_pointer()
                                    .hover(|s| s.border_color(Theme::accent_mint()).bg(Theme::tint_mint_badge()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_gpu_tier(cx);
                                    }))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(Theme::SPACE_2_5))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .child(
                                                        div()
                                                            .text_size(px(Theme::TEXT_BODY_LG))
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(Theme::accent_mint())
                                                            .child("我有独立显卡"),
                                                    )
                                                    .child(primitives::badge_accent("GPU 全速加速")),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap(px(Theme::SPACE_1_5))
                                                    .text_size(px(Theme::TEXT_SMALL))
                                                    .text_color(Theme::text_secondary())
                                                    .child("• 开启 Vulkan / CUDA 硬件推理加速")
                                                    .child("• 启用 FFmpeg 视频硬件解码")
                                                    .child("• 启用 DirectML ONNX 加速")
                                                    .child("• 推荐 Whisper Turbo Q5 / Q8 大模型"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .mt(px(Theme::SPACE_4))
                                            .w_full()
                                            .child(
                                                primitives::btn_clickable(
                                                    "选择「我有独立显卡」",
                                                    primitives::BtnSize::Md,
                                                    primitives::BtnVariant::Primary,
                                                )
                                                .id("hardware-setup-btn-gpu")
                                                .track_focus(&gpu_focus)
                                                .focus(|s| {
                                                    s.border_1().border_color(Theme::accent_blue())
                                                })
                                                .w_full()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.choose_gpu_tier(cx);
                                                })),
                                            ),
                                    ),
                            )
                            // 卡片 2：我没有独立显卡（纯 CPU 模式）
                            .child(
                                div()
                                    .id("hardware-choice-cpu")
                                    .track_focus(&cpu_card_focus)
                                    .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .flex()
                                    .flex_col()
                                    .justify_between()
                                    .p(px(Theme::SPACE_4))
                                    .rounded(px(Theme::RADIUS_LG))
                                    .bg(if !is_gpu {
                                        Theme::bg_hover()
                                    } else {
                                        Theme::bg_panel()
                                    })
                                    .border_1()
                                    .border_color(if !is_gpu {
                                        Theme::accent_blue()
                                    } else {
                                        Theme::border()
                                    })
                                    .cursor_pointer()
                                    .hover(|s| s.border_color(Theme::accent_blue()).bg(Theme::bg_hover()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.choose_cpu_tier(cx);
                                    }))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(Theme::SPACE_2_5))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .justify_between()
                                                    .child(
                                                        div()
                                                            .text_size(px(Theme::TEXT_BODY_LG))
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(Theme::text_primary())
                                                            .child("我没有独立显卡（纯 CPU 模式）"),
                                                    )
                                                    .child(primitives::badge("纯 CPU 优化")),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .gap(px(Theme::SPACE_1_5))
                                                    .text_size(px(Theme::TEXT_SMALL))
                                                    .text_color(Theme::text_secondary())
                                                    .child("• 针对多核 CPU 线程优化调度")
                                                    .child("• 0 显存占用，不增加显卡负担")
                                                    .child("• 纯 CPU 软件解码，运行稳定")
                                                    .child("• 推荐 SenseVoice 极速 / Small 模型"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .mt(px(Theme::SPACE_4))
                                            .w_full()
                                            .child(
                                                primitives::btn_clickable(
                                                    "选择「我没有独立显卡」",
                                                    primitives::BtnSize::Md,
                                                    primitives::BtnVariant::Secondary,
                                                )
                                                .id("hardware-setup-btn-cpu")
                                                .track_focus(&cpu_focus)
                                                .focus(|s| {
                                                    s.border_1().border_color(Theme::accent_blue())
                                                })
                                                .w_full()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.choose_cpu_tier(cx);
                                                })),
                                            ),
                                    ),
                            ),
                    )
                    // 底部一行：「稍后再说」与提示语左右分置。
                    // 没有这个按钮时，首次启动的用户只有「选独立显卡 / 选 CPU」两个出口——
                    // 想先看看界面再决定的人被逼着表态，Esc 也关不掉，只能二选一。
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(Theme::SPACE_4))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_muted())
                                    .child("后续可在「性能设置」界面随时重新调整硬件加速模式与参数。"),
                            )
                            .child(
                                primitives::btn_clickable(
                                    "稍后再说",
                                    primitives::BtnSize::Md,
                                    primitives::BtnVariant::Secondary,
                                )
                                .id("hardware-setup-btn-later")
                                .track_focus(&later_focus)
                                .focus(|s| s.border_1().border_color(Theme::accent_blue()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.defer_hardware_setup_dialog(cx);
                                })),
                            ),
                    ),
            )
    }

    /// 选中「我有独立显卡」：两张选择卡片与各自的按钮都调它，键盘回车也调它。
    pub(crate) fn choose_gpu_tier(&mut self, cx: &mut Context<Self>) {
        self.state.set_hardware_tier(true);
        cx.notify();
    }

    /// 选中「我没有独立显卡」：同 [`Self::choose_gpu_tier`]。
    pub(crate) fn choose_cpu_tier(&mut self, cx: &mut Context<Self>) {
        self.state.set_hardware_tier(false);
        cx.notify();
    }

    /// 「稍后再说」：只关框，不动硬件档位、不跑检测、不改任何推理策略。
    pub(crate) fn defer_hardware_setup_dialog(&mut self, cx: &mut Context<Self>) {
        self.state.defer_hardware_setup();
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // 刻意不 `use super::*`：把被测的名字逐个列出来，签名一改就立刻报错。
    use super::keyboard::DialogAction;
    use super::{HW_SETUP_TAB_ACTIONS, HW_SETUP_TAB_COUNT};

    #[test]
    fn hw_setup_tab_order_is_cards_then_buttons() {
        // 下标顺序即 Tab 顺序：GPU 卡片 → CPU 卡片 →「我有独立显卡」按钮 →「我没有独立显卡」按钮 →「稍后再说」。
        assert_eq!(HW_SETUP_TAB_ACTIONS[0], DialogAction::HardwareSelectGpu);
        assert_eq!(HW_SETUP_TAB_ACTIONS[1], DialogAction::HardwareSelectCpu);
        assert_eq!(HW_SETUP_TAB_ACTIONS[2], DialogAction::HardwareSelectGpu);
        assert_eq!(HW_SETUP_TAB_ACTIONS[3], DialogAction::HardwareSelectCpu);
        assert_eq!(HW_SETUP_TAB_ACTIONS[4], DialogAction::HardwareDefer);
    }

    #[test]
    fn hw_setup_card_actions_match_their_buttons() {
        // 卡片与它内部的按钮共用同一个 action（鼠标点击与卡片上回车都走同一个 choose_*_tier）。
        assert_eq!(HW_SETUP_TAB_ACTIONS[0], HW_SETUP_TAB_ACTIONS[2]);
        assert_eq!(HW_SETUP_TAB_ACTIONS[1], HW_SETUP_TAB_ACTIONS[3]);
    }

    #[test]
    fn hw_setup_tab_count_matches_render_indices() {
        assert_eq!(HW_SETUP_TAB_COUNT, 5);
        assert_eq!(HW_SETUP_TAB_COUNT, HW_SETUP_TAB_ACTIONS.len());
    }
}
