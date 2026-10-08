//! 语音转写主工作台视图

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, QueueState, WorkspaceTab};
// 质检报告是 `segment.rs` 里的纯逻辑：这里只把结果画出来，不在界面层重算判据。
use crate::subtitle::segment::{quality_report, QualityReport};
use crate::subtitle::Segment;
use crate::utils::time::format_duration_short;

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
                    .min_w(px(Theme::MIN_CENTER_COL_W))
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
        let is_sensevoice =
            self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;

        // 页面外壳统一走 primitives::page_shell：底色 / 内边距 / 分区间距与其余三个
        // 工作台页同源，切页时中央列不再横向跳动；`.flex_1()` 保留它在左右分栏里
        // 撑满剩余宽度的职责。
        primitives::page_shell("transcribe-main-workspace")
            .flex_1()
            // ── 顶部步骤流式导航栏 (借鉴 SmartSub / VideoCaptioner 黄金布局) ──
            .child(
                div()
                    .w_full()
                    .px(px(Theme::CARD_PAD))
                    .py(px(Theme::SPACE_2))
                    .rounded(px(Theme::CARD_RADIUS))
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
                                            .w(px(Theme::STEP_DOT))
                                            .h(px(Theme::STEP_DOT))
                                            .rounded_full()
                                            .bg(if !is_processing && self.state.transcribe_file.is_some() { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if !is_processing && self.state.transcribe_file.is_some() { Theme::text_on_accent() } else { Theme::text_primary() })
                                            .child("1"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(if !is_processing && self.state.transcribe_file.is_some() { Theme::text_primary() } else { Theme::text_secondary() })
                                            .child("导入媒体"),
                                    ),
                            )
                            .child(primitives::step_connector())
                            // Step 2: 转写
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(Theme::STEP_DOT))
                                            .h(px(Theme::STEP_DOT))
                                            .rounded_full()
                                            .bg(if is_processing { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(if is_processing { Theme::text_on_accent() } else { Theme::text_primary() })
                                            .child("2"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(if is_processing { FontWeight::BOLD } else { FontWeight::MEDIUM })
                                            .text_color(if is_processing { Theme::accent_mint() } else { Theme::text_secondary() })
                                            .child("语音转写"),
                                    ),
                            )
                            .child(primitives::step_connector())
                            // Step 3: 校对
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(Theme::STEP_DOT))
                                            .h(px(Theme::STEP_DOT))
                                            .rounded_full()
                                            .bg(if current_tab == WorkspaceTab::Editor && !self.state.segments.is_empty() { Theme::accent_blue() } else { Theme::bg_hover_strong() })
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::BOLD)
                                            // 未选中态是**翻转**的中性底槽（浅色主题下约 0xe2e2e9），数字必须跟着
                                            // 底色一起翻转，取 text_primary（与步骤 1/2/4 一致，依据见
                                            // docs/UI设计规范.md §3.2「中性底槽 → text_primary（翻转）」）。
                                            // 这里**不能**用 text_on_saturated：它是恒白 token，在深色主题的深底槽上
                                            // 看着正常，浅色主题下却是白字压浅底，几乎不可见——正是「底翻转、字恒白」
                                            // 这个最常见错误。只有选中态（蓝色饱和块，恒深）才配 text_on_saturated。
                                            .text_color(if current_tab == WorkspaceTab::Editor && !self.state.segments.is_empty() { Theme::text_on_saturated() } else { Theme::text_primary() })
                                            .child("3"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("字幕校对"),
                                    ),
                            )
                            .child(primitives::step_connector())
                            // Step 4: 导出
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(Theme::STEP_DOT))
                                            .h(px(Theme::STEP_DOT))
                                            .rounded_full()
                                            .bg(Theme::bg_hover_strong())
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::BOLD)
                                            // 未激活的步骤圆点是**翻转**的中性底槽，数字须用翻转文字；
                                            // 只有激活态（薄荷/蓝实心）才用恒深 text_on_accent
                                            .text_color(Theme::text_primary())
                                            .child("4"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(Theme::text_secondary())
                                            .child("导出工程"),
                                    ),
                            ),
                    )
                    .child(
                        // 右侧状态药丸：转写中 / 文件就绪两态都收敛到 badge 原语，
                        // 行内状态点沿用 stat_dot_sm，不再各写一份胶囊尺寸。
                        if is_processing {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_1))
                                .child(primitives::stat_dot_sm(Theme::accent_mint()))
                                .child(primitives::badge_accent("正在转写..."))
                        } else if let Some(ref file) = self.state.transcribe_file {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_1))
                                .child(primitives::stat_dot_sm(Theme::accent_blue()))
                                .child(primitives::badge(
                                    file.file_name().and_then(|s| s.to_str()).unwrap_or("媒体文件").to_string(),
                                ))
                        } else {
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child("就绪")
                        }
                    ),
            )
            // ── 批量转写队列 (F-012)：队列非空时固定在步骤流下方，随时可见 ──
            .children(if self.state.batch_queue.is_empty() {
                None
            } else {
                Some(self.render_batch_queue_panel(cx))
            })
            // 核心工作台展示区
            .child(
                if is_processing {
                    // GPUI 的 `Div::child` 要求文本是 `'static`（内部转 `SharedString`），
                    // 无法借用 `self.state.status` 里的 `&str`，因此这里必须 clone 出所有权。
                    let (stage, progress, detail) = match &self.state.status {
                        ProcessStatus::Processing { stage, progress, detail } => {
                            (stage.clone(), *progress, detail.clone())
                        }
                        _ => ("正在全速转写中...".to_string(), 0.0, String::new()),
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
                    let seg_count = self.state.streaming_segment_count;

                    div()
                        .id("lightweight-processing-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(Theme::SPACE_4))
                        // 1. 媒体与当前阶段卡片
                        .child(
                            // 卡片外壳统一走 card()：内边距 / 圆角 / 底色与其余卡片同源
                            primitives::card()
                                .w_full()
                                .max_w(px(Theme::CONTENT_MAX_W))
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(Theme::SPACE_3))
                                        .child(
                                            div()
                                                .w(px(Theme::ICON_BOX))
                                                .h(px(Theme::ICON_BOX))
                                                .rounded(px(Theme::RADIUS_LG))
                                                .bg(if is_sensevoice { Theme::tint_mint_soft() } else { Theme::tint_blue_soft() })
                                                .border_1()
                                                .border_color(if is_sensevoice { Theme::tint_mint_border() } else { Theme::tint_blue_border() })
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .child(primitives::stat_dot(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() })),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                // 阶段名是看板的主标题，走 panel_title（14px）与其余面板标头一致
                                                .child(primitives::panel_title(stage))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
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
                                        .gap(px(Theme::SPACE_1_5))
                                        // 引擎标签与总时长都是元信息，统一走徽标原语
                                        .child(if is_sensevoice {
                                            primitives::badge_accent("SenseVoice INT8 (14x 极速)")
                                        } else {
                                            primitives::badge("Whisper Turbo (GPU加速)")
                                        })
                                        .child(primitives::badge(format!("{tot_mm:02}:{tot_ss:02}"))),
                                ),
                        )
                        // 2. 实时流式字幕视窗卡片
                        .child(
                            // 卡片外壳走 card()：圆角 / 内边距 / 底色与其余卡片同源
                            primitives::card()
                                .w_full()
                                .max_w(px(Theme::CONTENT_MAX_W))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(Theme::SPACE_2))
                                                // 卡片标头统一走 section_title，与「批量转写队列」等同级
                                                .child(primitives::stat_dot_sm(Theme::accent_mint()))
                                                .child(primitives::section_title("实时流式字幕视窗"))
                                                .child(primitives::badge_accent(format!("已流式生成 {seg_count} 句"))),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(Theme::TEXT_BODY))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_secondary())
                                                .child(if total_dur > 0.0 {
                                                    format!("进度: {cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02} ({:.1}%)", display_ratio * 100.0)
                                                } else {
                                                    "正在实时推流...".to_string()
                                                }),
                                        ),
                                )
                                // 平滑大进度条：统一走进度条原语（轨道色 / 高亮色 / 高度同源）
                                .child(primitives::progress_bar(display_ratio as f32))
                                // 字幕列表动态滚动流
                                .child(
                                    if seg_count == 0 {
                                        div()
                                            .min_h(px(Theme::STREAM_MIN_H))
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(Theme::TEXT_BODY_LG))
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
                                            .min_h(px(Theme::STREAM_MIN_H))
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
                                                        .px(px(Theme::SPACE_3))
                                                        .py(px(Theme::SPACE_2))
                                                        .rounded(px(Theme::RADIUS_LG))
                                                        .bg(Theme::tint_mint_soft())
                                                        .border_1()
                                                        .border_color(Theme::tint_mint_border())
                                                        .overflow_hidden()
                                                        .flex()
                                                        .items_start()
                                                        .gap(px(Theme::SPACE_3))
                                                        .child(
                                                            // 时间戳胶囊：字色与底色都来自薄荷 tint 阶梯
                                                            div()
                                                                .flex_shrink_0()
                                                                .mt(px(1.5))
                                                                .px(px(Theme::SPACE_1_5))
                                                                .py(px(Theme::SPACE_1))
                                                                .rounded(px(Theme::RADIUS_MD))
                                                                .bg(Theme::tint_mint_badge())
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .font_weight(FontWeight::BOLD)
                                                                .text_color(Theme::accent_mint())
                                                                .child(time_badge),
                                                        )
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .line_height(relative(1.4))
                                                                .text_size(px(Theme::TEXT_BODY_LG))
                                                                .font_weight(FontWeight::BOLD)
                                                                .text_color(Theme::text_primary())
                                                                .child(seg.display_text().to_string()),
                                                        )
                                                } else {
                                                    div()
                                                        .w_full()
                                                        .px(px(Theme::SPACE_3))
                                                        .py(px(Theme::SPACE_1_5))
                                                        .overflow_hidden()
                                                        .flex()
                                                        .items_start()
                                                        .gap(px(Theme::SPACE_3))
                                                        .child(
                                                            div()
                                                                .flex_shrink_0()
                                                                .mt(px(1.0))
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_color(Theme::text_muted())
                                                                .child(time_badge),
                                                        )
                                                        .child(
                                                            div()
                                                                .flex_1()
                                                                .min_w(px(0.0))
                                                                .overflow_hidden()
                                                                .line_height(relative(1.4))
                                                                .text_size(px(Theme::TEXT_BODY))
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
                                .max_w(px(Theme::CONTENT_MAX_W))
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    // 统计磁贴外壳统一走 card_sm()：内边距 / 圆角 / 描边同源，
                                    // 数字用 TEXT_STAT、标签用 TEXT_SMALL 保持四格对齐。
                                    primitives::card_sm()
                                        .flex_1()
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("总体进度"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{:.1}%", display_ratio * 100.0)))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child(format!("{cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02}"))),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("预计耗时"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(eta))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child(if is_sensevoice { "极速 14.5x 实时倍速" } else { "Whisper 并行解码" })),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("音频推流"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child("纯内存管道"))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child("0 磁盘 I/O · 0 磨损")),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("推理引擎"))
                                        // 引擎名比数字短，降到 TEXT_TITLE 免得撑破磁贴宽度
                                        .child(div().text_size(px(Theme::TEXT_TITLE)).font_weight(FontWeight::BOLD).text_color(if is_sensevoice { Theme::accent_mint() } else { Theme::accent_blue() }).child(if is_sensevoice { "SenseVoice INT8" } else { "Whisper Turbo" }))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child(if is_sensevoice { "非自回归 · 自带标点" } else { "多处理器并行" })),
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
                            crate::app::WhisperModelTier::Balanced => "Whisper Small-Q5",
                            crate::app::WhisperModelTier::TurboSpeed => "Whisper Turbo Q5",
                            crate::app::WhisperModelTier::Precise => "Whisper Turbo Q8",
                            _ => "Whisper",
                        }
                    };

                    // 转写质检摘要 (P1-9)：只在「工作区当前载入的字幕就是这张卡片对应的媒体」时展示。
                    // 用户刚挑了一个还没转写的新文件时 `transcribe_file` 已换成新路径，而
                    // `segments` 里还是上一支片子的结果——拿它算质检等于把上一支的问题挂到新文件头上。
                    let quality_card = if self.state.selected_file.as_ref() == self.state.transcribe_file.as_ref() {
                        self.render_quality_summary(cx)
                    } else {
                        None
                    };

                    div()
                        .id("transcribe-file-ready-dashboard")
                        .flex_1()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .p(px(Theme::PAGE_PAD))
                        .child(
                            // 就绪卡片外壳走 card()，与其余卡片同圆角 / 同内边距
                            primitives::card()
                                .w_full()
                                .max_w(px(Theme::CONTENT_MAX_W))
                                .items_center()
                                .gap(px(Theme::SPACE_4))
                                // 顶部状态条
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            // 引擎档位是元信息，走 badge 原语（与进度看板同款）
                                            if is_sensevoice {
                                                primitives::badge_accent(tier_name)
                                            } else {
                                                primitives::badge(tier_name)
                                            },
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(Theme::SPACE_2))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("时长: {}", dur_str)),
                                                )
                                                .children(if has_dur {
                                                    // 预估耗时药丸：薄荷档 = 极速引擎，其余走主重音 tint，
                                                    // 两档配色都取自 Theme 的 tint_* 阶梯；形状走
                                                    // primitives::tag_tinted，字号 / 字重按原样保留。
                                                    Some(
                                                        primitives::tag_tinted(
                                                            format!("预估 {}", eta_str),
                                                            if is_sensevoice { Theme::tint_mint_soft() } else { Theme::tint_primary_soft() },
                                                            if is_sensevoice { Theme::tint_mint_border() } else { Theme::tint_primary_border() },
                                                            if is_sensevoice { Theme::accent_mint() } else { Theme::accent_primary() },
                                                        )
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_size(px(Theme::TEXT_SMALL)),
                                                    )
                                                } else {
                                                    None
                                                }),
                                        ),
                                )
                                // 文件主标题
                                .child(primitives::page_title(fname))
                                // 缓存提示卡片
                                // （`cached_opt` 已是 `Option<Arc<TaskRecord>>`，直接 `map`：
                                //   写成 `if let Some(..) { Some(..) } else { None }` 会被
                                //   clippy 判成 `manual_map`。）
                                .children(cached_opt.as_ref().map(|cached| {
                                    div()
                                        .w_full()
                                        .px(px(Theme::SPACE_3))
                                        .py(px(Theme::SPACE_1_5))
                                        .rounded(px(Theme::CARD_RADIUS))
                                        .bg(Theme::tint_mint_soft())
                                        .border_1()
                                        .border_color(Theme::tint_mint_border())
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .gap(px(Theme::SPACE_2))
                                        .child(primitives::stat_dot_sm(Theme::accent_mint()))
                                        .child(
                                            div()
                                                .text_size(px(Theme::TEXT_BODY))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::accent_mint())
                                                .child(format!("命中本地转写缓存 (含 {} 句字幕)", cached.segments.len())),
                                        )
                                }))
                                // 转写质检摘要（已在卡片上方算好；无字幕时为 None）
                                .children(quality_card)
                                // 核心操作按钮组：主行动 Lg/Primary，其余 Lg/Secondary，
                                // 按钮骨架（高度 / 内边距 / 文字色）交给 btn 原语统一
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(Theme::SPACE_3))
                                        .mt(px(Theme::SPACE_2))
                                        .children(if let Some(cached) = cached_opt {
                                            vec![
                                                primitives::btn("载入缓存", primitives::BtnSize::Lg, primitives::BtnVariant::Primary)
                                                    .id("cache-hit-load-btn")
                                                    .on_click({
                                                        let cached_clone = cached.clone();
                                                        cx.listener(move |this, _, _, cx| {
                                                            this.load_cached_result(cx, cached_clone.clone());
                                                        })
                                                    })
                                                    .into_any_element(),
                                                primitives::btn("重新完整转写", primitives::BtnSize::Lg, primitives::BtnVariant::Secondary)
                                                    .id("ready-start-process-btn")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_processing(cx);
                                                    }))
                                                    .into_any_element(),
                                                primitives::btn("更换视频", primitives::BtnSize::Lg, primitives::BtnVariant::Secondary)
                                                    .id("ready-repick-file-btn")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.choose_file(cx);
                                                    }))
                                                    .into_any_element(),
                                            ]
                                        } else {
                                            vec![
                                                primitives::btn(if is_sensevoice { "开始极速转写" } else { "开始神经转写" }, primitives::BtnSize::Lg, primitives::BtnVariant::Primary)
                                                    .id("ready-start-process-btn")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_processing(cx);
                                                    }))
                                                    .into_any_element(),
                                                primitives::btn("重新选择", primitives::BtnSize::Lg, primitives::BtnVariant::Secondary)
                                                    .id("ready-repick-file-btn")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.choose_file(cx);
                                                    }))
                                                    .into_any_element(),
                                            ]
                                        }),
                                ),
                        )
                        .into_any_element()
                } else {
                    // 空闲就绪引导工作区 (Studio Dropzone)
                    //
                    // 「转写完成后的看板」实际就落在本分支：`actions.rs` 在 `Finished` 里
                    // 会把 `transcribe_file` 清空并回到这里（随后弹出的完成弹窗盖在上面）。
                    // 因此质检摘要挂在这里，用户一关弹窗就能看到「哪几句要复核」，
                    // 而不是只剩一张「导入音视频文件」的空态卡。
                    // 没有已载入字幕时 `render_quality_summary` 返回 None，布局与从前一致。
                    let quality_card = self.render_quality_summary(cx);
                    div()
                        .id("lightweight-idle-dashboard")
                        .flex_1()
                        .w_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(Theme::SPACE_4))
                        .p(px(Theme::PAGE_PAD))
                        .children(quality_card)
                        .child(
                            // 空态卡片外壳走 card()，内边距升一档让引导区更舒展
                            primitives::card_with_pad(Theme::SPACE_6)
                                .w_full()
                                .max_w(px(Theme::CONTENT_MAX_W))
                                .items_center()
                                .gap(px(Theme::SPACE_4))
                                .child(
                                    div()
                                        .w(px(Theme::ICON_BOX_LG))
                                        .h(px(Theme::ICON_BOX_LG))
                                        .rounded(px(Theme::CARD_RADIUS))
                                        .bg(Theme::bg_raised())
                                        .border_1()
                                        .border_color(Theme::border_mid())
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        // 大号「+」比页面标题再大一档，作为空态主视觉
                                        .text_size(px(Theme::TEXT_DISPLAY))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_blue())
                                        .child("+"),
                                )
                                .child(primitives::page_title("导入音视频文件"))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(Theme::SPACE_3))
                                        .mt(px(Theme::SPACE_2))
                                        .child(
                                            primitives::btn_clickable("浏览本地文件", primitives::BtnSize::Lg, primitives::BtnVariant::Primary)
                                                .id("idle-pick-file-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.choose_file(cx);
                                                })),
                                        )
                                        .child(
                                            primitives::btn_clickable("从历史视频库选择", primitives::BtnSize::Lg, primitives::BtnVariant::Secondary)
                                                .id("idle-goto-library-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.active_tab = WorkspaceTab::Library;
                                                    this.state.refresh_recent_tasks();
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            primitives::btn_clickable("批量导入多个文件", primitives::BtnSize::Lg, primitives::BtnVariant::Secondary)
                                                .id("idle-batch-import-btn")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.choose_batch_files(cx);
                                                })),
                                        ),
                                ),
                        )
                        .into_any_element()
                },
            )
    }

    // ── 转写质检摘要 (P1-9)：把 `segment.rs` 的质检报告变成用户可见的复核清单 ──

    /// 当前工程的质检报告；没有字幕时返回 `None`（此时不该渲染质检块）。
    ///
    /// 术语违规一栏取 `cached_glossary_violations` 而不是就地重算：那份缓存按
    /// 「术语表文本 + `segments_revision`」失效，剪辑台的字幕清单用的也是它，
    /// 于是同一份数据在两个页面给出同一个数字（不会「这页 3 句、那页 5 句」），
    /// 渲染开销也只在术语表或字幕真的变了时付一次。
    fn quality_report_snapshot(&mut self) -> Option<QualityReport> {
        if self.state.segments.is_empty() {
            return None;
        }
        // 「未翻译」只在用户已经翻译过时才算缺陷：整篇没译文是「还没开始翻译」，
        // 不是漏译。用「已有多少句带译文」当开关。
        let check_untranslated = self.state.translated_count() > 0;
        // 先按「无术语表」跑一遍拿到其余三项判据（术语表为空时 `glossary_violations`
        // 会立刻返回），再把上面那份缓存里的违规下标填进同一结构——
        // 计数与一键定位因此与剪辑台的高亮严格同源。
        // 阈值来自配置（`pipeline.whisper_low_confidence`，默认 -0.35），与常量同值；
        // 常量仅作为缺省来源，两者由 config.rs 的单测锁死同值。
        let threshold = self.state.config.pipeline.whisper_low_confidence;
        let mut report = quality_report(&self.state.segments, &[], threshold, check_untranslated);
        report.glossary_violations = self.cached_glossary_violations();
        Some(report)
    }

    /// 「下一处」的落点：当前选中句之后的第一处待复核；已是最后一处则回卷到第一处。
    /// 一处待复核都没有时返回 `None`（`jump_to_quality_issue` 会安全忽略）。
    fn quality_next_issue(&mut self) -> Option<usize> {
        let report = self.quality_report_snapshot()?;
        let all = report.all_issues();
        let after_current = self
            .state
            .selected_segment_index
            .and_then(|cur| all.iter().copied().find(|&idx| idx > cur));
        after_current.or_else(|| all.first().copied())
    }

    /// 一键定位：切回剪辑台 + 选中目标句 + 让字幕清单重新跟随滚动。
    ///
    /// 全部走 `AppState` 的公开面（`select_segment` / `active_tab` 字段），
    /// 不需要 `actions.rs` / `editor.rs` 提供额外入口；`subtitle_list_followed_sel`
    /// 置 `None` 是为了强制清单重新定位——切换页面本身不会滚动虚拟列表，
    /// 用户会「跳是跳过去了，视线还停在原地」。
    fn jump_to_quality_issue(&mut self, cx: &mut Context<Self>, target: Option<usize>) {
        let Some(index) = target else {
            return;
        };
        self.state.select_segment(index);
        // 剪辑台才是「选中某一句」的目的地：只选中不切页，用户还得自己找过去。
        self.state.active_tab = WorkspaceTab::Editor;
        self.subtitle_list_followed_sel = None;
        // 剪辑台的监视器与时间轴需要波形（与载入缓存后的处理同源）。
        self.ensure_waveform(cx);
        cx.notify();
    }

    /// 质检摘要卡片。返回 `None` 表示这一帧不加任何东西（无字幕 / 不属于转写完成态）。
    ///
    /// 只由「转写完成后的看板」调用（文件就绪卡与空闲引导卡），转写进行中走的是
    /// 进度看板分支，不会渲染到这里。
    fn render_quality_summary(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let report = self.quality_report_snapshot()?;
        let total = report.total_issues();
        let scored = report.confidence_scored;
        let missing = report.confidence_missing;
        let no_confidence = report.confidence_unavailable();
        // 「开始复核」的第一跳落点。必须在这里算好：下面给每个胶囊挂 `cx.listener`
        // 时要可变借用 `this`，闭包里再读 `report` 就与借用打架
        // （与 `render_batch_queue_panel` 先抽纯值是同一个处理）。
        let first_issue = report.first_issue();

        // 四类判据的展示配置：低置信走玫红、术语违规走琥珀（与剪辑台的琥珀色行提示同色）、
        // 未翻译走蓝、空/超短句走中性灰。配色全部取自 Theme 的 tint_* / accent_* 阶梯，
        // 调用点不出现裸色值。
        // 元组字段（用别名收窄，免得 clippy 判 `type_complexity`）：
        // 标签 / 句数 / 该类第一处序号 / 浅底 / 描边 / 前景。
        type QualityChip = (&'static str, usize, Option<usize>, Rgba, Rgba, Rgba);
        let rows: [QualityChip; 4] = [
            (
                "低置信",
                report.low_confidence.len(),
                report.low_confidence.first().copied(),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            ),
            (
                "术语违规",
                report.glossary_violations.len(),
                report.glossary_violations.first().copied(),
                Theme::tint_warn_soft(),
                Theme::tint_warn_border(),
                Theme::accent_orange(),
            ),
            (
                "未翻译",
                report.untranslated.len(),
                report.untranslated.first().copied(),
                Theme::tint_blue_soft(),
                Theme::tint_blue_border(),
                Theme::accent_blue(),
            ),
            (
                "空/超短句",
                report.empty_or_short.len(),
                report.empty_or_short.first().copied(),
                Theme::tint_neutral(),
                Theme::tint_neutral_border(),
                Theme::text_muted(),
            ),
        ];

        // 标题行：左「转写质检」，右状态药丸（有问题是「待复核 N 项」，全通过是「质检通过」）
        let header = div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("转写质检"),
            )
            .child(if total == 0 {
                primitives::badge_accent("质检通过")
            } else {
                primitives::badge_danger(format!("待复核 {total} 项"))
            });

        // 判据胶囊行：非零项按类着色并可点击定位；零项**保留计数但置灰不可点**——
        // 用户要看得出「这一项确实检查过、结果是 0」，而不是被悄悄藏掉。
        let chips = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .children(rows.into_iter().enumerate().map(
                |(slot, (label, count, first, soft, border, text))| {
                    if count == 0 {
                        return primitives::tag_tinted(
                            format!("{label} 0 句"),
                            Theme::tint_neutral(),
                            Theme::tint_neutral_border(),
                            Theme::text_muted(),
                        )
                        .into_any_element();
                    }
                    primitives::tag_tinted(format!("{label} {count} 句"), soft, border, text)
                        // 元素 id 用「槽位序号」而不是标签：`ElementId` 只接受
                        // `&'static str` / `SharedString` / 数字，`(&str, &str)` 装不进去。
                        .id(("quality-chip", slot))
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.jump_to_quality_issue(cx, first);
                        }))
                        .into_any_element()
                },
            ));

        // 置信度覆盖面：SenseVoice / 旧记录 / 流式预览片段都没有逐句置信度，
        // 此时「低置信」一栏恒为空，必须说清是「判不了」而不是「没问题」。
        let coverage = if missing == 0 {
            format!("置信度已评估 {scored} 句")
        } else if no_confidence {
            format!("当前引擎不提供逐句置信度（{missing} 句缺该项数据），低置信复核不可用")
        } else {
            format!("置信度已评估 {scored} 句 · {missing} 句缺该项数据")
        };

        let body: AnyElement = if total == 0 {
            // 零问题：给正向状态，而不是一排 0/0/0 的空壳。
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(Theme::SPACE_2))
                .child(primitives::stat_dot_sm(Theme::accent_mint()))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(Theme::accent_mint())
                        .child("质检通过：未发现低置信、术语违规或碎片句"),
                )
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_2))
                .child(chips)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(Theme::SPACE_2))
                        .child(
                            primitives::btn(
                                "开始复核",
                                primitives::BtnSize::Sm,
                                primitives::BtnVariant::Primary,
                            )
                            .id("quality-review-first")
                            .cursor_pointer()
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.jump_to_quality_issue(cx, first_issue);
                                },
                            )),
                        )
                        .child(
                            primitives::btn(
                                "下一处",
                                primitives::BtnSize::Sm,
                                primitives::BtnVariant::Secondary,
                            )
                            .id("quality-review-next")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let next = this.quality_next_issue();
                                this.jump_to_quality_issue(cx, next);
                            })),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_muted())
                                .child("点某一类可定位该类第一处"),
                        ),
                )
                .into_any_element()
        };

        Some(
            primitives::card_sm()
                .w_full()
                .max_w(px(Theme::CONTENT_MAX_W))
                .gap(px(Theme::SPACE_2))
                .child(header)
                .child(body)
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(Theme::text_muted())
                        .child(coverage),
                )
                .into_any_element(),
        )
    }

    /// 批量转写队列面板 (F-012)：文件清单 + 「开始全部 / 终止 / 清空」操作组。
    ///
    /// 队列非空时常驻在工作区顶部，让用户随时看清哪些跑完了、哪些还在等，
    /// 而不是只能盯着一条进度条猜整批的进度。
    pub(crate) fn render_batch_queue_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let batch_running = self.state.batch_running;
        let total = self.state.batch_queue.len();
        let pending = self.state.queue_pending_count();
        let (done, failed, _) = self.state.queue_summary();
        let total_dur = self.state.queue_total_duration();
        let active = self.state.batch_active;
        let can_start = !is_processing && pending > 0;

        // 先把要渲染的数据抽成纯值。下面给「移除」按钮挂 cx.listener 需要可变借用
        // 上下文，如果行内容还借着 self.state，两个借用会打架。
        // kind: 0 等待 / 1 转写中 / 2 已完成 / 3 失败
        let rows: Vec<(usize, String, String, f64, u8)> = self
            .state
            .batch_queue
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let kind = match item.state {
                    QueueState::Pending => 0u8,
                    QueueState::Running => 1,
                    QueueState::Done { .. } => 2,
                    QueueState::Failed(_) => 3,
                };
                (
                    idx,
                    item.name.clone(),
                    item.state.label(),
                    item.duration,
                    kind,
                )
            })
            .collect();

        // 面板外壳走 card_sm()：批量队列是工作区里的次级区块，用紧凑内边距；
        // 运行中换成薄荷描边提示「这批正在跑」。
        primitives::card_sm()
            .id("batch-queue-panel")
            .w_full()
            .flex_shrink_0()
            .border_color(if batch_running {
                Theme::tint_mint_border()
            } else {
                Theme::border()
            })
            .gap(px(Theme::SPACE_2))
            // ── 标头：统计徽章 + 操作组 ──
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(Theme::SPACE_3))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(primitives::section_title("批量转写队列"))
                            // 计数 / 失败 / 累计时长三类元信息统一走 badge 阶梯
                            .child(primitives::badge(format!("{done}/{total} 完成 · {pending} 待处理")))
                            .children(if failed > 0 {
                                Some(primitives::badge_danger(format!("{failed} 失败")))
                            } else {
                                None
                            })
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child(format!(
                                        "累计时长 {}",
                                        format_duration_short(total_dur)
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::btn("批量导入", primitives::BtnSize::Sm, primitives::BtnVariant::Secondary)
                                    .id("queue-add-files-btn")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_batch_files(cx)),
                                    ),
                            )
                            .child(if is_processing {
                                primitives::btn("终止批量", primitives::BtnSize::Sm, primitives::BtnVariant::Danger)
                                    .id("queue-cancel-btn")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_batch_queue(cx)),
                                    )
                                    .into_any_element()
                            } else {
                                // 「开始全部」有可点 / 不可点两态，禁用态需要自绘底色与文字色，
                                // 因此沿用 btn 的骨架再按 can_start 覆盖配色。
                                primitives::btn("开始全部", primitives::BtnSize::Sm, primitives::BtnVariant::Primary)
                                    .id("queue-start-all-btn")
                                    .bg(if can_start {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::bg_hover_strong()
                                    })
                                    .text_color(if can_start {
                                        Theme::text_on_accent()
                                    } else {
                                        Theme::text_muted()
                                    })
                                    .when(can_start, |s| s.cursor_pointer().hover(|s| s.opacity(0.9)))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.start_batch_queue(cx)),
                                    )
                                    .into_any_element()
                            })
                            .child(
                                primitives::btn("清空", primitives::BtnSize::Sm, primitives::BtnVariant::Ghost)
                                    .id("queue-clear-btn")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // 清空会连同正在跑的转写一起终止（`clear_batch_queue`
                                        // 内部先 cancel），而按钮是最弱的 Ghost 样式、又紧挨
                                        // 着「开始全部」，误触代价却是丢掉整批任务。
                                        // 队列为空时没什么可清，直接跳过弹窗。
                                        if this.state.batch_queue.is_empty() {
                                            return;
                                        }
                                        let running = matches!(
                                            this.state.status,
                                            ProcessStatus::Processing { .. }
                                        );
                                        this.confirm_dialog = Some(ConfirmDialogInfo {
                                            title: "清空批量队列？".to_string(),
                                            message: if running {
                                                "队列里所有文件都会被移除，**正在进行的转写也会被终止**。已转写完成并落库的工程不受影响。"
                                                    .to_string()
                                            } else {
                                                "队列里所有文件都会被移除。已转写完成并落库的工程不受影响。"
                                                    .to_string()
                                            },
                                            confirm_label: "清空".to_string(),
                                            danger: running,
                                            action: ConfirmAction::ClearBatchQueue,
                                        });
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            // ── 文件清单：超高时面板内部滚动，不把整个工作区撑开 ──
            .child(
                div()
                    .id("batch-queue-list")
                    .w_full()
                    .max_h(px(Theme::QUEUE_LIST_MAX_H))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1))
                    .children(rows.into_iter().map(|(idx, name, label, dur, kind)| {
                        let dot = match kind {
                            1 => Theme::accent_orange(),
                            2 => Theme::accent_mint(),
                            3 => Theme::accent_red(),
                            _ => Theme::bg_dot_idle(),
                        };
                        let label_color = match kind {
                            1 => Theme::accent_orange(),
                            3 => Theme::accent_red(),
                            _ => Theme::text_muted(),
                        };
                        let is_active = active == Some(idx);
                        div()
                            .id(("batch-queue-row", idx))
                            .w_full()
                            .flex_shrink_0()
                            .px(px(Theme::SPACE_2))
                            .py(px(Theme::SPACE_1_5))
                            .rounded(px(Theme::RADIUS_LG))
                            .bg(if is_active {
                                Theme::tint_mint_soft()
                            } else {
                                Theme::bg_inset()
                            })
                            .border_1()
                            .border_color(if is_active {
                                Theme::tint_mint_border()
                            } else {
                                Theme::border_subtle()
                            })
                            .flex()
                            .items_center()
                            .gap(px(Theme::SPACE_2))
                            .child(
                                primitives::stat_dot_sm(dot),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .truncate()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(if is_active {
                                        FontWeight::BOLD
                                    } else {
                                        FontWeight::MEDIUM
                                    })
                                    .text_color(Theme::text_primary())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child(if dur > 0.0 {
                                        format_duration_short(dur)
                                    } else {
                                        "--:--".to_string()
                                    }),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .max_w(px(Theme::QUEUE_LABEL_MAX_W))
                                    .truncate()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(label_color)
                                    .child(label),
                            )
                            .child(
                                if is_active && is_processing {
                                    // 正在转写的条目不给删：删了会让「当前项」失去归属，
                                    // 用户想停应该用「终止批量」。
                                    div()
                                        .flex_shrink_0()
                                        .px(px(Theme::SPACE_1_5))
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .text_color(Theme::accent_orange())
                                        .child("转写中")
                                        .into_any_element()
                                } else {
                                    div()
                                        .id(("batch-queue-remove", idx))
                                        .flex_shrink_0()
                                        .px(px(Theme::SPACE_1_5))
                                        .rounded(px(Theme::RADIUS_MD))
                                        .cursor_pointer()
                                        .text_size(px(Theme::TEXT_SMALL))
                                        .text_color(Theme::text_muted())
                                        .hover(|s| {
                                            s.bg(Theme::bg_hover_strong())
                                                .text_color(Theme::accent_red())
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.remove_queue_item(idx, cx)
                                        }))
                                        .child("移除")
                                        .into_any_element()
                                },
                            )
                    })),
            )
            .into_any_element()
    }
}
