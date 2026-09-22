//! 右侧转写参数配置抽屉及硬件监控部件
//!
//! 结构：标题 → 文件卡片 → 识别引擎档位栅格 → 转写参数（语言/格式/润色/线程）→ 硬件监控 → 主操作 CTA

use gpui::prelude::*;
use gpui::*;

use crate::app::state::ProcessStatus;
use crate::app::{PolishMode, WhisperModelTier};
use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染右侧配置面板 (转写配置与选项：左中右架构之「右」)
    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == WhisperModelTier::SenseVoice;
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        let fname = self.state.transcribe_file.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("未选择文件")
            .to_string();
        let dur_str = format_duration_short(self.state.transcribe_duration);

        div()
            .id("sidebar")
            .w(px(350.0))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .bg(Theme::bg_sidebar())
            .border_l_1()
            .border_color(Theme::border())
            .p_4()
            .flex()
            .flex_col()
            .gap_2p5()
            // ── 1. 顶栏标头与引擎指示 ──
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .rounded_full()
                                    .bg(if is_processing { Theme::accent_orange() } else { Theme::accent_mint() }),
                            )
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("转写配置"),
                            ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_full()
                            .bg(rgb(0x1a1a24))
                            .border_1()
                            .border_color(rgb(0x2d2d3a))
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_secondary())
                            .child(if is_sv { "SenseVoice" } else { "Whisper" }),
                    ),
            )
            // ── 2. 媒体文件选择与状态卡片 ──
            .child(
                div()
                    .id("media-select-card")
                    .p_3()
                    .rounded_xl()
                    .bg(rgb(0x181820))
                    .border_1()
                    .border_color(if has_file { rgba(0x10b98144) } else { Theme::border() })
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x1f1f2a)).border_color(Theme::border_light()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_file(cx);
                    }))
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .w(px(38.0))
                            .h(px(38.0))
                            .rounded_lg()
                            .bg(if has_file { rgba(0x38bdf818) } else { rgba(0x10b98114) })
                            .border_1()
                            .border_color(if has_file { rgba(0x38bdf833) } else { rgba(0x10b98133) })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file { Theme::accent_blue() } else { Theme::accent_mint() })
                            .child(if has_file { "FILE" } else { "+" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child(if has_file { fname } else { "选择音视频文件".to_string() }),
                            )
                            .children(if has_file {
                                Some(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(Theme::accent_mint())
                                        .child(format!("时长: {}", dur_str)),
                                )
                            } else {
                                None
                            }),
                    )
                    .child(
                        div()
                            .px_2p5()
                            .py_1()
                            .rounded_md()
                            .bg(if has_file { rgb(0x252532) } else { Theme::accent_mint() })
                            .border_1()
                            .border_color(if has_file { rgb(0x363646) } else { rgba(0x10b98166) })
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file { Theme::text_secondary() } else { rgb(0x09090b) })
                            .child(if has_file { "更换" } else { "浏览" }),
                    ),
            )
            // ── 3. 识别引擎档位栅格 ──
            .child(self.render_engine_card(cx))
            // ── 4. 转写参数（语言 / 格式 / 润色 / 线程）──
            .child(self.render_params_card(cx))
            // ── 5. 硬件监控看板 ──
            .child(self.render_hardware_monitor_card(cx))
            // ── 6. 底部主操作 CTA 按钮 (工程级突出呈现) ──
            .child(
                div()
                    .id("sidebar-primary-cta-btn")
                    .w_full()
                    .h(px(42.0))
                    .rounded_xl()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .bg(if is_processing {
                        rgb(0xe11d48)
                    } else if has_file {
                        Theme::accent_mint()
                    } else {
                        rgb(0x23232f)
                    })
                    .border_1()
                    .border_color(if is_processing {
                        rgba(0xf43f5e66)
                    } else if has_file {
                        rgba(0x10b98188)
                    } else {
                        rgb(0x323242)
                    })
                    .text_size(px(13.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if is_processing {
                        rgb(0xffffff)
                    } else if has_file {
                        rgb(0x09090b)
                    } else {
                        Theme::text_muted()
                    })
                    .hover(move |s| {
                        if has_file || is_processing { s.opacity(0.9) } else { s }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let processing = matches!(this.state.status, ProcessStatus::Processing { .. });
                        if processing {
                            // 真正终止：标记取消请求并强杀识别子进程，事件回传后由收尾逻辑复位状态
                            this.state.cancel_requested = true;
                            this.state.status = ProcessStatus::Processing {
                                stage: "终止中".to_string(),
                                progress: 1.0,
                                detail: "正在终止识别进程，请稍候...".to_string(),
                            };
                            this.state.pipeline.cancel();
                            cx.notify();
                        } else if this.state.transcribe_file.is_some() {
                            this.start_processing(cx);
                        } else {
                            this.choose_file(cx);
                        }
                    }))
                    .child(if is_processing {
                        "终止转写"
                    } else if has_file {
                        "开始转写"
                    } else {
                        "选择文件并转写"
                    }),
            )
    }

    /// 「识别引擎」卡片：五档模型栅格（SenseVoice 独占整行）+ 高级参数入口
    fn render_engine_card(&mut self, cx: &mut Context<Self>) -> Div {
        let tiers: [(WhisperModelTier, &'static str, &'static str); 4] = [
            (WhisperModelTier::Fast, "Base", "20x 倍速 · 轻量"),
            (WhisperModelTier::Balanced, "Small", "7x 倍速 · 均衡"),
            (WhisperModelTier::TurboSpeed, "Turbo Q5", "6x 倍速 · 推荐"),
            (WhisperModelTier::Precise, "Turbo Q8", "4x 倍速 · 高精"),
        ];

        let mut grid = div().flex().flex_wrap().gap_1p5();
        grid = grid.child(self.tier_pill(
            WhisperModelTier::SenseVoice,
            "SenseVoice 极速",
            "42x 极速 · 自带标点与数字规范",
            true,
            cx,
        ));
        for (tier, name, speed) in tiers {
            grid = grid.child(self.tier_pill(tier, name, speed, false, cx));
        }

        div()
            .p_3()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
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
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_secondary())
                            .child("识别引擎"),
                    )
                    .child(
                        div()
                            .id("btn-nav-to-performance")
                            .cursor_pointer()
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::accent_mint())
                            .hover(|s| s.text_color(Theme::accent_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = crate::app::WorkspaceTab::Performance;
                                cx.notify();
                            }))
                            .child("高级推理参数 →"),
                    ),
            )
            .child(grid)
    }

    /// 模型档位选择胶囊：两行布局（档位名 + 速度提示）
    fn tier_pill(
        &mut self,
        tier: WhisperModelTier,
        name: &'static str,
        speed: &'static str,
        full_width: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_sel = self.state.whisper_model_tier == tier;
        let pill = div()
            .id(name)
            .h(px(42.0))
            .px_2()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_0p5()
            .rounded_lg()
            .border_1()
            .border_color(if is_sel { rgba(0x38bdf888) } else { rgba(0x00000000) })
            .bg(if is_sel { rgba(0x38bdf826) } else { rgb(0x1b1b24) })
            .cursor_pointer()
            .hover(move |s| if is_sel { s } else { s.bg(rgba(0xffffff0d)) })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.whisper_model_tier = tier;
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(if is_sel { FontWeight::BOLD } else { FontWeight::MEDIUM })
                    .text_color(if is_sel { Theme::accent_blue() } else { Theme::text_primary() })
                    .child(name),
            )
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(if is_sel { rgba(0x38bdf8aa) } else { Theme::text_muted() })
                    .child(speed),
            );
        if full_width {
            pill.w_full()
        } else {
            pill.flex_1().min_w(px(140.0))
        }
    }

    /// 「转写参数」卡片：识别语言 / 输出格式 / 标点润色 / 转写线程
    fn render_params_card(&mut self, cx: &mut Context<Self>) -> Div {
        let lang_sel = match self.state.language.as_str() {
            "zh" => "zh",
            "en" => "en",
            _ => "auto",
        };
        let fmt_sel = match self.state.output_format.as_str() {
            "vtt" => "vtt",
            "ass" => "ass",
            "txt" => "txt",
            _ => "srt",
        };
        let polish_sel = if !self.state.enable_polish {
            "off"
        } else if self.state.polish_mode == PolishMode::PuncFast {
            "punc"
        } else {
            "qwen"
        };
        let thread_sel = self.state.whisper_threads.to_string();

        div()
            .p_3()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .gap_2p5()
            .child(self.param_group(
                "识别语言",
                vec![("auto", "自动"), ("zh", "中文"), ("en", "English")],
                lang_sel,
                |this, sel, cx| {
                    this.state.language = sel.to_string();
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "字幕输出格式",
                vec![("srt", "SRT"), ("vtt", "VTT"), ("ass", "ASS"), ("txt", "TXT")],
                fmt_sel,
                |this, sel, cx| {
                    this.state.output_format = sel.to_string();
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "标点与润色",
                vec![("off", "关闭"), ("punc", "极速标点"), ("qwen", "Qwen 润色")],
                polish_sel,
                |this, sel, cx| {
                    match sel {
                        "punc" => {
                            this.state.enable_polish = true;
                            this.state.polish_mode = PolishMode::PuncFast;
                        }
                        "qwen" => {
                            this.state.enable_polish = true;
                            this.state.polish_mode = PolishMode::QwenDeep;
                        }
                        _ => {
                            this.state.enable_polish = false;
                        }
                    }
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "转写线程",
                vec![("4", "4 线程"), ("8", "8 线程"), ("16", "16 线程")],
                &thread_sel,
                |this, sel, cx| {
                    this.state.whisper_threads = sel.parse::<u32>().unwrap_or(8);
                    cx.notify();
                },
                cx,
            ))
    }

    /// 参数分组：小标题 + 分段选择器行
    fn param_group(
        &mut self,
        label: &'static str,
        options: Vec<(&'static str, &'static str)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &'static str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_muted())
                    .child(label),
            )
            .child(self.pill_row(options, selected, on_select, cx))
    }

    /// 通用分段选择器：一行等宽胶囊，单击切换
    fn pill_row(
        &mut self,
        options: Vec<(&'static str, &'static str)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &'static str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut row = div().flex().w_full().gap_1p5();
        for (key, label) in options {
            let is_sel = key == selected;
            row = row.child(
                div()
                    .id(key)
                    .flex_1()
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_sel { rgba(0x38bdf866) } else { rgba(0x00000000) })
                    .bg(if is_sel { rgba(0x38bdf81f) } else { rgb(0x1b1b24) })
                    .text_size(px(11.0))
                    .cursor_pointer()
                    .font_weight(if is_sel { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                    .text_color(if is_sel { Theme::accent_blue() } else { Theme::text_secondary() })
                    .hover(move |s| {
                        if is_sel {
                            s
                        } else {
                            s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| on_select(this, key, cx)))
                    .child(label),
            );
        }
        row
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
}
