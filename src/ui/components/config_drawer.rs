//! 右侧转写参数配置抽屉及硬件监控部件

use gpui::prelude::*;
use gpui::*;

use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染右侧配置面板 (转写配置与选项：左中右架构之「右」)
    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar")
            .w(px(310.0))
            .h_full()
            .bg(Theme::bg_sidebar())
            .border_l_1()
            .border_color(Theme::border())
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            // 面板标头
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_primary())
                            .child("转写配置"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_muted())
                            .child("Whisper + LLM"),
                    ),
            )
            // 媒体文件卡片 (iOS Inset Card)
            .child(
                div()
                    .id("media-select-card")
                    .p_3()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::bg_hover()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_file(cx);
                    }))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(36.0))
                            .h(px(36.0))
                            .rounded_lg()
                            .bg(rgb(0x18181e))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_muted())
                            .child(if self.state.transcribe_file.is_some() { "FILE" } else { "+" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(match &self.state.transcribe_file {
                                        Some(path) => path
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("已选择文件")
                                            .to_string(),
                                        None => "选择音视频文件".to_string(),
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_muted())
                                    .child(if self.state.transcribe_file.is_some() {
                                        format_duration_short(self.state.transcribe_duration)
                                    } else {
                                        "点击导入媒体文件".to_string()
                                    }),
                            ),
                    ),
            )
            // 转写配置分组卡片 (iOS Inset Group)
            .child(
                div()
                    .p_3()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .flex_col()
                    .gap_3()
                    // ── 模型档位分段器 ──
                    .child({
                        let eta = self.state.whisper_eta_label();
                        div()
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
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("识别档位"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(Theme::accent_mint())
                                            .child(format!("预估 {eta}")),
                                    ),
                            )
                            .child(
                                div()
                                    .bg(rgb(0x131317))
                                    .p(px(2.5))
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x22222a))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_model_tier_pill(
                                        crate::app::WhisperModelTier::Fast,
                                        "极速 Base",
                                        cx,
                                    ))
                                    .child(self.render_model_tier_pill(
                                        crate::app::WhisperModelTier::Balanced,
                                        "均衡 Small",
                                        cx,
                                    ))
                                    .child(self.render_model_tier_pill(
                                        crate::app::WhisperModelTier::TurboSpeed,
                                        "极速 Turbo",
                                        cx,
                                    ))
                                    .child(self.render_model_tier_pill(
                                        crate::app::WhisperModelTier::Precise,
                                        "高精 Turbo",
                                        cx,
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(Theme::text_muted())
                                    .child(match self.state.whisper_model_tier {
                                        crate::app::WhisperModelTier::TurboSpeed => "Q5 量化：减小显存带宽瓶颈，提速 25%~30%，适合标准普通话",
                                        crate::app::WhisperModelTier::Precise => "Q8 旗舰：无损高精，专治口音、吞音、方言与教学专有名词",
                                        crate::app::WhisperModelTier::Balanced => "Small 模型：资源消耗适中，适合日常普通对话",
                                        crate::app::WhisperModelTier::Fast => "Base 模型：极小体积，适合极速生成粗略草稿",
                                    }),
                            )
                    })
                    // 语言分段器
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_secondary())
                                    .child("识别语言"),
                            )
                            .child(
                                div()
                                    .bg(rgb(0x131317))
                                    .p(px(2.5))
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x22222a))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_option_pill("zh", "中文", cx))
                                    .child(self.render_option_pill("en", "英文", cx))
                                    .child(self.render_option_pill("ja", "日文", cx))
                                    .child(self.render_option_pill("auto", "自动", cx)),
                            ),
                    )
                    // 导出格式分段器
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_secondary())
                                    .child("导出格式"),
                            )
                            .child(
                                div()
                                    .bg(rgb(0x131317))
                                    .p(px(2.5))
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x22222a))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_format_pill("srt", "SRT", cx))
                                    .child(self.render_format_pill("ass", "ASS", cx))
                                    .child(self.render_format_pill("txt", "TXT", cx)),
                            ),
                    )
                    // CPU 线程分段器
                    .child(
                        div()
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
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("并发核心"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(Theme::text_muted())
                                            .child(if self.state.hardware.use_gpu_pipeline() {
                                                "GPU 推理时线程收益较小"
                                            } else {
                                                "更多线程会缩短预估时间"
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .bg(rgb(0x131317))
                                    .p(px(2.5))
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(0x22222a))
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(self.render_thread_pill(4, "4核", cx))
                                    .child(self.render_thread_pill(8, "8核", cx))
                                    .child(self.render_thread_pill(12, "12核", cx))
                                    .child(self.render_thread_pill(16, "16核", cx)),
                            ),
                    )
                    // AI 润色开关行 (iOS Switch Row)
                    .child(
                        div()
                            .id("toggle-polish-btn")
                            .flex()
                            .items_center()
                            .justify_between()
                            .pt_1()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.enable_polish = !this.state.enable_polish;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_secondary())
                                    .child("标点与文本润色"),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(if self.state.enable_polish { Theme::accent_mint() } else { rgb(0x27272a) })
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(if self.state.enable_polish { rgb(0x09090b) } else { Theme::text_muted() })
                                    .child(if self.state.enable_polish { "开启" } else { "关闭" }),
                            ),
                    )
                    // 润色模式选择胶囊组 (CT-Punc 极速标点 vs Qwen 大模型深度润色)
                    .children(if self.state.enable_polish {
                        let is_punc = self.state.polish_mode == crate::app::PolishMode::PuncFast;
                        Some(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1p5()
                                .pt_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_muted())
                                                .child("润色引擎模式"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.0))
                                                .text_color(if is_punc { Theme::accent_mint() } else { rgb(0x38bdf8) })
                                                .child(if is_punc { "约 6 秒" } else { "约 5~6 分钟" }),
                                        ),
                                )
                                .child(
                                    div()
                                        .p(px(2.0))
                                        .rounded_lg()
                                        .bg(rgb(0x131317))
                                        .border_1()
                                        .border_color(rgb(0x22222a))
                                        .flex()
                                        .items_center()
                                        .gap(px(2.0))
                                        .child(
                                            div()
                                                .id("select-punc-fast-btn")
                                                .flex_1()
                                                .py_1()
                                                .rounded(px(5.0))
                                                .bg(if is_punc { rgb(0x1e2e28) } else { rgba(0x00000000) })
                                                .border_1()
                                                .border_color(if is_punc { rgba(0x10b98160) } else { rgba(0x00000000) })
                                                .cursor_pointer()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.polish_mode = crate::app::PolishMode::PuncFast;
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .font_weight(if is_punc { FontWeight::BOLD } else { FontWeight::NORMAL })
                                                        .text_color(if is_punc { Theme::accent_mint() } else { Theme::text_muted() })
                                                        .child("极速标点 (推荐)"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("select-qwen-deep-btn")
                                                .flex_1()
                                                .py_1()
                                                .rounded(px(5.0))
                                                .bg(if !is_punc { rgb(0x1e2433) } else { rgba(0x00000000) })
                                                .border_1()
                                                .border_color(if !is_punc { rgba(0x38bdf860) } else { rgba(0x00000000) })
                                                .cursor_pointer()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.polish_mode = crate::app::PolishMode::QwenDeep;
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .font_weight(if !is_punc { FontWeight::BOLD } else { FontWeight::NORMAL })
                                                        .text_color(if !is_punc { rgb(0x38bdf8) } else { Theme::text_muted() })
                                                        .child("Qwen 润色"),
                                                ),
                                        ),
                                ),
                        )
                    } else {
                        None
                    }),
            )
            // 硬件与本地模型资源监控对比卡片
            .child(self.render_hardware_monitor_card(cx))
    }

    /// 渲染硬件与模型资源监控对比卡片 (CPU / 内存实时对比)
    pub(crate) fn render_hardware_monitor_card(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.state.metrics;
        let sys_cpu_pct = m.sys_cpu.clamp(0.0, 100.0);
        let proc_cpu_pct = m.proc_cpu.clamp(0.0, 100.0);

        let sys_mem_gb = m.sys_mem_used as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_mem_gb = (m.sys_mem_total as f64 / (1024.0 * 1024.0 * 1024.0)).max(1.0);
        let sys_mem_pct = ((sys_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        let proc_mem_gb = m.proc_mem as f64 / (1024.0 * 1024.0 * 1024.0);
        let proc_mem_pct = ((proc_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        div()
            .id("hardware-monitor-card")
            .p_3()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .gap_2()
            // 标头行：标题 + 状态胶囊
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_muted())
                            .child("系统监控"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded(px(3.0))
                                    .bg(if m.is_model_running {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_muted()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if m.is_model_running {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_secondary()
                                    })
                                    .child(m.proc_name.clone()),
                            ),
                    ),
            )
            // 1. CPU 对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(if m.is_model_running {
                                        "CPU (大模型):"
                                    } else {
                                        "CPU (应用待机):"
                                    }))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("{:.1}%", proc_cpu_pct)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("系统: {:.1}%", sys_cpu_pct)),
                            ),
                    )
                    // 双层条
                    .child(
                        div()
                            .w_full()
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(rgb(0x18181c))
                            .relative()
                            .overflow_hidden()
                            // 系统占用槽 (暗深灰)
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((sys_cpu_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(rgb(0x4a4a58)),
                            )
                            // 模型进程高亮条 (鲜艳 Mint 绿)
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((proc_cpu_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(Theme::accent_mint()),
                            ),
                    ),
            )
            // 2. 内存对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(if m.is_model_running {
                                        "内存 (大模型):"
                                    } else {
                                        "内存 (应用待机):"
                                    }))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_blue())
                                            .child(crate::app::ResourceMetrics::format_bytes(m.proc_mem)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("{:.1} / {:.0} GB", sys_mem_gb, total_mem_gb)),
                            ),
                    )
                    // 双层内存条
                    .child(
                        div()
                            .w_full()
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(rgb(0x18181c))
                            .relative()
                            .overflow_hidden()
                            // 系统已用内存槽
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((sys_mem_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(rgb(0x4a4a58)),
                            )
                            // 模型占用内存条
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((proc_mem_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(Theme::accent_blue()),
                            ),
                    ),
            )
    }

    pub(crate) fn render_model_tier_pill(
        &mut self,
        tier: crate::app::WhisperModelTier,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.whisper_model_tier == tier;
        div()
            .id(label)
            .flex_1()
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected { rgb(0x2c2c36) } else { rgba(0x00000000) })
            .font_weight(if is_selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
            .text_color(if is_selected { rgb(0xffffff) } else { Theme::text_secondary() })
            .hover(move |s| {
                if !is_selected { s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary()) } else { s }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.whisper_model_tier = tier;
                cx.notify();
            }))
            .child(label)
    }

    pub(crate) fn render_option_pill(
        &mut self,
        val: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.language == val;
        div()
            .id(val)
            .flex_1()
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                rgb(0x2c2c36)
            } else {
                rgba(0x00000000)
            })
            .font_weight(if is_selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .text_color(if is_selected {
                rgb(0xffffff)
            } else {
                Theme::text_secondary()
            })
            .hover(move |s| {
                if !is_selected {
                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                } else {
                    s
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.language = val.to_string();
                cx.notify();
            }))
            .child(label)
    }

    pub(crate) fn render_format_pill(
        &mut self,
        val: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.output_format == val;
        div()
            .id(val)
            .flex_1()
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                rgb(0x2c2c36)
            } else {
                rgba(0x00000000)
            })
            .font_weight(if is_selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .text_color(if is_selected {
                rgb(0xffffff)
            } else {
                Theme::text_secondary()
            })
            .hover(move |s| {
                if !is_selected {
                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                } else {
                    s
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.output_format = val.to_string();
                cx.notify();
            }))
            .child(label)
    }

    pub(crate) fn render_thread_pill(
        &mut self,
        val: u32,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.whisper_threads == val;
        div()
            .id(label)
            .flex_1()
            .h(px(26.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                rgb(0x2c2c36)
            } else {
                rgba(0x00000000)
            })
            .font_weight(if is_selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .text_color(if is_selected {
                rgb(0xffffff)
            } else {
                Theme::text_secondary()
            })
            .hover(move |s| {
                if !is_selected {
                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                } else {
                    s
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.whisper_threads = val;
                cx.notify();
            }))
            .child(label)
    }
}
