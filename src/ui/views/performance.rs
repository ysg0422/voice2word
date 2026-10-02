//! 性能与推理设置工作台视图
//! 
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。
//! 此功能为实验性功能。

use gpui::prelude::*;
use gpui::*;


use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染性能与推理设置工作台
    pub(crate) fn render_performance_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // 页面外壳统一走 primitives::page_shell：内边距 / 分区间距 / 标题字号
        // 与其余三个工作台页同源，切页时内容不再横向跳动。
        primitives::page_shell("performance-settings-page")
            // 1. 页头：标题 + 帮助按钮
            .child(self.render_page_header(cx))
            // 2. 配置说明卡 (右上角问号按钮展开)
            .children(if self.state.show_perf_help {
                Some(self.render_perf_help_card(cx))
            } else {
                None
            })
            // 3. 步骤 2: 识别引擎与模型架构选择
            .child(self.render_engine_selection_card(cx))
            // 4. 步骤 3: 并行度（线程数 / 进程数滑条，量程按本机核心数推导）
            .child(self.render_parallelism_card(cx))
            // 5. 步骤 4: 标点恢复与 AI 文本润色
            .child(self.render_polish_selection_card(cx))
            // 6. 步骤 5: 字幕翻译引擎（离线 Qwen / 在线 OpenAI 兼容 API）
            .child(self.render_translate_settings_card(cx))
    }

    /// 步骤 5：字幕翻译引擎设置。
    ///
    /// 离线档用本地 llama.cpp + Qwen，免费且断网可用；在线档走任意 OpenAI 兼容的
    /// `/chat/completions`（DeepSeek / OpenAI / 通义 / Kimi / 本地 vLLM 均可），
    /// 质量与速度都更好，但需要密钥。两档共用一个「测试连接」按钮，
    /// 让用户在整片翻译失败之前就能发现密钥或地址写错。
    fn render_translate_settings_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::engines::TranslateMode;
        use crate::ui::ApiField;

        let mode = self.state.translate_mode;
        let is_online = mode == TranslateMode::OnlineApi;

        let mode_control = primitives::segmented_cluster()
            .child(self.seg_option(
                "perf-translate-offline",
                "本地 Qwen",
                !is_online,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OfflineQwen);
                    cx.notify();
                },
            ))
            .child(self.seg_option(
                "perf-translate-online",
                "在线 API",
                is_online,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OnlineApi);
                    cx.notify();
                },
            ))
            .into_any_element();

        let base_control = self.render_api_input("api-base-input", ApiField::Base, cx);
        let model_control = self.render_api_input("api-model-input", ApiField::Model, cx);
        let key_input = self.render_api_input("api-key-input", ApiField::Key, cx);
        let key_visible = self.api_key_visible;
        let key_control = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .child(key_input)
            .child(
                // 「显示 / 隐藏」密钥：外观与 chip 原语同形同色，直接复用
                primitives::chip(if key_visible { "隐藏" } else { "显示" }, false, false)
                    .id("api-key-visible-toggle")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.api_key_visible = !this.api_key_visible;
                        cx.notify();
                    })),
            )
            .into_any_element();

        // 批量条数：在线接口按 token 计费，批量越大往返越少；本地推理则影响不大
        let batch = self.state.config.translate.batch_size;
        let batch_control = primitives::segmented_cluster()
            .children([10usize, 20, 40].into_iter().map(|size| {
                self.seg_option(
                    match size {
                        10 => "perf-translate-batch-10",
                        20 => "perf-translate-batch-20",
                        _ => "perf-translate-batch-40",
                    },
                    match size {
                        10 => "10 条/批",
                        20 => "20 条/批",
                        _ => "40 条/批",
                    },
                    batch == size,
                    cx,
                    move |this, cx| {
                        this.state.config.translate.batch_size = size;
                        this.state.save_translate_config();
                        cx.notify();
                    },
                )
            }))
            .into_any_element();

        let probing = self.is_probing_translate;
        let probe_msg = self.translate_probe_msg.clone();
        let probe_control = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(if let Some((ok, text)) = probe_msg {
                div()
                    .max_w(px(Theme::PROBE_MSG_MAX_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_red()
                    })
                    .truncate()
                    .child(text)
                    .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                div()
                    .id("perf-translate-probe-btn")
                    .px(px(Theme::SPACE_4))
                    .h(px(Theme::CTRL_H_SM))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::RADIUS_LG))
                    .bg(if probing {
                        Theme::bg_disabled()
                    } else {
                        Theme::bg_panel()
                    })
                    .border_1()
                    .border_color(Theme::bg_hover_strong())
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if probing {
                        Theme::text_disabled()
                    } else {
                        Theme::accent_blue()
                    })
                    .when(!probing, |s| {
                        s.cursor_pointer()
                            .hover(|s| s.bg(Theme::bg_track()).text_color(Theme::accent_blue()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.probe_online_translate_api(cx);
                            }))
                    })
                    .child(if probing { "测试中…" } else { "测试连接" }),
            )
            .into_any_element();

        // 离线档下把在线参数整块调暗：仍然可见可编辑（方便提前填好），但明确提示未生效
        let dim = move |el: AnyElement| -> AnyElement {
            if is_online {
                el
            } else {
                div().opacity(0.45).child(el).into_any_element()
            }
        };

        primitives::card_rows()
            .child(Self::render_setting_row("翻译引擎", mode_control))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("接口基址", dim(base_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("模型名", dim(model_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("API Key", dim(key_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("每批条数", dim(batch_control)))
            .child(Self::render_setting_divider())
            .child(Self::render_setting_row("连通性", probe_control))
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
            .child(primitives::page_title("性能与推理设置"))
            .child(
                div()
                    .id("perf-help-toggle-btn")
                    .w(px(Theme::CTRL_H_SM))
                    .h(px(Theme::CTRL_H_SM))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .bg(if show_help { Theme::tint_blue_badge() } else { Theme::bg_raised() })
                    .border_1()
                    .border_color(if show_help { Theme::tint_blue_border() } else { Theme::bg_hover_strong() })
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if show_help { Theme::accent_blue() } else { Theme::text_secondary() })
                    .hover(|s| s.border_color(Theme::tint_blue_border()).text_color(Theme::accent_blue()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.show_perf_help = !this.state.show_perf_help;
                        cx.notify();
                    }))
                    .child("?"),
            )
    }

    /// 配置说明卡：由右上角问号按钮展开，逐条介绍各配置项的作用
    fn render_perf_help_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let items: [(&'static str, &'static str); 7] = [
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
                "单个识别进程内的 CPU 线程数。SenseVoice 超过 8 线程后收益递减；Whisper 建议设为物理核心数。",
            ),
            (
                "并行进程数",
                "长音频按时间轴切成多块、每块独立起一个识别进程并发跑。实测多进程远比加线程有效；「自动」按 CPU 核数推导。",
            ),
            (
                "自动标点与语法修正",
                "CT-Punc 毫秒级补标点；Qwen 深度润色更慢，但能纠正错字与口语。",
            ),
            (
                "用户推理偏好",
                "速度优先 / 平衡模式 / 精度优先，一键套用整套推荐配置。",
            ),
            (
                "字幕翻译引擎",
                "本地 Qwen 免费离线；在线 API 支持任意 OpenAI 兼容接口（DeepSeek / OpenAI / 通义等），更快更好但需填密钥。",
            ),
        ];

        div()
            .w_full()
            .p(px(Theme::PAGE_PAD))
            .rounded(px(Theme::CARD_RADIUS))
            .bg(Theme::tint_blue_soft())
            .border_1()
            .border_color(Theme::tint_blue_border())
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        primitives::section_title("配置说明")
                            .text_color(Theme::accent_blue()),
                    )
                    .child(
                        primitives::btn("收起", primitives::BtnSize::Xs, primitives::BtnVariant::Secondary)
                            .id("perf-help-close-btn")
                            .hover(|s| s.text_color(Theme::text_primary()).border_color(Theme::border_strong()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.show_perf_help = false;
                                cx.notify();
                            })),
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
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(format!("· {}", name)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child(*desc),
                    )
            }))
    }


    /// 设置行通用布局：左侧名称，右侧单行等级选择器（简约，无描述小字）
    fn render_setting_row(label: &'static str, control: AnyElement) -> Div {
        primitives::setting_row(label, control)
    }

    /// 设置行之间的细分隔线
    fn render_setting_divider() -> Div {
        primitives::divider()
    }

    /// 单行等级选择胶囊：紧凑分段控件的最小单元。
    /// 外观全部来自 [`primitives::segmented`]，这里只补交互。
    fn seg_option(
        &self,
        id: &'static str,
        label: &'static str,
        is_sel: bool,
        cx: &mut Context<Self>,
        on_select: impl Fn(&mut Self, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        primitives::segmented(label, is_sel, false)
            .id(id)
            .on_click(cx.listener(move |this, _, _, cx| on_select(this, cx)))
    }

    /// 步骤 2：识别引擎与模型架构选择 (每行一个配置项，右侧单行等级选择)
    fn render_engine_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        let engine_control = primitives::segmented_cluster()
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
            (crate::app::WhisperModelTier::Balanced, "perf-tier-small", "Small-Q5 · CPU"),
            (crate::app::WhisperModelTier::TurboSpeed, "perf-tier-turboq5", "Turbo Q5 · 6x"),
            (crate::app::WhisperModelTier::Precise, "perf-tier-turboq8", "Turbo Q8 · 4x"),
        ];
        let mut tier_row = primitives::segmented_cluster();
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
            .gap(px(Theme::PAGE_GAP))
            .child(
                primitives::card_rows()
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

    /// 步骤 3：并行度设置（转写线程数 / 并行进程数 / 润色线程数）
    ///
    /// 三项都用滑条，量程按本机核心数推导：线程数上限 = 逻辑核心数；
    /// 进程数上限 = 引擎内实测最优的并行进程数（`sensevoice_worker_count`），
    /// 最左档为「自动」，即按核数推导。改动即时写入 config.toml 并作用于下一次任务。
    fn render_parallelism_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cores = crate::app::AppState::logical_cores();
        let thread_max = cores.max(2);
        let thread_val = self.state.whisper_threads.clamp(2, thread_max);

        let auto_procs = crate::engines::sensevoice_worker_count(cores) as u32;
        let proc_max = auto_procs.max(1);
        let proc_val = self.state.config.pipeline.parallel_workers.min(proc_max);

        let thread_hint = format!(
            "本机 {cores} 逻辑核心 · SenseVoice 单会话超过 8 线程后收益递减，Whisper 建议设为物理核心数"
        );
        let proc_hint = if proc_val == 0 {
            format!("自动：按本机 {cores} 核推导为 {auto_procs} 进程（实测最优）")
        } else {
            format!("本机 {cores} 核，自动值为 {auto_procs} 进程 · 仅长音频（≥3 分钟）多进程切块时生效")
        };

        let thread_slider = self.render_slider(
            "perf-slider-threads",
            "转写线程数",
            thread_val,
            2,
            thread_max,
            format!("{thread_val} 线程"),
            thread_hint,
            cx,
            move |this, v, cx| {
                if this.state.whisper_threads != v {
                    this.state.whisper_threads = v;
                    this.state.config.pipeline.whisper_threads = v;
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                }
            },
        );

        let proc_slider = self.render_slider(
            "perf-slider-processes",
            "并行进程数",
            proc_val,
            0,
            proc_max,
            if proc_val == 0 {
                format!("自动（{auto_procs}）")
            } else {
                format!("{proc_val} 进程")
            },
            proc_hint,
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.parallel_workers != v {
                    this.state.config.pipeline.parallel_workers = v;
                    this.state.pipeline.set_parallel_workers(v as usize);
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                }
            },
        );

        let llm_val = self.state.config.pipeline.llm_threads.clamp(1, cores.max(1));
        let llm_slider = self.render_slider(
            "perf-slider-llm-threads",
            "润色线程数",
            llm_val,
            1,
            cores.max(1),
            format!("{llm_val} 线程"),
            format!("本机 {cores} 逻辑核心 · 供 Qwen 深度润色使用，选「极速标点」时不受影响"),
            cx,
            move |this, v, cx| {
                if this.state.config.pipeline.llm_threads != v {
                    this.state.config.pipeline.llm_threads = v;
                    this.state.pipeline.set_llm_threads(v);
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                }
            },
        );

        primitives::card_rows()
            .child(thread_slider)
            .child(Self::render_setting_divider())
            .child(proc_slider)
            .child(Self::render_setting_divider())
            .child(llm_slider)
    }

    /// 通用滑条：上行「标签 + 当前值」，中间为可点击 / 可拖动的轨道，下方为提示文案。
    ///
    /// GPUI 0.2 没有内置滑条，这里用「每档一个隐形命中格」实现：
    /// 视觉层（底轨 / 填充 / 手柄）绝对定位，命中层是 `max - min + 1` 个等宽透明格子，
    /// 各自在按下与按住移动时把本档取值回传。这样无需任何坐标换算，
    /// 也不依赖窗口尺寸，天然适配不同 DPI 与布局。
    #[allow(clippy::too_many_arguments)]
    fn render_slider(
        &self,
        id: &'static str,
        label: &'static str,
        value: u32,
        min: u32,
        max: u32,
        value_text: String,
        hint: String,
        cx: &mut Context<Self>,
        on_change: impl Fn(&mut Self, u32, &mut Context<Self>) + Copy + 'static,
    ) -> Stateful<Div> {
        let max = max.max(min);
        let span = (max - min).max(1) as f32;
        let ratio = ((value.clamp(min, max) - min) as f32 / span).clamp(0.0, 1.0);

        div()
            .id(id)
            .w_full()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .py(px(Theme::SPACE_2))
            // 上行：标签 + 当前值胶囊
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(label),
                    )
                    .child(primitives::badge_accent(value_text)),
            )
            // 轨道
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(Theme::SPACE_6))
                    .flex()
                    .items_center()
                    // 1) 底轨 + 已选填充
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .h(px(Theme::PROGRESS_H))
                            .rounded_full()
                            .bg(Theme::bg_track())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(ratio))
                                    .rounded_full()
                                    .bg(Theme::accent_mint()),
                            ),
                    )
                    // 2) 手柄：外圈用卡片底色形成描边，内圈实心薄荷色
                    .child(
                        div()
                            .absolute()
                            .left(relative(ratio))
                            .ml(px(-8.0))
                            .w(px(Theme::SPACE_4))
                            .h(px(Theme::SPACE_4))
                            .rounded_full()
                            .bg(Theme::bg_card())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .w(px(Theme::SPACE_2))
                                    .h(px(Theme::SPACE_2))
                                    .rounded_full()
                                    .bg(Theme::accent_mint()),
                            ),
                    )
                    // 3) 命中层：每档一个隐形格子，支持单击与按住拖动
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .flex()
                            .cursor_pointer()
                            .children((min..=max).map(|v| {
                                div()
                                    .flex_1()
                                    .h_full()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| on_change(this, v, cx)),
                                    )
                                    .on_mouse_move(cx.listener(
                                        move |this, event: &MouseMoveEvent, _, cx| {
                                            if event.pressed_button == Some(MouseButton::Left) {
                                                on_change(this, v, cx);
                                            }
                                        },
                                    ))
                            })),
                    ),
            )
            // 提示
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(hint),
            )
    }

    /// 步骤 4：标点恢复与 AI 润色引擎 (每行一个配置项，右侧单行等级选择)
    fn render_polish_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enable_polish = self.state.enable_polish;
        let is_punc = self.state.polish_mode == crate::app::PolishMode::PuncFast;

        let toggle_btn = primitives::pill_btn_solid(
            if enable_polish { "功能已开启" } else { "功能已关闭" },
            if enable_polish { Theme::accent_mint() } else { Theme::bg_card_hover() },
        )
        .id("perf-toggle-polish-btn")
        .text_color(if enable_polish { Theme::text_on_accent() } else { Theme::text_muted() })
        .on_click(cx.listener(|this, _, _, cx| {
            this.state.enable_polish = !this.state.enable_polish;
            cx.notify();
        }))
        .into_any_element();

        let mode_control = primitives::segmented_cluster()
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

        primitives::card_rows()
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
