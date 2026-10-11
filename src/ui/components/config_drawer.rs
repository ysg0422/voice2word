//! 右侧转写参数配置抽屉及硬件监控部件
//!
//! 结构：标题 → 文件卡片 → 识别引擎档位栅格 → 转写参数（语言/格式/润色/线程）→ 硬件监控 → 主操作 CTA

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::app::{PolishMode, WhisperModelTier};
use crate::engines::MAX_SPEAKERS;
use crate::utils::time::format_duration_short;

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
    options.extend(
        SPEAKER_PRESETS[..(MAX_SPEAKERS as usize) - 1]
            .iter()
            .copied(),
    );
    options
}

/// 「转写线程」候选项：与性能页滑条同口径（`2..=本机逻辑核心数`），
/// 取 2 的幂次常用档位，并显式并入滑条上限（核数）与配置里的当前值。
///
/// 为什么必须并入当前值：抽屉此前把选项写死成 4/8/16，而性能页滑条是按核数
/// 推导的（`views/performance.rs`），配置里可能是 12 这类不在旧表里的值；那时
/// 抽屉里三个胶囊全部处于未选中态，看起来像「什么都没选」，两处口径不一致。
/// 候选项与性能页共用同一个上限来源（[`crate::app::AppState::logical_cores`]）。
fn thread_presets(current: u32, cores: u32) -> Vec<u32> {
    let max = cores.max(2);
    let mut values: Vec<u32> = [2u32, 4, 8, 16, 32]
        .into_iter()
        .filter(|v| *v <= max)
        .collect();
    // 滑条能达到的上限档，保证抽屉里也能直接选到它
    values.push(max);
    // 配置里的当前值必须始终在列，否则胶囊会全部落空
    values.push(current.clamp(2, max));
    values.sort_unstable();
    values.dedup();
    values
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
        let fname = self
            .state
            .transcribe_file
            .as_ref()
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
                            .child(primitives::stat_dot(if is_processing {
                                Theme::accent_orange()
                            } else {
                                Theme::accent_mint()
                            }))
                            .child(primitives::panel_title("转写配置")),
                    )
                    .child(primitives::badge(if is_sv {
                        "SenseVoice"
                    } else {
                        "Whisper"
                    })),
            )
            // ── 2. 媒体文件选择与状态卡片 ──
            .child(
                primitives::card_sm()
                    .id("media-select-card")
                    // card_sm 默认纵向排布；这一张是「图标 + 文件名 + 按钮」的横排卡片
                    .flex_row()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(if has_file {
                        Theme::tint_mint_border()
                    } else {
                        Theme::border()
                    })
                    .cursor_pointer()
                    .hover(|s| {
                        s.bg(Theme::bg_card_hover())
                            .border_color(Theme::border_light())
                    })
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
                            .bg(if has_file {
                                Theme::tint_blue_soft()
                            } else {
                                Theme::tint_mint_soft()
                            })
                            .border_1()
                            .border_color(if has_file {
                                Theme::tint_blue_border()
                            } else {
                                Theme::tint_mint_border()
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file {
                                Theme::accent_blue()
                            } else {
                                Theme::accent_mint()
                            })
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
                                    .child(if has_file {
                                        fname
                                    } else {
                                        "选择音视频文件".to_string()
                                    }),
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
                            .bg(if has_file {
                                Theme::bg_card_hover()
                            } else {
                                Theme::accent_mint()
                            })
                            .border_1()
                            .border_color(if has_file {
                                Theme::border_strong()
                            } else {
                                Theme::tint_mint_border()
                            })
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(if has_file {
                                Theme::text_secondary()
                            } else {
                                Theme::text_on_accent()
                            })
                            .child(if has_file { "更换" } else { "浏览" }),
                    ),
            )
            // ── 3. 识别引擎档位栅格 ──
            .child(self.render_engine_card(cx))
            // ── 4. 转写核心参数（语言 / 格式 / 润色 / 线程 / GPU）──
            .child(self.render_params_card(cx))
            // ── 5. 底部主操作 CTA 按钮 (工程级突出呈现) ──
            .child(
                primitives::btn(
                    if is_processing {
                        if batch_running {
                            "终止批量"
                        } else {
                            "终止转写"
                        }
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
                    if can_act || is_processing {
                        s.opacity(0.9)
                    } else {
                        s
                    }
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
                })),
            )
    }

    /// 「字幕翻译」摘要卡：当前模式 / 端点 / 模型 + 一个跳转按钮。
    ///
    /// 为什么只做摘要不做控件：这一栏是 350px 的窄抽屉（[`Theme::DRAWER_W`]），
    /// 而在线翻译要填基址、密钥、模型并拉取模型列表，塞进来会把「转写参数」挤成
    /// 一屏半。真正的配置都在性能设置页的翻译卡上（含「拉取模型列表」），
    /// 这里只解决用户实测的那个坑：**配置项在界面上根本没有入口**，
    /// 之前只能手改 `config.toml`。摘要 + 一次点击即到配置处。
    #[allow(dead_code)]
    fn render_translate_summary_card(&mut self, cx: &mut Context<Self>) -> Div {
        let translate = &self.state.config.translate;
        let is_online = translate.is_online();
        let mode_label = if is_online {
            "在线 API"
        } else {
            "本地 Qwen"
        };
        // 端点只显示到主机级别：密钥与完整路径不该在这一栏多露一次
        let endpoint = if is_online {
            let url = translate.resolved_endpoint();
            match url
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
            {
                Some(host) if !host.is_empty() => host.to_string(),
                _ => url,
            }
        } else {
            "本地推理，无需网络".to_string()
        };
        let model = if is_online {
            let model = translate.api_model.trim();
            if model.is_empty() {
                "未填写模型名".to_string()
            } else {
                model.to_string()
            }
        } else {
            String::new()
        };

        let mut card = primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("字幕翻译"))
                    .child(primitives::badge(mode_label)),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child(endpoint),
            );
        if !model.is_empty() {
            card = card.child(
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .truncate()
                    .child(model),
            );
        }
        card.child(
            primitives::chip_clickable("在性能设置中配置", false, false)
                .id("sidebar-translate-config-btn")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state.active_tab = WorkspaceTab::Performance;
                    cx.notify();
                })),
        )
    }

    /// 「识别引擎」卡片：只显示已下载就位的模型，未下载不显示
    fn render_engine_card(&mut self, cx: &mut Context<Self>) -> Div {
        let tiers: [(WhisperModelTier, &'static str, &'static str); 4] = [
            (WhisperModelTier::Fast, "Base", "whisper-base"),
            (WhisperModelTier::Balanced, "Small-Q5", "whisper-small"),
            (WhisperModelTier::TurboSpeed, "Turbo Q5", "whisper-turbo-q5"),
            (WhisperModelTier::Precise, "Turbo Q8", "whisper-turbo-q8"),
        ];

        let mut available: Vec<(WhisperModelTier, &'static str, bool)> = Vec::new();
        // 检查 SenseVoice 模型是否已就位
        if self.state.model_is_present("sensevoice-model") {
            available.push((WhisperModelTier::SenseVoice, "SenseVoice 极速", true));
        }

        // Whisper 各模型档位：只保留已就位的
        for (tier, name, item_id) in tiers {
            if self.state.model_is_present(item_id) {
                available.push((tier, name, false));
            }
        }

        // 兜底：若全未下载，保留推荐的 SenseVoice
        if available.is_empty() {
            available.push((WhisperModelTier::SenseVoice, "SenseVoice 极速", true));
        } else {
            // 当前选中的档位若未下载，自动平滑切至首个已就位模型
            let cur = self.state.whisper_model_tier;
            if !available.iter().any(|(t, _, _)| *t == cur) {
                let fallback = available[0].0;
                self.state.set_whisper_model_tier(fallback);
            }
        }

        let mut grid = div().flex().flex_wrap().gap(px(Theme::SPACE_1_5));
        for (tier, name, full_width) in available {
            grid = grid.child(self.tier_pill(tier, name, full_width, cx));
        }

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(primitives::section_title("识别引擎"))
            .child(grid)
    }

    /// 模型档位选择胶囊
    fn tier_pill(
        &mut self,
        tier: WhisperModelTier,
        name: &'static str,
        full_width: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let is_sel = self.state.whisper_model_tier == tier;
        let pill = div()
            .id(name)
            .h(px(Theme::CTRL_H_MD))
            .px(px(Theme::SPACE_2))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::RADIUS_LG))
            .border_1()
            .border_color(if is_sel {
                Theme::tint_blue_border()
            } else {
                Theme::transparent()
            })
            .bg(if is_sel {
                Theme::tint_blue_badge()
            } else {
                Theme::bg_raised()
            })
            .cursor_pointer()
            .hover(move |s| {
                if is_sel {
                    s
                } else {
                    s.bg(Theme::tint_neutral())
                }
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.set_whisper_model_tier(tier);
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(if is_sel {
                        FontWeight::BOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .text_color(if is_sel {
                        Theme::accent_blue()
                    } else {
                        Theme::text_primary()
                    })
                    .child(name),
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
            "ja" => "ja",
            "en" => "en",
            "ko" => "ko",
            "yue" => "yue",
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
        // 线程候选项与性能页同源：上限取本机逻辑核心数，并并入配置当前值
        let thread_options: Vec<(String, String)> = thread_presets(
            self.state.whisper_threads,
            crate::app::AppState::logical_cores(),
        )
        .into_iter()
        .map(|n| (n.to_string(), format!("{n} 线程")))
        .collect();
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
                vec![
                    ("auto", "自动"),
                    ("zh", "中文"),
                    ("ja", "日语"),
                    ("en", "英语"),
                    ("ko", "韩语"),
                    ("yue", "粤语"),
                ],
                lang_sel,
                |this, sel, cx| {
                    this.state.language = sel.to_string();
                    // 与同卡片其它项一致：状态 + config.toml 双写。此前只改 state，
                    // 重启后 `AppState::with_hardware` 从配置读回的仍是旧语言。
                    this.state.config.pipeline.language = sel.to_string();
                    let _ = this.state.config.save_to_file("config.toml");
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
                    // 同上：不落盘的话，重启后配置里的旧格式会把这次选择覆盖掉
                    this.state.config.pipeline.output_format = sel.to_string();
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "标点与润色",
                vec![("off", "关闭"), ("punc", "极速标点"), ("qwen", "Qwen 润色")],
                polish_sel,
                |this, sel, cx| {
                    // 统一走 `AppState::set_polish`：状态 + config.toml 双写。
                    // 此前这里只改内存，重启后润色开关与档位都会被配置里的旧值覆盖。
                    // 「关闭」时沿用当前档位，只把总开关置假——否则用户关一下再开，
                    // 引擎会悄悄从 Qwen 变回 CT-Punc。
                    let mode = this.state.polish_mode;
                    match sel {
                        "punc" => this.state.set_polish(true, PolishMode::PuncFast),
                        "qwen" => this.state.set_polish(true, PolishMode::QwenDeep),
                        _ => this.state.set_polish(false, mode),
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
                            this.state.config.pipeline.speaker_count =
                                n.parse::<u32>().unwrap_or(2);
                        }
                    }
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                },
                cx,
            ))
            .child(self.param_group(
                "转写线程",
                thread_options,
                &thread_sel,
                |this, sel, cx| {
                    let v = sel.parse::<u32>().unwrap_or(8);
                    if this.state.whisper_threads != v {
                        this.state.whisper_threads = v;
                        // 与性能页滑条同一落盘口径（见 views/performance.rs）
                        this.state.config.pipeline.whisper_threads = v;
                        let _ = this.state.config.save_to_file("config.toml");
                        cx.notify();
                    }
                },
                cx,
            ))
    }

    /// 参数分组：小标题 + 分段选择器行
    fn param_group<K, L>(
        &mut self,
        label: &'static str,
        options: Vec<(K, L)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div
    where
        K: Into<SharedString> + Clone + 'static,
        L: Into<SharedString>,
    {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_1))
            .child(primitives::field_label(label))
            .child(self.pill_row(options, selected, on_select, cx))
    }

    /// 通用分段选择器：一行等宽胶囊，单击切换（多于4项时自适应换行，避免溢出重叠）
    fn pill_row<K, L>(
        &mut self,
        options: Vec<(K, L)>,
        selected: &str,
        on_select: impl Fn(&mut Self, &str, &mut Context<Self>) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> Div
    where
        K: Into<SharedString> + Clone + 'static,
        L: Into<SharedString>,
    {
        let is_wrap = options.len() > 4;
        let mut row = div().w_full().flex().gap(px(Theme::SPACE_1_5));
        if is_wrap {
            row = row.flex_wrap();
        }
        for (key, label) in options {
            let key: SharedString = key.into();
            let is_sel = key.as_str() == selected;
            let pill_id = key.clone();
            row = row.child(
                primitives::segmented(label, is_sel, !is_wrap)
                    .id(pill_id)
                    .on_click(cx.listener(move |this, _, _, cx| on_select(this, key.as_str(), cx))),
            );
        }
        row
    }

    /// 渲染硬件与模型资源监控对比卡片 (CPU / 内存实时对比)
    #[allow(dead_code)]
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
                    .child(primitives::field_label("系统监控"))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(primitives::stat_dot_sm(if m.is_model_running {
                                Theme::accent_mint()
                            } else {
                                Theme::text_muted()
                            }))
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
                                    .child(div().text_color(Theme::text_muted()).child(
                                        if m.is_model_running {
                                            "CPU (大模型):"
                                        } else {
                                            "CPU (应用待机):"
                                        },
                                    ))
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
                    .child(primitives::meter_bar(
                        sys_cpu_pct / 100.0,
                        proc_cpu_pct / 100.0,
                        Theme::accent_mint(),
                    )),
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
                                    .child(div().text_color(Theme::text_muted()).child(
                                        if m.is_model_running {
                                            "内存 (大模型):"
                                        } else {
                                            "内存 (应用待机):"
                                        },
                                    ))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_blue())
                                            .child(crate::app::ResourceMetrics::format_bytes(
                                                m.proc_mem,
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("{:.1} / {:.0} GB", sys_mem_gb, total_mem_gb)),
                            ),
                    )
                    // 双层内存条
                    .child(primitives::meter_bar(
                        sys_mem_pct / 100.0,
                        proc_mem_pct / 100.0,
                        Theme::accent_blue(),
                    )),
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
