//! 右侧转写参数配置抽屉及硬件监控部件
//!
//! 结构：标题 → 文件卡片 → 识别引擎档位栅格 → 转写参数（语言/格式/润色/线程）→ 硬件监控 → 主操作 CTA

use gpui::prelude::*;
use gpui::*;

use crate::app::state::ProcessStatus;
use crate::app::{PolishMode, WhisperModelTier};
use crate::engines::MAX_SPEAKERS;
use crate::utils::time::format_duration_short;
use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;

/// 说话人数量预设表：`(选项值, 显示名)`，从 2 人起（引擎收敛区间的下限也是 2）。
/// 渲染时只取前 `MAX_SPEAKERS - 1` 项，所以这张表只需要「够长」，多余的项不会出现在界面上。
const SPEAKER_PRESETS: [(&str, &str); 6] = [
    ("2", "2 人"),
    ("3", "3 人"),
    ("4", "4 人"),
    ("5", "5 人"),
    ("6", "6 人"),
    ("7", "7 人"),
];

/// 编译期校验：预设表必须覆盖 `2..=MAX_SPEAKERS`。
///
/// 这条约束以前写成运行时的 `debug_assert_eq!`，而且判定条件本身写错了
/// （拿「选项数 - 1」去比 `MAX_SPEAKERS`，等于要求界面提供 2..=5 人）。
/// 结果每次点开「视频转写」渲染右侧配置面板都会 panic，程序直接退出。
/// 改成 const 断言后，条件不对是编译不过，而不是用户点一下界面就崩。
const _: () = assert!(
    MAX_SPEAKERS >= 2 && (MAX_SPEAKERS as usize) - 1 <= SPEAKER_PRESETS.len(),
    "SPEAKER_PRESETS 条目数不足以覆盖 2..=MAX_SPEAKERS，请补齐后重试"
);

/// 说话人分离的选项列表：`关闭` + `2..=MAX_SPEAKERS` 人。
fn speaker_options() -> Vec<(&'static str, &'static str)> {
    let mut options = Vec::with_capacity(MAX_SPEAKERS as usize);
    options.push(("off", "关闭"));
    options.extend(SPEAKER_PRESETS[..(MAX_SPEAKERS as usize) - 1].iter().copied());
    options
}

impl MainWindow {
    /// 渲染右侧配置面板 (转写配置与选项：左中右架构之「右」)
    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_sv = self.state.whisper_model_tier == WhisperModelTier::SenseVoice;
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        // 批量队列非空时，侧栏 CTA 的主语义从「转写这一个」变成「把这一批跑完」
        let has_batch = !self.state.batch_queue.is_empty();
        let batch_running = self.state.batch_running;
        let can_act = has_file || has_batch;
        let fname = self.state.transcribe_file.as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("未选择文件")
            .to_string();
        let dur_str = format_duration_short(self.state.transcribe_duration);

