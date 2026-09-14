//! 性能与推理设置工作台视图
//! 
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。
//! 此功能为实验性功能。

use gpui::prelude::*;
use gpui::*;

use crate::core::UserStrategy;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染性能与推理设置工作台
    pub(crate) fn render_performance_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let hw = self.state.hardware_info.clone();
        let level = self.state.performance_level;
        let strategy = self.state.user_strategy;
        let profile = self.state.recommended_profile.clone();
        let bench = self.state.benchmark_result.clone();
        let is_benchmarking = self.state.is_benchmarking;

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
            // 1. 页头：标题 + 实验性功能徽标 + 性能等级指示
            .child(self.render_page_header(level))
            // 2. 实验性功能提示横幅
            .child(Self::render_experimental_banner())
            // 3. 步骤 1: 硬件计算算力与规格概览
            .child(self.render_hardware_overview_cards(&hw, cx))
            .child(self.render_benchmark_and_actions_bar(&bench, is_benchmarking, cx))
            // 4. 步骤 2: 识别引擎与模型架构选择 (移入不常换的高级设置)
            .child(self.render_engine_selection_card(cx))
            // 5. 步骤 3: CPU 并发计算线程 (移入不常换的高级设置)
            .child(self.render_thread_selection_card(cx))
            // 6. 步骤 4: 标点恢复与 AI 文本润色 (移入不常换的高级设置)
            .child(self.render_polish_selection_card(cx))
            // 7. 步骤 5: 用户推荐偏好策略与一键配置面板
            .child(self.render_strategy_selector(strategy, cx))
            .child(self.render_recommended_profile_panel(&profile, level, strategy, cx))
    }

    /// 页头：标题 + 实验性标签 + 等级指示器
    fn render_page_header(
        &self,
        level: crate::core::PerformanceLevel,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    // 标题
                    .child(
                        div()
                            .text_size(px(17.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("性能与推理设置"),
                    )
                    // 实验性功能徽标
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(rgba(0xf59e0b12))
                            .border_1()
                            .border_color(rgba(0xf59e0b30))
                            .text_size(px(10.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::accent_orange())
                            .child("实验性功能"),
                    ),
            )
            // 右侧：性能等级圆点标签
            .child(
                div()
                    .px_3()
                    .py_1()
                    .rounded_lg()
                    .bg(rgba((level.color_hex() << 8) | 0x14))
                    .border_1()
                    .border_color(rgba((level.color_hex() << 8) | 0x38))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(rgb(level.color_hex())))
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(level.color_hex()))
                            .child(level.label()),
                    ),
            )
    }

    /// 实验性功能提示横幅 —— 低调提醒用户这是实验性功能
    fn render_experimental_banner() -> impl IntoElement {
        div()
            .w_full()
            .px_4()
            .py_2p5()
            .rounded_lg()
            .bg(rgba(0xf59e0b0a))
            .border_1()
            .border_color(rgba(0xf59e0b1a))
            .flex()
            .items_center()
            .gap_2p5()
            .child(
                div()
                    .w(px(3.0))
                    .h(px(16.0))
                    .rounded_sm()
                    .bg(rgba(0xf59e0b66)),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgba(0xf59e0bcc))
                    .child("此页面包含实验性功能，硬件检测结果和推荐配置仅供参考，可能在后续版本中调整或移除。"),
            )
    }

    /// 渲染区块标题 (带左侧竖向强调线)
    fn render_section_header(title: &str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .w(px(3.0))
                    .h(px(14.0))
                    .rounded_sm()
                    .bg(Theme::accent_primary()),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .child(title.to_string()),
            )
    }

    /// 渲染硬件信息 4 格卡片 (极简视觉，统一层级结构)
    fn render_hardware_overview_cards(
        &self,
        hw: &crate::core::HardwareInfo,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let primary_gpu = hw
            .gpus
            .first()
            .map(|g| format!("{} ({})", g.name, g.backend))
            .unwrap_or_else(|| "未检测到独立/集成 GPU".to_string());

        let gpu_sub = if hw.gpus.iter().any(|g| g.is_discrete) {
            ("独立显卡加速", true)
        } else {
            ("集成核显 / 软件后端", false)
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(Self::render_section_header("硬件规格概览"))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2p5()
                    // CPU
                    .child(Self::render_hw_card(
                        "CPU 处理器",
                        &hw.cpu_brand,
                        &format!(
                            "{} 核 / {} 线程 ({:.1} GHz)",
                            hw.physical_cores,
                            hw.logical_threads,
                            hw.cpu_frequency_mhz as f64 / 1000.0
                        ),
                        Theme::accent_mint(),
                    ))
                    // RAM
                    .child(Self::render_hw_card(
                        "系统内存 (RAM)",
                        &hw.formatted_total_memory(),
                        &format!("可用: {}", hw.formatted_available_memory()),
                        Theme::text_secondary(),
                    ))
                    // GPU
                    .child(Self::render_hw_card(
                        "GPU 硬件加速",
                        &primary_gpu,
                        gpu_sub.0,
                        if gpu_sub.1 { Theme::accent_mint() } else { Theme::text_muted() },
                    ))
                    // 推理模式
                    .child(Self::render_hw_card(
                        "当前推理模式",
                        &hw.inference_mode.label(),
                        "AVX2 / FMA 向量指令加速",
                        Theme::text_muted(),
                    )),
            )
    }

    /// 单个硬件卡片 — 统一复用样板 (label / value / sub)
    fn render_hw_card(
        label: &str,
        value: &str,
        sub: &str,
        sub_color: Rgba,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .p_3p5()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .justify_between()
            .gap_2()
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_muted())
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .overflow_hidden()
                    .child(value.to_string()),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(sub_color)
                    .overflow_hidden()
                    .child(sub.to_string()),
            )
    }

    /// 渲染跑分测试与重检操作栏
    fn render_benchmark_and_actions_bar(
        &self,
        bench: &Option<crate::core::BenchmarkResult>,
        is_benchmarking: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .px_4()
            .py_3()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    // 左侧竖线
                    .child(
                        div()
                            .w(px(3.0))
                            .h(px(20.0))
                            .rounded_sm()
                            .bg(if bench.is_some() {
                                Theme::accent_mint()
                            } else {
                                Theme::text_muted()
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_secondary())
                            .child("CPU 算力压测"),
                    )
                    .child(
                        if let Some(res) = bench {
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                // 分数
                                .child(
                                    div()
                                        .px_2p5()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(rgba(0x10b98114))
                                        .border_1()
                                        .border_color(rgba(0x10b98128))
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_mint())
                                        .child(format!("{} 分 ({:.1} GFLOPS)", res.score, res.gflops_estimate)),
                                )
                                // 评级标签
                                .child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(rgb(0x1e1e28))
                                        .text_size(px(10.5))
                                        .text_color(Theme::text_muted())
                                        .child(format!("评级: {} · 耗时 {} ms", res.throughput_rating, res.duration_ms)),
                                )
                        } else if is_benchmarking {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                // 简单脉冲指示器
                                .child(
                                    div()
                                        .w(px(6.0))
                                        .h(px(6.0))
                                        .rounded_full()
                                        .bg(Theme::accent_primary()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Theme::accent_primary())
                                        .child("正在执行多核矩阵密集型计算压测..."),
                                )
                        } else {
                            div()
                                .text_size(px(11.0))
                                .text_color(Theme::text_muted())
                                .child("未运行基准测试")
                        },
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    // 重新检测硬件
                    .child(
                        div()
                            .id("btn-refresh-hardware")
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .bg(rgb(0x1c1c26))
                            .border_1()
                            .border_color(Theme::border())
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_secondary())
                            .cursor_pointer()
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.refresh_hardware_detection();
                                cx.notify();
                            }))
                            .child("重新检测"),
                    )
                    // 运行性能测试
                    .child(
                        div()
                            .id("btn-run-benchmark")
                            .px_3p5()
                            .py_1p5()
                            .rounded_lg()
                            .bg(if is_benchmarking {
                                rgb(0x3a3a48)
                            } else {
                                Theme::accent_primary()
                            })
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .hover(move |s| {
                                if !is_benchmarking {
                                    s.opacity(0.88)
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.state.is_benchmarking {
                                    return;
                                }
                                this.state.is_benchmarking = true;
                                cx.notify();

                                cx.spawn(async move |this, cx| {
                                    let res = cx
                                        .background_executor()
                                        .spawn(async move {
                                            crate::core::run_cpu_benchmark()
                                        })
                                        .await;

                                    let _ = this.update(cx, |this, cx| {
                                        this.state.is_benchmarking = false;
                                        this.state.benchmark_result = Some(res);
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }))
                            .child(if is_benchmarking {
                                "压测执行中..."
                            } else {
                                "运行性能测试"
                            }),
                    ),
            )
    }

    /// 渲染用户策略切换卡片 (极简工程风格，radio-button 选择器)
    fn render_strategy_selector(
        &self,
        current: UserStrategy,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let strategies = [
            (
                UserStrategy::Speed,
                "strategy-card-speed",
                "速度优先",
                "极速",
                UserStrategy::Speed.description(),
            ),
            (
                UserStrategy::Balanced,
                "strategy-card-balanced",
                "平衡模式",
                "推荐",
                UserStrategy::Balanced.description(),
            ),
            (
                UserStrategy::Quality,
                "strategy-card-quality",
                "精度优先",
                "高精",
                UserStrategy::Quality.description(),
            ),
        ];

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(Self::render_section_header("用户推理偏好策略"))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2p5()
                    .children(strategies.into_iter().map(|(strat, card_id, title, tag, desc)| {
                        let is_active = current == strat;
                        div()
                            .id(card_id)
                            .flex_1()
                            .min_w_0()
                            .p_3p5()
                            .rounded_xl()
                            .cursor_pointer()
                            .bg(if is_active {
                                rgba(0x6366f10e)
                            } else {
                                Theme::bg_card()
                            })
                            .border_1()
                            .border_color(if is_active {
                                rgba(0x6366f166)
                            } else {
                                Theme::border()
                            })
                            .hover(move |s| {
                                if !is_active {
                                    s.bg(Theme::bg_hover()).border_color(Theme::border_light())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.set_user_strategy(strat);
                                cx.notify();
                            }))
                            .flex()
                            .flex_col()
                            .gap_2p5()
                            // 第一行: radio + 标题 + tag
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            // Radio 圆点
                                            .child(
                                                div()
                                                    .w(px(14.0))
                                                    .h(px(14.0))
                                                    .rounded_full()
                                                    .border_1()
                                                    .border_color(if is_active { Theme::accent_primary() } else { Theme::text_muted() })
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(if is_active {
                                                        div().w(px(7.0)).h(px(7.0)).rounded_full().bg(Theme::accent_primary()).into_any_element()
                                                    } else {
                                                        div().into_any_element()
                                                    }),
                                            )
                                            // 标题文字
                                            .child(
                                                div()
                                                    .text_size(px(12.5))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(if is_active {
                                                        Theme::text_primary()
                                                    } else {
                                                        Theme::text_primary()
                                                    })
                                                    .child(title),
                                            ),
                                    )
                                    // tag 标签
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_md()
                                            .bg(if is_active { rgba(0x6366f11c) } else { rgba(0xffffff08) })
                                            .text_size(px(10.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if is_active { Theme::accent_primary() } else { Theme::text_muted() })
                                            .child(tag),
                                    ),
                            )
                            // 第二行: 描述文字
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_muted())
                                    .child(desc),
                            )
                    })),
            )
    }

    /// 渲染推荐推理配置看板 (统一的子卡片样式)
    fn render_recommended_profile_panel(
        &self,
        profile: &crate::core::InferenceProfile,
        level: crate::core::PerformanceLevel,
        strategy: UserStrategy,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            // 区块标题行
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2p5()
                            .child(Self::render_section_header("当前推荐推理配置"))
                            // 当前策略标签
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(rgba(0x10b98112))
                                    .border_1()
                                    .border_color(rgba(0x10b98128))
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_mint())
                                    .child(format!("{} · {}", level.code(), strategy.label())),
                            ),
                    )
                    // 应用推荐配置按钮
                    .child(
                        div()
                            .id("btn-apply-recommended-profile")
                            .px_3p5()
                            .py_1p5()
                            .rounded_lg()
                            .bg(Theme::accent_primary())
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.88))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.apply_recommended_profile();
                                cx.notify();
                            }))
                            .child("应用推荐配置"),
                    ),
            )
            // 决策参数网格 —— 外部容器卡片
            .child(
                div()
                    .w_full()
                    .p_3p5()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    // 第 1 行
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap_2p5()
                            .child(Self::render_profile_cell(
                                "语音模型",
                                profile.whisper_model_name,
                                Theme::text_primary(),
                            ))
                            .child(Self::render_profile_cell(
                                "转写线程",
                                &format!("{} 线程", profile.whisper_threads),
                                Theme::accent_mint(),
                            ))
                            .child(Self::render_profile_cell(
                                "静音加速",
                                if profile.enable_vad { "启用 (跳过无声片段)" } else { "禁用" },
                                if profile.enable_vad { Theme::accent_mint() } else { Theme::text_muted() },
                            )),
                    )
                    // 第 2 行
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap_2p5()
                            .child(Self::render_profile_cell(
                                "纠错模型",
                                profile.llm_model_name,
                                Theme::text_primary(),
                            ))
                            .child(Self::render_profile_cell(
                                "纠错线程",
                                &format!("{} 线程", profile.llm_threads),
                                Theme::accent_mint(),
                            ))
                            .child(Self::render_profile_cell(
                                "计算硬件",
                                &format!(
                                    "{} / {} 个任务",
                                    if profile.gpu_offload { "GPU 加速" } else { "纯 CPU 计算" },
                                    profile.max_concurrency
                                ),
                                Theme::text_primary(),
                            )),
                    ),
            )
            // 决策理由
            .child(
                div()
                    .w_full()
                    .px_3p5()
                    .py_2p5()
                    .rounded_lg()
                    .bg(rgba(0x6366f108))
                    .border_1()
                    .border_color(rgba(0x6366f11a))
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .w(px(3.0))
                            .h(px(14.0))
                            .rounded_sm()
                            .bg(rgba(0x6366f166)),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::accent_primary())
                            .child("推荐依据"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .child(profile.recommendation_reason.clone()),
                    ),
            )
    }

    /// 推荐配置面板中的单个参数格子 (统一复用)
    fn render_profile_cell(
        label: &str,
        value: &str,
        value_color: Rgba,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .p_3()
            .rounded_lg()
            .bg(rgb(0x14141d))
            .border_1()
            .border_color(rgb(0x1e1e2a))
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_muted())
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(value_color)
                    .overflow_hidden()
                    .child(value.to_string()),
            )
    }

    /// 步骤 2：识别引擎与模型架构选择
    fn render_engine_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(Self::render_section_header("步骤 2：转写引擎与模型架构"))
            .child(
                div()
                    .w_full()
                    .p_4()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .gap_3()
                    // 选项 1: 阿里 SenseVoice 极速 (推荐)
                    .child(
                        div()
                            .id("perf-engine-card-sensevoice")
                            .p_3p5()
                            .rounded_xl()
                            .border_1()
                            .cursor_pointer()
                            .bg(if is_sv { rgba(0x10b98114) } else { rgb(0x15151c) })
                            .border_color(if is_sv { rgba(0x10b98177) } else { rgb(0x252530) })
                            .hover(move |s| if !is_sv { s.border_color(rgb(0x363646)).bg(rgb(0x1a1a23)) } else { s })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.whisper_model_tier = crate::app::WhisperModelTier::SenseVoice;
                                cx.notify();
                            }))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .w(px(14.0))
                                                    .h(px(14.0))
                                                    .rounded_full()
                                                    .border_1()
                                                    .border_color(if is_sv { Theme::accent_mint() } else { Theme::text_muted() })
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(if is_sv {
                                                        div().w(px(7.0)).h(px(7.0)).rounded_full().bg(Theme::accent_mint()).into_any_element()
                                                    } else {
                                                        div().into_any_element()
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(if is_sv { rgb(0xffffff) } else { Theme::text_primary() })
                                                    .child("SenseVoice 极速引擎 (推荐)"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(Theme::text_muted())
                                            .child("阿里非自回归 CTC 单次前向 · 42 倍速极速识别 · 原生带富文本标点与 ITN 数字格式转换"),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_1()
                                    .rounded_full()
                                    .bg(rgba(0x10b98124))
                                    .border_1()
                                    .border_color(rgba(0x10b98144))
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_mint())
                                    .child("42x 倍速推荐"),
                            ),
                    )
                    // 选项 2: OpenAI Whisper 全能
                    .child(
                        div()
                            .id("perf-engine-card-whisper")
                            .p_3p5()
                            .rounded_xl()
                            .border_1()
                            .cursor_pointer()
                            .bg(if !is_sv { rgba(0x6366f114) } else { rgb(0x15151c) })
                            .border_color(if !is_sv { rgba(0x6366f177) } else { rgb(0x252530) })
                            .hover(move |s| if is_sv { s.border_color(rgb(0x363646)).bg(rgb(0x1a1a23)) } else { s })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice {
                                    this.state.whisper_model_tier = crate::app::WhisperModelTier::TurboSpeed;
                                }
                                cx.notify();
                            }))
                            .flex()
                            .flex_col()
                            .gap_2p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .child(
                                                        div()
                                                            .w(px(14.0))
                                                            .h(px(14.0))
                                                            .rounded_full()
                                                            .border_1()
                                                            .border_color(if !is_sv { Theme::accent_primary() } else { Theme::text_muted() })
                                                            .flex()
                                                            .items_center()
                                                            .justify_center()
                                                            .child(if !is_sv {
                                                                div().w(px(7.0)).h(px(7.0)).rounded_full().bg(Theme::accent_primary()).into_any_element()
                                                            } else {
                                                                div().into_any_element()
                                                            }),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(13.0))
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(if !is_sv { rgb(0xffffff) } else { Theme::text_primary() })
                                                            .child("OpenAI Whisper 全能引擎"),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.0))
                                                    .text_color(Theme::text_muted())
                                                    .child("自回归 Sequence-to-Sequence 模型 · 多语种强鲁棒性 · 极佳方言口音抗干扰"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2p5()
                                            .py_1()
                                            .rounded_full()
                                            .bg(rgba(0x6366f124))
                                            .border_1()
                                            .border_color(rgba(0x6366f144))
                                            .text_size(px(10.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_primary())
                                            .child("多语种高精"),
                                    ),
                            )
                            // Whisper 4 档位选择面板
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(Theme::text_secondary())
                                            .child("Whisper 模型档位精细微调："),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .children([
                                                (crate::app::WhisperModelTier::Fast, "Base", "20x 倍速 · 39M"),
                                                (crate::app::WhisperModelTier::Balanced, "Small", "7x 倍速 · 244M"),
                                                (crate::app::WhisperModelTier::TurboSpeed, "Turbo Q5", "6x 倍速 · 破带宽推荐"),
                                                (crate::app::WhisperModelTier::Precise, "Turbo Q8", "4x 倍速 · 旗舰高精"),
                                            ].into_iter().enumerate().map(|(idx, (tier, name, desc))| {
                                                let is_sel = self.state.whisper_model_tier == tier;
                                                div()
                                                    .id(("perf-tier-chip", idx))
                                                    .flex_1()
                                                    .p_2p5()
                                                    .rounded_lg()
                                                    .cursor_pointer()
                                                    .bg(if is_sel { Theme::accent_primary() } else { rgb(0x181822) })
                                                    .border_1()
                                                    .border_color(if is_sel { Theme::accent_primary() } else { rgb(0x282834) })
                                                    .text_color(if is_sel { rgb(0xffffff) } else { Theme::text_secondary() })
                                                    .hover(|s| if !is_sel { s.bg(Theme::bg_hover()).text_color(Theme::text_primary()) } else { s })
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.state.whisper_model_tier = tier;
                                                        cx.notify();
                                                    }))
                                                    .flex()
                                                    .flex_col()
                                                    .items_center()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .text_size(px(11.5))
                                                            .font_weight(FontWeight::BOLD)
                                                            .child(name),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_size(px(10.0))
                                                            .opacity(0.85)
                                                            .child(desc),
                                                    )
                                            }))
                                    )
                            )
                    )
            )
    }

    /// 步骤 3：CPU 并发计算核心数
    fn render_thread_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cur_threads = self.state.whisper_threads;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(Self::render_section_header("步骤 3：CPU 并发计算核心数"))
            .child(
                div()
                    .w_full()
                    .p_4()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .child("调整 Whisper.cpp 多线程 CPU 计算并发核心数（建议设为物理核心数，8 线程收益最佳）："),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2p5()
                            .children([
                                (4, "4 线程", "低功耗 / 双核 CPU"),
                                (8, "8 线程 (推荐)", "最佳性能开销平衡"),
                                (12, "12 线程", "高规格 6核/8核 CPU"),
                                (16, "16 线程", "极限多核并行压测"),
                            ].into_iter().map(|(num, label, desc)| {
                                let is_sel = cur_threads == num;
                                div()
                                    .id(("perf-thread-chip", num))
                                    .flex_1()
                                    .p_3()
                                    .rounded_xl()
                                    .cursor_pointer()
                                    .bg(if is_sel { rgba(0x10b98118) } else { rgb(0x181822) })
                                    .border_1()
                                    .border_color(if is_sel { Theme::accent_mint() } else { rgb(0x282834) })
                                    .hover(|s| if !is_sel { s.bg(Theme::bg_hover()) } else { s })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.whisper_threads = num;
                                        cx.notify();
                                    }))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if is_sel { Theme::accent_mint() } else { Theme::text_primary() })
                                            .child(label),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(Theme::text_muted())
                                            .child(desc),
                                    )
                            }))
                    )
            )
    }

    /// 步骤 4：标点恢复与 AI 润色引擎
    fn render_polish_selection_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enable_polish = self.state.enable_polish;
        let is_punc = self.state.polish_mode == crate::app::PolishMode::PuncFast;

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2p5()
            .child(Self::render_section_header("步骤 4：标点恢复与智能润色"))
            .child(
                div()
                    .w_full()
                    .p_4()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .gap_3()
                    // 状态开关行
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .child("自动标点恢复与语法修正开关"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(Theme::text_muted())
                                            .child("开启后可自动切分句子、补充句号叹号，或使用大语言模型进行深度错字纠错"),
                                    ),
                            )
                            .child(
                                div()
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
                                    .child(if enable_polish { "功能已开启" } else { "功能已关闭" }),
                            )
                    )
                    // 引擎模式切换
                    .children(if enable_polish {
                        Some(
                            div()
                                .flex()
                                .gap_2p5()
                                .pt_1()
                                .child(
                                    div()
                                        .id("perf-select-punc-fast")
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .cursor_pointer()
                                        .bg(if is_punc { rgba(0x10b98114) } else { rgb(0x181822) })
                                        .border_1()
                                        .border_color(if is_punc { rgba(0x10b98155) } else { rgb(0x282834) })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.state.polish_mode = crate::app::PolishMode::PuncFast;
                                            cx.notify();
                                        }))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(if is_punc { Theme::accent_mint() } else { Theme::text_primary() })
                                                .child("CT-Punc 极速标点 (推荐)"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(Theme::text_muted())
                                                .child("阿里 CT-Punc 标点模型 · 仅耗时数毫秒 · 高效对齐语调停顿"),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("perf-select-qwen-deep")
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .cursor_pointer()
                                        .bg(if !is_punc { rgba(0x38bdf814) } else { rgb(0x181822) })
                                        .border_1()
                                        .border_color(if !is_punc { rgba(0x38bdf855) } else { rgb(0x282834) })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.state.polish_mode = crate::app::PolishMode::QwenDeep;
                                            cx.notify();
                                        }))
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(if !is_punc { rgb(0x38bdf8) } else { Theme::text_primary() })
                                                .child("Qwen 大模型深度润色"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(Theme::text_muted())
                                                .child("Qwen2.5 7B 端侧 LLM 深度上下文重构 · 口误智能修正"),
                                        ),
                                )
                        )
                    } else {
                        None
                    })
            )
    }
}
