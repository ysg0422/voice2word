//! 语音转写主工作台视图

use gpui::prelude::*;
use gpui::*;

use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::Segment;
use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染智能生成模式主体布局 (中间是工作区，右侧是配置和选项)
    pub(crate) fn render_generate_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            // 中间：主工作台与底部控制
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(520.0))
                    .h_full()
                    .overflow_hidden()
                    .child(self.render_main_workspace(cx))
                    .child(self.render_bottom_timeline(cx)),
            )
            // 右侧：配置和选项面板
            .child(self.render_sidebar(cx))
    }

    /// 渲染中央处理工作区：轻量化实时看板，彻底告别庞大表格卡顿
    pub(crate) fn render_main_workspace(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_tab = self.state.active_tab;
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let is_sensevoice = self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        div()
            .flex_1()
            .h_full()
            .bg(Theme::bg_panel())
            .p_5()
            .flex()
            .flex_col()
            .gap_3()
            // ── 顶部步骤流式导航栏 (借鉴 SmartSub / VideoCaptioner 黄金布局) ──
            .child(
                div()
                    .w_full()
                    .px_4()
                    .py_2()
                    .rounded_xl()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_4()
                            // Step 1: 导入
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(20.0))
                                            .h(px(20.0))
                                            .rounded_full()
                                            .bg(if !is_processing && self.state.transcribe_file.is_some() { Theme::accent_mint() } else { rgb(0x282834) })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(10.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if !is_processing && self.state.transcribe_file.is_some() { rgb(0x09090b) } else { rgb(0xffffff) })
                                            .child("1"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if !is_processing && self.state.transcribe_file.is_some() { Theme::text_primary() } else { Theme::text_secondary() })
                                            .child("导入媒体"),
                                    ),
                            )
                            .child(div().w(px(24.0)).h(px(1.0)).bg(rgb(0x2a2a36)))
                            // Step 2: 转写
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(20.0))
                                            .h(px(20.0))
                                            .rounded_full()
                                            .bg(if is_processing { Theme::accent_mint() } else { rgb(0x282834) })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(10.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if is_processing { rgb(0x09090b) } else { rgb(0xffffff) })
                                            .child("2"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(if is_processing { FontWeight::BOLD } else { FontWeight::MEDIUM })
                                            .text_color(if is_processing { Theme::accent_mint() } else { Theme::text_secondary() })
                                            .child("语音转写"),
                                    ),
                            )
                            .child(div().w(px(24.0)).h(px(1.0)).bg(rgb(0x2a2a36)))
                            // Step 3: 校对
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(20.0))
                                            .h(px(20.0))
                                            .rounded_full()
                                            .bg(if current_tab == WorkspaceTab::Editor && !self.state.segments.is_empty() { Theme::accent_blue() } else { rgb(0x282834) })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(10.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(0xffffff))
                                            .child("3"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("字幕校对"),
                                    ),
                            )
                            .child(div().w(px(24.0)).h(px(1.0)).bg(rgb(0x2a2a36)))
                            // Step 4: 导出
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(20.0))
                                            .h(px(20.0))
                                            .rounded_full()
                                            .bg(rgb(0x282834))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(10.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(0xffffff))
                                            .child("4"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("导出工程"),
                                    ),
                            ),
                    )
                    .child(
                        // 右侧状态药丸
                        if is_processing {
                            div()
                                .px_2p5()
                                .py_1()
                                .rounded_full()
                                .bg(rgba(0x10b98118))
                                .border_1()
                                .border_color(rgba(0x10b98133))
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(Theme::accent_mint()))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_mint())
                                        .child("正在转写..."),
                                )
                        } else if let Some(ref file) = self.state.transcribe_file {
                            div()
                                .px_2p5()
                                .py_1()
                                .rounded_full()
                                .bg(rgb(0x181820))
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(Theme::accent_blue()))
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(Theme::text_secondary())
                                        .child(file.file_name().and_then(|s| s.to_str()).unwrap_or("媒体文件").to_string()),
                                )
                        } else {
                            div()
                                .text_size(px(11.0))
                                .text_color(Theme::text_muted())
                                .child("就绪")
                        }
                    ),
            )
            // 核心工作台展示区
            .child(
                if is_processing {
                    let (stage, progress, detail) = match &self.state.status {
                        ProcessStatus::Processing { stage, progress, detail } => {
                            (stage.clone(), *progress, detail.clone())
                        }
                        _ => ("正在全速转写中...".to_string(), 0.0, "".to_string()),
                    };

                    let total_dur = self.state.transcribe_duration;
                    let cur_sec = self.state.streaming_current_sec;
                    let cur_mm = (cur_sec / 60.0) as u32;
                    let cur_ss = (cur_sec % 60.0) as u32;
                    let tot_mm = (total_dur / 60.0) as u32;
                    let tot_ss = (total_dur % 60.0) as u32;

                    let display_ratio = if total_dur > 0.0 && cur_sec > 0.0 {
                        (cur_sec / total_dur).clamp(0.0, 1.0)
                    } else {
                        progress.clamp(0.0, 1.0)
                    };

                    let eta = self.state.whisper_eta_label();
                    let seg_count = self.state.streaming_segments.len();

                    div()
                        .id("lightweight-processing-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3p5()
                        // 1. 媒体与当前阶段卡片
                        .child(
                            div()
                                .w_full()
                                .max_w(px(860.0))
                                .p_3()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .child(
                                            div()
                                                .w(px(32.0))
                                                .h(px(32.0))
                                                .rounded_lg()
                                                .bg(if is_sensevoice { rgba(0x10b9811c) } else { rgba(0x38bdf81c) })
                                                .border_1()
                                                .border_color(if is_sensevoice { rgba(0x10b98133) } else { rgba(0x38bdf833) })
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(div().w(px(9.0)).h(px(9.0)).rounded_full().bg(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() })),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .child(
                                                    div()
                                                        .text_size(px(14.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::text_primary())
                                                        .child(stage),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_muted())
                                                        .child(if detail.trim().is_empty() {
                                                            if is_sensevoice { "SenseVoice 正在通过纯内存音频管道极速识别...".to_string() } else { "Whisper 模型正在转写音频流...".to_string() }
                                                        } else {
                                                            detail
                                                        }),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .px_2p5()
                                                .py_1()
                                                .rounded_md()
                                                .bg(if is_sensevoice { rgba(0x10b98118) } else { rgba(0x38bdf818) })
                                                .border_1()
                                                .border_color(if is_sensevoice { rgba(0x10b98133) } else { rgba(0x38bdf833) })
                                                .text_size(px(10.5))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() })
                                                .child(if is_sensevoice { "SenseVoice INT8 (42x 极速)" } else { "Whisper Turbo (GPU加速)" }),
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .bg(rgb(0x181820))
                                                .border_1()
                                                .border_color(Theme::border())
                                                .text_size(px(10.5))
                                                .text_color(Theme::text_secondary())
                                                .child(format!("{tot_mm:02}:{tot_ss:02}")),
                                        ),
                                ),
                        )
                        // 2. 实时流式字幕视窗卡片
                        .child(
                            div()
                                .w_full()
                                .max_w(px(860.0))
                                .rounded_2xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .p_4()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .gap_3()
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
                                                .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(Theme::accent_mint()))
                                                .child(
                                                    div()
                                                        .text_size(px(13.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::text_primary())
                                                        .child("实时流式字幕视窗"),
                                                )
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgba(0x10b9811c))
                                                        .text_size(px(10.5))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child(format!("已流式生成 {seg_count} 句")),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.5))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_secondary())
                                                .child(if total_dur > 0.0 {
                                                    format!("进度: {cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02} ({:.1}%)", display_ratio * 100.0)
                                                } else {
                                                    "正在实时推流...".to_string()
                                                }),
                                        ),
                                )
                                // 平滑大进度条
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(6.0))
                                        .rounded_full()
                                        .bg(rgb(0x181820))
                                        .border_1()
                                        .border_color(Theme::border())
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .rounded_full()
                                                .w(relative(display_ratio as f32))
                                                .bg(Theme::accent_mint()),
                                        ),
                                )
                                // 字幕列表动态滚动流
                                .child(
                                    if seg_count == 0 {
                                        div()
                                            .min_h(px(110.0))
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(12.5))
                                            .text_color(Theme::text_muted())
                                            .child(if is_sensevoice {
                                                "SenseVoice 极速模型正在解析音频流，首句字幕即将秒级呈现..."
                                            } else {
                                                "AI 语音引擎正在加载权重并解析音频流，首句字幕即将实时呈现..."
                                            })
                                            .into_any_element()
                                    } else {
                                        let seg_list: Vec<&Segment> = self.state.streaming_segments.iter().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
                                        let total_items = seg_list.len();
                                        div()
                                            .min_h(px(110.0))
                                            .w_full()
                                            .flex()
                                            .flex_col()
                                            .justify_end()
                                            .gap_2()
                                            .children(seg_list.into_iter().enumerate().map(|(i, seg)| {
                                                let is_last = i == total_items - 1;
                                                let s_m = (seg.start / 60.0) as u32;
                                                let s_s = (seg.start % 60.0) as u32;
                                                let e_m = (seg.end / 60.0) as u32;
                                                let e_s = (seg.end % 60.0) as u32;
                                                let time_badge = format!("{s_m:02}:{s_s:02} - {e_m:02}:{e_s:02}");

                                                if is_last {
                                                    div()
                                                        .w_full()
                                                        .px_3p5()
                                                        .py_2()
                                                        .rounded_lg()
                                                        .bg(rgba(0x10b98114))
                                                        .border_1()
                                                        .border_color(rgba(0x10b98138))
                                                        .overflow_hidden()
                                                        .flex()
                                                        .items_start()
                                                        .gap_3()
                                                        .child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .mt(px(1.5))
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .bg(rgba(0x10b98125))
                                                                .text_size(px(10.5))
                                                                .font_weight(FontWeight::BOLD)
                                                                .text_color(Theme::accent_mint())
                                                                .child(time_badge),
                                                        )
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .line_height(relative(1.4))
                                                                .text_size(px(13.0))
                                                                .font_weight(FontWeight::BOLD)
                                                                .text_color(Theme::text_primary())
                                                                .child(seg.display_text().to_string()),
                                                        )
                                                } else {
                                                    div()
                                                        .w_full()
                                                        .px_3p5()
                                                        .py_1p5()
                                                        .overflow_hidden()
                                                        .flex()
                                                        .items_start()
                                                        .gap_3()
                                                        .child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .mt(px(1.0))
                                                                .text_size(px(10.5))
                                                                .text_color(Theme::text_muted())
                                                                .child(time_badge),
                                                        )
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .overflow_hidden()
                                                                .line_height(relative(1.4))
                                                                .text_size(px(12.0))
                                                                .text_color(Theme::text_secondary())
                                                                .child(seg.display_text().to_string()),
                                                        )
                                                }
                                            }))
                                            .into_any_element()
                                    }
                                ),
                        )
                        // 3. 底部 4 宫格性能与状态仪表盘
                        .child(
                            div()
                                .w_full()
                                .max_w(px(860.0))
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("总体进度"))
                                        .child(div().text_size(px(17.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{:.1}%", display_ratio * 100.0)))
                                        .child(div().text_size(px(9.5)).text_color(Theme::text_secondary()).child(format!("{cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02}"))),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("预计耗时"))
                                        .child(div().text_size(px(17.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(eta))
                                        .child(div().text_size(px(9.5)).text_color(Theme::text_secondary()).child(if is_sensevoice { "极速 42.1x 实时倍速" } else { "Whisper 并行解码" })),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("音频推流"))
                                        .child(div().text_size(px(17.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child("纯内存管道"))
                                        .child(div().text_size(px(9.5)).text_color(Theme::text_secondary()).child("0 磁盘 I/O · 0 磨损")),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .p_3()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .child(div().text_size(px(10.5)).text_color(Theme::text_muted()).child("推理引擎"))
                                        .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).text_color(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() }).child(if is_sensevoice { "SenseVoice INT8" } else { "Whisper Turbo" }))
                                        .child(div().text_size(px(9.5)).text_color(Theme::text_secondary()).child(if is_sensevoice { "非自回归 · 自带标点" } else { "多处理器并行" })),
                                ),
                        )
                        .into_any_element()
                } else if let Some(ref file) = self.state.transcribe_file {
                    // 已就绪待处理卡片 (用户刚选中了新视频，准备转写)
                    let fname = file.file_name().and_then(|s| s.to_str()).unwrap_or("视频").to_string();
                    let dur_str = format_duration_short(self.state.transcribe_duration);
                    let eta_str = self.state.whisper_eta_label();
                    let has_dur = self.state.transcribe_duration > 0.0;
                    let cached_opt = self.state.get_cached_transcription();

                    let tier_name = if is_sensevoice {
                        "SenseVoice 极速"
                    } else {
                        match self.state.whisper_model_tier {
                            crate::app::WhisperModelTier::Fast => "Whisper Base",
                            crate::app::WhisperModelTier::Balanced => "Whisper Small",
                            crate::app::WhisperModelTier::TurboSpeed => "Whisper Turbo Q5",
                            crate::app::WhisperModelTier::Precise => "Whisper Turbo Q8",
                            _ => "Whisper",
                        }
                    };

                    div()
                        .id("transcribe-file-ready-dashboard")
                        .flex_1()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .p_6()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(640.0))
                                .p_6()
                                .rounded_2xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_4()
                                // 顶部状态条
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .px_2p5()
                                                .py_0p5()
                                                .rounded_full()
                                                .bg(if is_sensevoice { rgba(0x10b98118) } else { rgba(0x38bdf818) })
                                                .border_1()
                                                .border_color(if is_sensevoice { rgba(0x10b98133) } else { rgba(0x38bdf833) })
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() })
                                                .child(tier_name),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2p5()
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("时长: {}", dur_str)),
                                                )
                                                .children(if has_dur {
                                                    Some(
                                                        div()
                                                            .px_2()
                                                            .py_0p5()
                                                            .rounded_full()
                                                            .bg(if is_sensevoice { rgba(0x10b98118) } else { rgba(0x6366f118) })
                                                            .border_1()
                                                            .border_color(if is_sensevoice { rgba(0x10b98133) } else { rgba(0x6366f133) })
                                                            .text_size(px(10.5))
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_primary() })
                                                            .child(format!("预估 {}", eta_str)),
                                                    )
                                                } else {
                                                    None
                                                }),
                                        ),
                                )
                                // 文件主标题
                                .child(
                                    div()
                                        .text_size(px(18.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::text_primary())
                                        .child(fname),
                                )
                                // 缓存提示卡片
                                .children(if let Some(ref cached) = cached_opt {
                                    Some(
                                        div()
                                            .w_full()
                                            .px_3p5()
                                            .py_1p5()
                                            .rounded_xl()
                                            .bg(rgba(0x10b98114))
                                            .border_1()
                                            .border_color(rgba(0x10b98130))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .gap_2()
                                            .child(div().w(px(6.0)).h(px(6.0)).rounded_full().bg(Theme::accent_mint()))
                                            .child(
                                                div()
                                                    .text_size(px(12.0))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(Theme::accent_mint())
                                                    .child(format!("命中本地转写缓存 (含 {} 句字幕)", cached.segments.len())),
                                            ),
                                    )
                                } else {
                                    None
                                })
                                // 核心操作按钮组
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .mt_2()
                                        .children(if let Some(cached) = cached_opt {
                                            vec![
                                                div()
                                                    .id("cache-hit-load-btn")
                                                    .px_6()
                                                    .py_2p5()
                                                    .rounded_xl()
                                                    .bg(Theme::accent_mint())
                                                    .cursor_pointer()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(rgb(0x09090b))
                                                    .hover(|s| s.opacity(0.9))
                                                    .on_click({
                                                        let cached_clone = cached.clone();
                                                        cx.listener(move |this, _, _, cx| {
                                                            this.load_cached_result(cx, cached_clone.clone());
                                                        })
                                                    })
                                                    .child("载入缓存")
                                                    .into_any_element(),
                                                div()
                                                    .id("ready-start-process-btn")
                                                    .px_5()
                                                    .py_2p5()
                                                    .rounded_xl()
                                                    .bg(Theme::bg_card())
                                                    .border_1()
                                                    .border_color(Theme::border())
                                                    .cursor_pointer()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(Theme::text_primary())
                                                    .hover(|s| s.bg(Theme::bg_hover()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_processing(cx);
                                                    }))
                                                    .child("重新完整转写")
                                                    .into_any_element(),
                                                div()
                                                    .id("ready-repick-file-btn")
                                                    .px_4()
                                                    .py_2p5()
                                                    .rounded_xl()
                                                    .bg(Theme::bg_card())
                                                    .border_1()
                                                    .border_color(Theme::border())
                                                    .cursor_pointer()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(Theme::text_secondary())
                                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.choose_file(cx);
                                                    }))
                                                    .child("更换视频")
                                                    .into_any_element(),
                                            ]
                                        } else {
                                            vec![
                                                div()
                                                    .id("ready-start-process-btn")
                                                    .px_8()
                                                    .py_3()
                                                    .rounded_xl()
                                                    .bg(Theme::accent_mint())
                                                    .cursor_pointer()
                                                    .text_size(px(14.0))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(rgb(0x09090b))
                                                    .hover(|s| s.opacity(0.9))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_processing(cx);
                                                    }))
                                                    .child(if is_sensevoice { "开始极速转写" } else { "开始神经转写" })
                                                    .into_any_element(),
                                                div()
                                                    .id("ready-repick-file-btn")
                                                    .px_5()
                                                    .py_3()
                                                    .rounded_xl()
                                                    .bg(Theme::bg_card())
                                                    .border_1()
                                                    .border_color(Theme::border())
                                                    .cursor_pointer()
                                                    .text_size(px(13.0))
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(Theme::text_secondary())
                                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.choose_file(cx);
                                                    }))
                                                    .child("重新选择")
                                                    .into_any_element(),
                                            ]
                                        }),
                                ),
                        )
                        .into_any_element()
                } else {
                    // 空闲就绪引导工作区 (Studio Dropzone)
                    div()
                        .id("lightweight-idle-dashboard")
                        .flex_1()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .p_6()
                        .child(
                            div()
                                .w_full()
                                .max_w(px(640.0))
                                .p_10()
                                .rounded_2xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_4()
                                .child(
                                    div()
                                        .w(px(56.0))
                                        .h(px(56.0))
                                        .rounded_2xl()
                                        .bg(rgb(0x1a1a24))
                                        .border_1()
                                        .border_color(rgb(0x2c2c3e))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_size(px(22.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_blue())
                                        .child("+"),
                                )
                                .child(
                                    div()
                                        .text_size(px(18.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::text_primary())
                                        .child("导入音视频文件"),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        .mt_2()
                                        .child(
                                            div()
                                                .id("idle-pick-file-btn")
                                                .px_6()
                                                .py_2p5()
                                                .rounded_xl()
                                                .bg(Theme::accent_mint())
                                                .cursor_pointer()
                                                .text_size(px(13.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(rgb(0x09090b))
                                                .hover(|s| s.opacity(0.9))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.choose_file(cx);
                                                }))
                                                .child("浏览本地文件"),
                                        )
                                        .child(
                                            div()
                                                .id("idle-goto-library-btn")
                                                .px_5()
                                                .py_2p5()
                                                .rounded_xl()
                                                .bg(Theme::bg_card())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(13.0))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.active_tab = WorkspaceTab::Library;
                                                    this.state.refresh_recent_tasks();
                                                    cx.notify();
                                                }))
                                                .child("从历史视频库选择"),
                                        ),
                                ),
                        )
                        .into_any_element()
                },
            )
    }
}
