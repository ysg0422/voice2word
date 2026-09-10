//! 性能与推理设置工作台视图
//! 
//! 实现「硬件检测 → 性能评估 → 策略矩阵决策 → 一键应用」的原生 GPUI 交互面板。

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
            .p_5()
            .flex()
            .flex_col()
            .gap_4()
            // 1. 顶部标头与综合评级
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_2p5()
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("性能与推理设置"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(Theme::text_muted())
                                    .child("硬件实时感知与多维性能综合评估 · AI 推理策略参数智能决策"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(Theme::text_secondary())
                                    .child("本机性能等级:"),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(rgb(0x1a1a24))
                                    .border_1()
                                    .border_color(rgb(level.color_hex()))
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(level.color_hex()))
                                    .child(level.label()),
                            ),
                    ),
            )
            // 2. 硬件概览卡片矩阵 (CPU / 内存 / GPU / 推理模式)
            .child(self.render_hardware_overview_cards(&hw, cx))
            // 3. 跑分与检测操作栏
            .child(self.render_benchmark_and_actions_bar(&bench, is_benchmarking, cx))
            // 4. 用户策略选择器 (速度优先 / 平衡 / 精度优先)
            .child(self.render_strategy_selector(strategy, cx))
            // 5. 当前推荐推理决策面板
            .child(self.render_recommended_profile_panel(&profile, level, strategy, cx))
    }

    /// 渲染硬件信息 4 格卡片 (使用 Flex 4 列横排，严格限制 min_w_0 防止长文字溢出)
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

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::text_secondary())
                    .child("硬件规格概览"),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2p5()
                    // 卡片 1: CPU
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_muted())
                                    .child("CPU 处理器"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .overflow_hidden()
                                    .child(hw.cpu_brand.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(Theme::accent_mint())
                                    .overflow_hidden()
                                    .child(format!(
                                        "{} 核 / {} 线程 ({:.1} GHz)",
                                        hw.physical_cores,
                                        hw.logical_threads,
                                        hw.cpu_frequency_mhz as f64 / 1000.0
                                    )),
                            ),
                    )
                    // 卡片 2: 内存
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_muted())
                                    .child("系统内存 (RAM)"),
                            )
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child(hw.formatted_total_memory()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(Theme::text_secondary())
                                    .child(format!("可用: {}", hw.formatted_available_memory())),
                            ),
                    )
                    // 卡片 3: GPU 图形
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_muted())
                                    .child("GPU 硬件加速"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .overflow_hidden()
                                    .child(primary_gpu),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(if hw.gpus.iter().any(|g| g.is_discrete) {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_muted()
                                    })
                                    .child(if hw.gpus.iter().any(|g| g.is_discrete) {
                                        "独立显卡加速"
                                    } else {
                                        "集成核显 / 软件后端"
                                    }),
                            ),
                    )
                    // 卡片 4: 推理模式
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .flex()
                            .flex_col()
                            .justify_between()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_muted())
                                    .child("当前推理模式"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_primary())
                                    .overflow_hidden()
                                    .child(hw.inference_mode.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(Theme::text_muted())
                                    .child("AVX2 / FMA 向量指令集优化"),
                            ),
                    ),
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
            .p_3()
            .rounded_lg()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child("CPU 性能压测吞吐:"),
                    )
                    .child(
                        if let Some(res) = bench {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_mint())
                                        .child(format!("{} 分 ({:.1} GFLOPS)", res.score, res.gflops_estimate)),
                                )
                                .child(
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .bg(rgb(0x282834))
                                        .text_size(px(10.5))
                                        .text_color(Theme::text_secondary())
                                        .child(format!("评级: {} (耗时 {} ms)", res.throughput_rating, res.duration_ms)),
                                )
                        } else if is_benchmarking {
                            div()
                                .text_size(px(11.5))
                                .text_color(Theme::accent_primary())
                                .child("正在执行多核矩阵密集型计算压测中...")
                        } else {
                            div()
                                .text_size(px(11.5))
                                .text_color(Theme::text_muted())
                                .child("尚未运行基准测试，点击右侧按钮测试本机实际算力")
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
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x252530))
                            .border_1()
                            .border_color(rgb(0x353545))
                            .text_size(px(11.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(0x303040)))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.refresh_hardware_detection();
                                cx.notify();
                            }))
                            .child("重新检测硬件"),
                    )
                    // 运行性能测试
                    .child(
                        div()
                            .id("btn-run-benchmark")
                            .px_3()
                            .py_1()
                            .rounded_md()
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
                                    s.opacity(0.9)
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

    /// 渲染用户策略切换卡片 (极简风格，无 Emoji)
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
                "极速交付 · 轻量模型",
                UserStrategy::Speed.description(),
            ),
            (
                UserStrategy::Balanced,
                "strategy-card-balanced",
                "平衡模式 (推荐)",
                "兼顾速度 · 高准确度",
                UserStrategy::Balanced.description(),
            ),
            (
                UserStrategy::Quality,
                "strategy-card-quality",
                "精度优先",
                "高精大模型 · 深度润色",
                UserStrategy::Quality.description(),
            ),
        ];

        div()
            .w_full()
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
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_secondary())
                            .child("用户推理偏好策略"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(Theme::text_muted())
                            .child("最终配置 = [硬件综合等级] × [用户偏好策略] 共同决策"),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap_2p5()
                    .children(strategies.into_iter().map(|(strat, card_id, title, subtitle, desc)| {
                        let is_active = current == strat;
                        div()
                            .id(card_id)
                            .flex_1()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .cursor_pointer()
                            .bg(if is_active {
                                rgb(0x1e1e2a)
                            } else {
                                Theme::bg_card()
                            })
                            .border_1()
                            .border_color(if is_active {
                                Theme::accent_primary()
                            } else {
                                Theme::border()
                            })
                            .hover(move |s| {
                                if !is_active {
                                    s.bg(Theme::bg_hover())
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
                            .gap_1p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(12.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if is_active {
                                                Theme::accent_primary()
                                            } else {
                                                Theme::text_primary()
                                            })
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.5))
                                            .text_color(Theme::text_muted())
                                            .child(subtitle),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .line_height(relative(1.35))
                                    .text_color(Theme::text_secondary())
                                    .child(desc),
                            )
                    })),
            )
    }

    /// 渲染推荐推理配置看板 (固定容器防溢出，极简现代设计)
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
            .gap_2p5()
            .p_3p5()
            .rounded_lg()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(rgb(0x303042))
            // 顶部横幅
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_2p5()
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("当前推荐推理配置"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(rgb(0x222230))
                                    .text_size(px(10.5))
                                    .text_color(Theme::accent_mint())
                                    .child(format!("{} + {}", level.code(), strategy.label())),
                            ),
                    )
                    .child(
                        div()
                            .id("btn-apply-recommended-profile")
                            .px_3p5()
                            .py_1()
                            .rounded_md()
                            .bg(Theme::accent_primary())
                            .text_size(px(11.5))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0xffffff))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.apply_recommended_profile();
                                cx.notify();
                            }))
                            .child("应用推荐配置到当前系统"),
                    ),
            )
            // 决策参数网格 (2 行 × 3 列，每个格都有 min_w_0)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_2()
                    // 第 1 行
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap_2p5()
                            // 1. Whisper 模型
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("Whisper 语音模型"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).overflow_hidden().child(profile.whisper_model_name)),
                            )
                            // 2. Whisper 线程数
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("Whisper CPU 线程数"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{} 线程", profile.whisper_threads))),
                            )
                            // 3. VAD 静音加速
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("VAD 静音检测加速"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(if profile.enable_vad { Theme::accent_mint() } else { Theme::text_muted() }).child(if profile.enable_vad { "启用 (跳过无声片段)" } else { "禁用 (全音频扫描)" })),
                            ),
                    )
                    // 第 2 行
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .gap_2p5()
                            // 4. LLM 模型
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("LLM 纠错与标点模型"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).overflow_hidden().child(profile.llm_model_name)),
                            )
                            // 5. LLM 线程数
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("LLM 推理线程数"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{} 线程", profile.llm_threads))),
                            )
                            // 6. 并发与 GPU Offload
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .p_2p5()
                                    .rounded_md()
                                    .bg(rgb(0x161620))
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("GPU Offload / 并发任务"))
                                    .child(div().text_size(px(12.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(format!("{} / {} 个任务", if profile.gpu_offload { "启用 GPU" } else { "纯 CPU 计算" }, profile.max_concurrency))),
                            ),
                    ),
            )
            // 决策理由阐述
            .child(
                div()
                    .w_full()
                    .p_2p5()
                    .rounded_md()
                    .bg(rgb(0x1c1c26))
                    .border_1()
                    .border_color(rgb(0x282838))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::accent_primary())
                            .child("决策理由:"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .child(profile.recommendation_reason.clone()),
                    ),
            )
    }
}
