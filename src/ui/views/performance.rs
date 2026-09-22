//! 性能与推理设置工作台视图
//! 
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。
//! 此功能为实验性功能。

use gpui::prelude::*;
use gpui::*;


use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染性能与推理设置工作台
    pub(crate) fn render_performance_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("performance-settings-page")
            .w_full()
            .h_full()
            .bg(Theme::bg_panel())
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_5()
            // 1. 页头：标题 + 实验性功能徽标 + 帮助按钮 + 性能等级指示
            .child(self.render_page_header(cx))
            // 2. 配置说明卡 (右上角问号按钮展开)
            .children(if self.state.show_perf_help {
                Some(self.render_perf_help_card(cx))
            } else {
                None
            })
            // 3. 步骤 2: 识别引擎与模型架构选择
            .child(self.render_engine_selection_card(cx))
            // 4. 步骤 3: CPU 并发计算线程
            .child(self.render_thread_selection_card(cx))
            // 5. 步骤 4: 标点恢复与 AI 文本润色
            .child(self.render_polish_selection_card(cx))
    }

    /// 页头：标题 + 帮助按钮 (简约，无徽标)
    fn render_page_header(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let show_help = self.state.show_perf_help;
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(17.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("性能与推理设置"),
            )
            .child(
                div()
                    .id("perf-help-toggle-btn")
                    .w(px(26.0))
                    .h(px(26.0))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .bg(if show_help { rgba(0x38bdf826) } else { rgb(0x1b1b24) })
                    .border_1()
                    .border_color(if show_help { rgba(0x38bdf877) } else { rgb(0x2a2a36) })
                    .text_size(px(12.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if show_help { Theme::accent_blue() } else { Theme::text_secondary() })
                    .hover(|s| s.border_color(rgba(0x38bdf866)).text_color(Theme::accent_blue()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.show_perf_help = !this.state.show_perf_help;
                        cx.notify();
                    }))
                    .child("?"),
            )
    }

    /// 配置说明卡：由右上角问号按钮展开，逐条介绍各配置项的作用
    fn render_perf_help_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: [(&'static str, &'static str); 5] = [
            (
                "转写引擎",
                "SenseVoice：中文极速、自带标点；Whisper：多语种、方言口音更稳。",
            ),
            (
                "Whisper 模型档位",
                "Base 最快 → Turbo Q8 最精，按「速度换精度」选择，倍速为相对实时倍率。",
            ),
            (
                "转写线程数",
                "CPU 并行核心数，一般设为物理核心数即可，8 线程收益最佳。",
            ),
            (
                "自动标点与语法修正",
                "CT-Punc 毫秒级补标点；Qwen 深度润色更慢，但能纠正错字与口语。",
            ),
            (
                "用户推理偏好",
                "速度优先 / 平衡模式 / 精度优先，一键套用整套推荐配置。",
            ),
        ];

        div()
            .w_full()
            .p_4()
            .rounded_xl()
            .bg(rgba(0x38bdf80d))
            .border_1()
            .border_color(rgba(0x38bdf82e))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::accent_blue())
                            .child("配置说明"),
                    )
                    .child(
                        div()
                            .id("perf-help-close-btn")
                            .px_2p5()
                            .py_0p5()
                            .rounded_md()
                            .bg(rgb(0x1b1b24))
                            .border_1()
                            .border_color(rgb(0x2a2a36))
                            .cursor_pointer()
                            .text_size(px(10.5))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.text_color(Theme::text_primary()).border_color(rgb(0x3a3a48)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.show_perf_help = false;
                                cx.notify();
                            }))
                            .child("收起"),
                    ),
            )
            .children(items.iter().map(|(name, desc)| {
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(format!("· {}", name)),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .child(*desc),
                    )
            }))
    }


    /// 设置行通用布局：左侧名称，右侧单行等级选择器（简约，无描述小字）
    fn render_setting_row(label: &'static str, control: AnyElement) -> Div {
        div()
            .w_full()
            .flex()
            .items_center()
            .gap_4()
            .py_2p5()
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .child(control),
            )
    }

    /// 设置行之间的细分隔线
    fn render_setting_divider() -> Div {
        div().w_full().h(px(1.0)).bg(rgb(0x24242e))
    }

    /// 单行等级选择胶囊：紧凑分段控件的最小单元
    fn seg_option(
        &self,
        id: &'static str,
        label: &'static str,
        is_sel: bool,
        cx: &mut Context<Self>,
        on_select: impl Fn(&mut Self, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .px_3p5()
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_lg()
            .cursor_pointer()
            .bg(if is_sel { rgba(0x38bdf81f) } else { rgb(0x181822) })
            .border_1()
            .border_color(if is_sel { rgba(0x38bdf866) } else { rgb(0x282834) })
            .text_size(px(11.0))
            .font_weight(if is_sel { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
            .text_color(if is_sel { Theme::accent_blue() } else { Theme::text_secondary() })
            .hover(move |s| if is_sel { s } else { s.bg(rgb(0x20202c)).text_color(Theme::text_primary()) })
            .on_click(cx.listener(move |this, _, _, cx| on_select(this, cx)))
            .child(label)
    }

    /// 步骤 2：识别引擎与模型架构选择 (每行一个配置项，右侧单行等级选择)
    fn render_engine_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        let engine_control = div()
            .flex()
            .gap_1p5()
            .child(self.seg_option("perf-engine-pill-sv", "SenseVoice 极速 (推荐)", is_sv, cx, |this, cx| {
                this.state.whisper_model_tier = crate::app::WhisperModelTier::SenseVoice;
                cx.notify();
            }))
            .child(self.seg_option("perf-engine-pill-whisper", "OpenAI Whisper 全能", !is_sv, cx, |this, cx| {
                if this.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice {
                    this.state.whisper_model_tier = crate::app::WhisperModelTier::TurboSpeed;
                }
                cx.notify();
            }))
            .into_any_element();

        let tiers: [(crate::app::WhisperModelTier, &'static str, &'static str); 4] = [
            (crate::app::WhisperModelTier::Fast, "perf-tier-base", "Base · 20x"),
            (crate::app::WhisperModelTier::Balanced, "perf-tier-small", "Small · 7x"),
            (crate::app::WhisperModelTier::TurboSpeed, "perf-tier-turboq5", "Turbo Q5 · 6x"),
            (crate::app::WhisperModelTier::Precise, "perf-tier-turboq8", "Turbo Q8 · 4x"),
        ];
        let mut tier_row = div().flex().gap_1p5();
        for (tier, id, label) in tiers {
            let is_sel = self.state.whisper_model_tier == tier;
            tier_row = tier_row.child(self.seg_option(id, label, is_sel, cx, move |this, cx| {
                this.state.whisper_model_tier = tier;
                cx.notify();
            }));
        }
        let tier_control = if is_sv {
            tier_row.opacity(0.5).into_any_element()
        } else {
            tier_row.into_any_element()
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(
                div()
                    .w_full()
                    .px_4()
                    .py_1()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .child(Self::render_setting_row(
                        "转写引擎",
                        engine_control,
                    ))
                    .child(Self::render_setting_divider())
                    .child(Self::render_setting_row(
                        "Whisper 模型档位",
                        tier_control,
                    )),
            )
    }

    /// 步骤 3：CPU 并发计算核心数 (每行一个配置项，右侧单行等级选择)
    fn render_thread_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cur_threads = self.state.whisper_threads;

        let mut thread_row = div().flex().gap_1p5();
        for (num, id, label) in [
            (4u32, "perf-thread-4", "4 线程"),
            (8, "perf-thread-8", "8 线程"),
            (12, "perf-thread-12", "12 线程"),
            (16, "perf-thread-16", "16 线程"),
        ] {
            let is_sel = cur_threads == num;
            thread_row = thread_row.child(self.seg_option(id, label, is_sel, cx, move |this, cx| {
                this.state.whisper_threads = num;
                cx.notify();
            }));
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(
                div()
                    .w_full()
                    .px_4()
                    .py_1()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .child(Self::render_setting_row(
                        "转写线程数",
                        thread_row.into_any_element(),
                    )),
            )
    }

    /// 步骤 4：标点恢复与 AI 润色引擎 (每行一个配置项，右侧单行等级选择)
    fn render_polish_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enable_polish = self.state.enable_polish;
        let is_punc = self.state.polish_mode == crate::app::PolishMode::PuncFast;

        let toggle_btn = div()
            .id("perf-toggle-polish-btn")
            .px_4()
            .py_1p5()
            .rounded_full()
            .bg(if enable_polish { Theme::accent_mint() } else { rgb(0x27272a) })
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .on_click(cx.listener(|this, _, _, cx| {
                this.state.enable_polish = !this.state.enable_polish;
                cx.notify();
            }))
            .text_size(px(11.5))
            .font_weight(FontWeight::BOLD)
            .text_color(if enable_polish { rgb(0x09090b) } else { Theme::text_muted() })
            .child(if enable_polish { "功能已开启" } else { "功能已关闭" })
            .into_any_element();

        let mode_control = div()
            .flex()
            .gap_1p5()
            .child(self.seg_option("perf-select-punc-fast", "CT-Punc 极速标点", is_punc && enable_polish, cx, |this, cx| {
                // 点选润色引擎 = 明确要润色，顺手打开总开关
                this.state.enable_polish = true;
                this.state.polish_mode = crate::app::PolishMode::PuncFast;
                cx.notify();
            }))
            .child(self.seg_option("perf-select-qwen-deep", "Qwen 深度润色", !is_punc && enable_polish, cx, |this, cx| {
                this.state.enable_polish = true;
                this.state.polish_mode = crate::app::PolishMode::QwenDeep;
                cx.notify();
            }))
            .into_any_element();
        // 总开关关闭时降透明度提示「未生效」，但行保持可见可点
        let mode_control = if enable_polish {
            mode_control
        } else {
            div().opacity(0.45).child(mode_control).into_any_element()
        };

        div()
            .w_full()
            .px_4()
            .py_1()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .child(Self::render_setting_row(
                "自动标点与语法修正",
                toggle_btn,
            ))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row(
                "润色引擎",
                mode_control,
            ))
    }
}
