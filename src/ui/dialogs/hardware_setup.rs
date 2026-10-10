//! 首次打开软件硬件引导对话框（我有 GPU / 我没有 GPU 向导）

use gpui::prelude::*;
use gpui::*;

use crate::ui::primitives;
use crate::ui::theme::Theme;
use crate::ui::MainWindow;

impl MainWindow {
    /// 首次启动硬件选择对话框：让用户一键选择「我有 GPU」或「我没有 GPU」
    pub(crate) fn render_hardware_setup_modal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_gpu = self.state.config.gpu.is_gpu_tier();

        primitives::modal_scrim()
            .id("hardware-setup-scrim")
            .occlude()
            .child(
                primitives::modal_card(680.0, Theme::PAGE_PAD)
                    .id("hardware-setup-card")
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
                                    .child("请根据您的设备硬件类型选择运行模式，系统将自动配置推理策略与模型方案："),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(Theme::SPACE_4))
                            .w_full()
                            // 卡片 1：我有独立显卡 (GPU 加速)
                            .child(
                                div()
                                    .id("hardware-choice-gpu")
                                    .flex_1()
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
                                        this.state.set_hardware_tier(true);
                                        cx.notify();
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
                                                            .child("⚡ 我有独立显卡"),
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
                                                    "选择「我有 GPU」",
                                                    primitives::BtnSize::Md,
                                                    primitives::BtnVariant::Primary,
                                                )
                                                .id("hardware-setup-btn-gpu")
                                                .w_full()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.set_hardware_tier(true);
                                                    cx.notify();
                                                })),
                                            ),
                                    ),
                            )
                            // 卡片 2：我没有显卡 (纯 CPU 模式)
                            .child(
                                div()
                                    .id("hardware-choice-cpu")
                                    .flex_1()
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
                                        this.state.set_hardware_tier(false);
                                        cx.notify();
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
                                                            .child("💻 我没有 GPU"),
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
                                                    "选择「我没有 GPU」",
                                                    primitives::BtnSize::Md,
                                                    primitives::BtnVariant::Secondary,
                                                )
                                                .id("hardware-setup-btn-cpu")
                                                .w_full()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.set_hardware_tier(false);
                                                    cx.notify();
                                                })),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child("后续可在「性能设置」界面随时重新调整硬件加速模式与参数。"),
                    ),
            )
    }
}