        div()
            .id("sidebar")
            .w(px(Theme::DRAWER_W))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .bg(Theme::bg_sidebar())
            .border_l_1()
            .border_color(Theme::border())
            .p(px(Theme::PAGE_PAD))
            .flex()
            .flex_col()
            .gap(px(Theme::CARD_GAP))
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
                                primitives::stat_dot(if is_processing {
                                    Theme::accent_orange()
                                } else {
                                    Theme::accent_mint()
                                }),
                            )
                            .child(
                                primitives::panel_title("转写配置"),
                            ),
                    )
                    .child(
                        primitives::badge(if is_sv { "SenseVoice" } else { "Whisper" }),
                    ),
            )
            // ── 2. 媒体文件选择与状态卡片 ──
            .child(
                primitives::card_sm()
                    .id("media-select-card")
                    // card_sm 默认纵向排布；这一张是「图标 + 文件名 + 按钮」的横排卡片
                    .flex_row()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(if has_file { Theme::tint_mint_border() } else { Theme::border() })
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::bg_card_hover()).border_color(Theme::border_light()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_file(cx);
                    }))
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_3))
                    .child(
                        div()
                            .w(px(Theme::CTRL_H_LG))
                            .h(px(Theme::CTRL_H_LG))
                            .rounded(px(Theme::RADIUS_LG))
                            .bg(if has_file { Theme::tint_blue_soft() } else { Theme::tint_mint_soft() })
                            .border_1()
                            .border_color(if has_file { Theme::tint_blue_border() } else { Theme::tint_mint_border() })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_BODY))
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
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child(if has_file { fname } else { "选择音视频文件".to_string() }),
                            )
                            .children(if has_file {
                                Some(
                                    div()
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .text_color(Theme::accent_mint())
                                        .child(format!("时长: {}", dur_str)),
                                )
                            } else {
                                None
                            }),
                    )
                    .child(
                        div()
                            .h(px(Theme::CHIP_H))
                            .px(px(Theme::CHIP_PAD_X))
                            .rounded(px(Theme::RADIUS_SM))
                            .flex()
                            .items_center()
                            .bg(if has_file { Theme::bg_card_hover() } else { Theme::accent_mint() })
                            .border_1()
                            .border_color(if has_file { Theme::border_strong() } else { Theme::tint_mint_border() })
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file { Theme::text_secondary() } else { Theme::text_on_accent() })
                            .child(if has_file { "更换" } else { "浏览" }),
                    ),
            )
            // ── 3. 识别引擎档位栅格 ──
            .child(self.render_engine_card(cx))
            // ── 4. 转写参数（语言 / 格式 / 润色 / 线程）──
            .child(self.render_params_card(cx))
            // ── 5. 硬件监控看板 ──
            .child(self.render_hardware_monitor_card(cx))
            // ── 5.5 模型缺失引导（仅在缺组件时出现，就绪后自动消失）──
            .children({
                let missing = self.state.missing_model_count();
                if missing > 0 {
                    Some(self.render_model_manager(true, cx))
                } else {
                    None
                }
            })
            // ── 6. 底部主操作 CTA 按钮 (工程级突出呈现) ──
            .child(
                primitives::btn(
                    if is_processing {
                        if batch_running { "终止批量" } else { "终止转写" }
                    } else if has_file {
                        "开始转写"
                    } else if has_batch {
                        "开始全部"
                    } else {
                        "选择文件并转写"
                    },
                    primitives::BtnSize::Lg,
                    primitives::BtnVariant::Primary,
                )
                    .id("sidebar-primary-cta-btn")
                    .w_full()
                    .gap(px(Theme::SPACE_2))
                    .bg(if is_processing {
                        Theme::accent_red_strong()
                    } else if can_act {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_disabled()
                    })
                    .border_1()
                    .border_color(if is_processing {
                        Theme::tint_red_border()
                    } else if can_act {
                        Theme::tint_mint_border()
                    } else {
                        Theme::border_strong()
                    })
                    .text_color(if is_processing {
                        // 转写中按钮是深红实心块，文字恒白
                        Theme::text_on_saturated()
                    } else if can_act {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_muted()
                    })
                    .hover(move |s| {
                        if can_act || is_processing { s.opacity(0.9) } else { s }
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        let processing = matches!(this.state.status, ProcessStatus::Processing { .. });
                        if processing {
                            // 真正终止：标记取消请求并强杀识别子进程，事件回传后由收尾逻辑复位状态。
                            // 批量模式下同时停掉续跑，避免杀完当前项又自动开下一个。
                            if this.state.batch_running {
                                this.cancel_batch_queue(cx);
                            } else {
                                this.request_cancel_processing(cx);
                            }
                        } else if this.state.transcribe_file.is_some() {
                            this.start_processing(cx);
                        } else if !this.state.batch_queue.is_empty() {
                            this.start_batch_queue(cx);
                        } else {
                            this.choose_file(cx);
                        }
                    }))
            )
    }

    /// 「识别引擎」卡片：五档模型栅格（SenseVoice 独占整行）
    fn render_engine_card(&mut self, cx: &mut Context<Self>) -> Div {
        let tiers: [(WhisperModelTier, &'static str, &'static str); 4] = [
            (WhisperModelTier::Fast, "Base", "20x · 最省资源"),
            (WhisperModelTier::Balanced, "Small-Q5", "8x · 纯 CPU 友好"),
            (WhisperModelTier::TurboSpeed, "Turbo Q5", "6x · 推荐"),
            (WhisperModelTier::Precise, "Turbo Q8", "4x · 最准"),
        ];

        let mut grid = div().flex().flex_wrap().gap(px(Theme::SPACE_1_5));
        grid = grid.child(self.tier_pill(
            WhisperModelTier::SenseVoice,
            "SenseVoice 极速",
            "42x · 自带标点与数字规范",
            true,
            cx,
        ));
        for (tier, name, speed) in tiers {
            grid = grid.child(self.tier_pill(tier, name, speed, false, cx));
        }

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(primitives::section_title("识别引擎"))
            .child(grid)
    }

    /// 模型档位选择胶囊：档位名 + 速度说明两行。
    ///
    /// 速度是选档位的唯一依据，去掉副标题后用户只能凭名字猜（「Turbo Q8 比 Q5 快还是慢？」），
    /// 所以这一行必须留着；档位名用主字色、速度说明用弱化色，扫一眼就能比较。
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
            .h(px(Theme::TIER_PILL_H))
            .px(px(Theme::SPACE_2))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(Theme::SPACE_1))
            .rounded(px(Theme::RADIUS_LG))
            .border_1()
            .border_color(if is_sel { Theme::tint_blue_border() } else { Theme::transparent() })
            .bg(if is_sel { Theme::tint_blue_badge() } else { Theme::bg_raised() })
            .cursor_pointer()
            .hover(move |s| if is_sel { s } else { s.bg(Theme::tint_neutral()) })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.whisper_model_tier = tier;
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(if is_sel { FontWeight::BOLD } else { FontWeight::MEDIUM })
                    .text_color(if is_sel { Theme::accent_blue() } else { Theme::text_primary() })
                    .child(name),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if is_sel { Theme::accent_blue() } else { Theme::text_muted() })
                    .child(speed),
            );
        if full_width {
            pill.w_full()
        } else {
            pill.flex_1().min_w(px(Theme::SEG_PILL_MIN_W))
        }
    }

    /// 「转写参数」卡片：识别语言 / 输出格式 / 标点润色 / 转写线程
    fn render_params_card(&mut self, cx: &mut Context<Self>) -> Div {
        let lang_sel = match self.state.language.as_str() {
            "zh" => "zh",
            "en" => "en",
            _ => "auto",
        };
        // 兜底回落到 "srt" 之前，必须把新增的专业格式也列出来——否则 output_format
        // 明明是 json/ttml/ttal，界面上却高亮 SRT，用户以为设置没生效。
        let fmt_sel = match self.state.output_format.as_str() {
            "vtt" => "vtt",
            "ass" => "ass",
            "txt" => "txt",
            "json" => "json",
            "ttml" => "ttml",
            "ttal" => "ttal",
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
        // 说话人数量：选项与收敛区间都以 diarization::MAX_SPEAKERS 为准，
        // 不在这里另写一份字面量。引擎把人数收敛到 2..=MAX_SPEAKERS，
        // UI 若还留着旧的固定 4，用户选了也白选。
        let speaker_options = speaker_options();
        let speaker_sel = if !self.state.config.pipeline.enable_diarization {
            "off".to_string()
        } else {
            self.state
                .config
                .pipeline
                .speaker_count
                .clamp(2, MAX_SPEAKERS)
                .to_string()
        };
        let gpu_sel = self.state.gpu_mode.clone();
        let gpu_limit_sel = self.state.config.gpu.effective_gpu_limit().to_string();
        let audio_speed_sel = format!("{:.2}", self.state.config.pipeline.whisper_audio_speed);
        let vad_threshold_sel = format!("{:.2}", self.state.config.pipeline.whisper_vad_threshold);

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(self.param_group(
                "GPU 占用策略",
                vec![
                    ("full", "全速 GPU"),
                    ("balanced", "GPU 让路"),
                    ("eco", "低占用 GPU"),
                    ("cpu", "纯 CPU"),
                ],
                &gpu_sel,
                |this, sel, cx| {
                    this.state.gpu_mode = sel.to_string();
                    this.state.config.gpu = crate::utils::config::GpuConfig::from_mode(sel);
                    // 让路开关是**进程级全局量**，只在启动时下发一次。用户改档位后
                    // 必须立即重下发，否则「保存了但本进程仍按旧值跑」，要重启才生效。
                    crate::engines::media_pipeline::set_yield_to_desktop(
                        this.state.config.gpu.yield_to_desktop,
                    );
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                },
                cx,
            ))
            .child(if gpu_sel == "eco" {
                self.param_group(
                    "GPU 占用上限",
                    vec![("40", "40%"), ("60", "60%"), ("80", "80%")],
                    &gpu_limit_sel,
                    |this, sel, cx| {
                        this.state.config.gpu.gpu_limit_percent = sel.parse::<u32>().unwrap_or(60);
                        let _ = this.state.config.save_to_file("config.toml");
                        cx.notify();
                    },
                    cx,
                )
            } else {
                div()
            })
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
                vec![
                    ("srt", "SRT"),
                    ("vtt", "VTT"),
                    ("ass", "ASS"),
                    ("txt", "TXT"),
                    ("json", "JSON"),
                    ("ttml", "EBU-TT-D"),
                    ("ttal", "Netflix TTAL"),
                ],
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
                "说话人分离",
                speaker_options,
                &speaker_sel,
                |this, sel, cx| {
                    match sel {
                        "off" => this.state.config.pipeline.enable_diarization = false,
                        n => {
                            this.state.config.pipeline.enable_diarization = true;
                            this.state.config.pipeline.speaker_count = n.parse::<u32>().unwrap_or(2);
                        }
                    }
                    let _ = this.state.config.save_to_file("config.toml");
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
            .child(self.param_group(
                "音频加速",
                vec![("1.00", "关闭"), ("1.15", "1.15x"), ("1.25", "1.25x"), ("1.35", "1.35x"), ("1.50", "1.50x")],
                &audio_speed_sel,
                |this, sel, cx| {
                    this.state.config.pipeline.whisper_audio_speed = sel.parse::<f64>().unwrap_or(1.0);
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "Whisper VAD 阈值",
                vec![("0.50", "标准"), ("0.55", "稍积极"), ("0.60", "积极")],
                &vad_threshold_sel,
                |this, sel, cx| {
                    this.state.config.pipeline.whisper_vad_threshold = sel.parse::<f64>().unwrap_or(0.50);
                    let _ = this.state.config.save_to_file("config.toml");
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
            .gap(px(Theme::SPACE_1))
            .child(primitives::field_label(label))
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
        let mut row = primitives::segmented_row();
        for (key, label) in options {
            let is_sel = key == selected;
            row = row.child(
                primitives::segmented(label, is_sel, true)
                    .id(key)
                    .on_click(cx.listener(move |this, _, _, cx| on_select(this, key, cx))),
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

        primitives::card_sm()
            .id("hardware-monitor-card")
            // 标头行：标题 + 状态胶囊
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        primitives::field_label("系统监控"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                primitives::stat_dot_sm(if m.is_model_running {
                                    Theme::accent_mint()
                                } else {
                                    Theme::text_muted()
                                }),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
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
                            .text_size(px(Theme::TEXT_SMALL))
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
                        primitives::meter_bar(
                            sys_cpu_pct / 100.0,
                            proc_cpu_pct / 100.0,
                            Theme::accent_mint(),
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
                            .text_size(px(Theme::TEXT_SMALL))
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
                        primitives::meter_bar(
                            sys_mem_pct / 100.0,
                            proc_mem_pct / 100.0,
                            Theme::accent_blue(),
                        ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // 这里刻意不用 `use super::*`：父模块的 `use gpui::*` 会把 gpui 自带的
    // `test` 属性宏带进来，遮蔽内建的 `#[test]`，导致展开递归超限。
    use super::speaker_options;
    use crate::engines::MAX_SPEAKERS;

    /// 回归测试：2026-09-28 两次「点开视频转写就闪退」的根因，就是这里少了一项——
    /// 选项表写成固定的 4 项（关闭 + 2/3/4 人），而当时那句断言要求 5 项，
    /// 于是每次渲染右侧配置面板都 panic。
    /// 这里把「选项必须完整覆盖引擎的 2..=MAX_SPEAKERS」钉住。
    #[test]
    fn speaker_options_cover_engine_range() {
        let options = speaker_options();

        assert_eq!(
            options.len(),
            MAX_SPEAKERS as usize,
            "选项应为「关闭」+ 2..=MAX_SPEAKERS，共 MAX_SPEAKERS 项"
        );
        assert_eq!(options[0], ("off", "关闭"));

        for (i, (key, label)) in options[1..].iter().enumerate() {
            let count = i + 2;
            assert_eq!(*key, count.to_string(), "第 {i} 项的选项值应为 {count}");
            assert_eq!(*label, format!("{count} 人"), "第 {i} 项的显示名不匹配");
        }

        // 配置里存的人数必须能在选项表里找到，否则胶囊会全部处于未选中态
        let selected = 2u32.clamp(2, MAX_SPEAKERS).to_string();
        assert!(options.iter().any(|(key, _)| *key == selected));
    }
}
