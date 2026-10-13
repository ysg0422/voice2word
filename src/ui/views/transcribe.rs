//! 语音转写主工作台视图

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{ConfirmAction, ConfirmDialogInfo};
use super::super::MainWindow;
use crate::app::state::{ProcessStatus, QueueItem, QueueState, WorkspaceTab};
use crate::ui::format_mm_ss;
// 「修复常见错误」的命中清单 / 重复句分组都来自 `cleanup.rs` 的纯函数：
// 这里只把结果画出来，并把它们按「字幕版本 + 勾选规则」缓存起来（见 `TranscribeDerivedCache`）。
use crate::subtitle::cleanup::{CleanupPlan, CleanupRules, DuplicateGroup};
// 质检报告是 `segment.rs` 里的纯逻辑：这里只把结果画出来，不在界面层重算判据。
use crate::subtitle::segment::{QualityCategory, QualityReport};
use crate::utils::time::format_duration_short;
use std::sync::Arc;

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
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let is_sensevoice =
            self.state.whisper_model_tier == crate::app::WhisperModelTier::SenseVoice;
        let file_loaded = self.state.transcribe_file.is_some();
        // 「字幕确实属于这支媒体」的判据与质检卡片完全一致（`selected_file ==
        // transcribe_file`）：只数 `segments` 非空的话，刚换了一支还没转写的新片，
        // 上一支留下的字幕会把第 2 步说成「已完成」。
        let subtitles_ready = !self.state.segments.is_empty()
            && self.state.selected_file.as_ref() == self.state.transcribe_file.as_ref();
        // 步骤条四步与右侧状态药丸的判定都走纯函数：它们各有四五条分支，写在渲染链里
        // 只能用肉眼比对，而它们说的正是「现在在第几步、正在干什么」——对不上就是误导。
        let steps = StepChain::of(file_loaded, is_processing, subtitles_ready);
        let status_pill =
            transcribe_status_pill(is_processing, self.state.batch_running, file_loaded);

        // 页面外壳统一走 primitives::page_shell：底色 / 内边距 / 分区间距与其余三个
        // 工作台页同源，切页时中央列不再横向跳动；`.flex_1()` 保留它在左右分栏里
        // 撑满剩余宽度的职责。
        primitives::page_shell("transcribe-main-workspace")
            .flex_1()
            // ── 顶部步骤流式导航栏：转写是一条「导入媒体 → 语音转写 → 剪辑校对 → 导出工程」的直线，
// 步骤常驻在顶部，用户随时知道自己在第几步、下一步该干什么 ──
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
                            .child(step_item(
                                "1",
                                "导入媒体",
                                steps.import,
                                Theme::accent_mint(),
                                Theme::text_on_accent(),
                            ))
                            .child(primitives::step_connector())
                            .child(step_item(
                                "2",
                                "语音转写",
                                steps.transcribe,
                                Theme::accent_mint(),
                                Theme::text_on_accent(),
                            ))
                            .child(primitives::step_connector())
                            .child(step_item(
                                "3",
                                "剪辑校对",
                                steps.edit,
                                Theme::accent_blue(),
                                Theme::text_on_saturated(),
                            ))
                            .child(primitives::step_connector())
                            .child(step_item(
                                "4",
                                "导出工程",
                                steps.export,
                                Theme::accent_blue(),
                                Theme::text_on_saturated(),
                            ))
                    )
                    .child(
                        // 右侧状态药丸：四态由 `transcribe_status_pill` 先判定（批量与单文件转写
                        // 必须说两种话），外观一律走 badge / stat_dot_sm 原语。
                        if let Some(label) = status_pill.transcribing_label() {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_1))
                                .child(primitives::stat_dot_sm(Theme::accent_mint()))
                                .child(primitives::badge_accent(label))
                        } else if status_pill == StatusPill::FileReady {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Theme::SPACE_1))
                                .child(primitives::stat_dot_sm(Theme::accent_blue()))
                                .child(primitives::badge(
                                    self.state
                                        .transcribe_file
                                        .as_ref()
                                        .and_then(|f| f.file_name())
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("媒体文件")
                                        .to_string(),
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
                        _ => ("正在全速转写中…".to_string(), 0.0, String::new()),
                    };
                    let total_dur = self.state.transcribe_duration;
                    let cur_sec = self.state.streaming_current_sec;
                    // `mm:ss` 换算统一走 `ui::format_mm_ss`：本文件此前手写的三份
                    // 与状态栏各自维护的同一换算，现在同源。
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
                                                            if is_sensevoice { "SenseVoice 正在通过纯内存音频管道极速识别…".to_string() } else { "Whisper 模型正在转写音频流…".to_string() }
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
                                        .child(primitives::badge(format_mm_ss(total_dur))),
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
                                                    format!(
                                                        "进度：{} / {}（{:.1}%）",
                                                        format_mm_ss(cur_sec),
                                                        format_mm_ss(total_dur),
                                                        display_ratio * 100.0
                                                    )
                                                } else {
                                                    "正在实时推流…".to_string()
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
                                                "SenseVoice 极速模型正在解析音频流，首句字幕即将秒级呈现…"
                                            } else {
                                                "AI 语音引擎正在加载权重并解析音频流，首句字幕即将实时呈现…"
                                            })
                                            .into_any_element()
                                    } else {
                                        // 只回显最后 `STREAM_TAIL_ROWS` 行：`streaming_segments` 是
                                        // 按到达顺序 push 的，直接取尾部切片即可——原写法
                                        // `rev().take(3)` 收进 `Vec` 再 `rev()` 收了第二遍，每帧两次分配。
                                        let all = &self.state.streaming_segments;
                                        let seg_list =
                                            &all[all.len().saturating_sub(STREAM_TAIL_ROWS)..];
                                        let total_items = seg_list.len();
                                        div()
                                            .min_h(px(Theme::STREAM_MIN_H))
                                            .w_full()
                                            .flex()
                                            .flex_col()
                                            .justify_end()
                                            .gap_2()
                                            .children(seg_list.iter().enumerate().map(|(i, seg)| {
                                                let is_last = is_last_stream_row(i, total_items);
                                                let time_badge = format!(
                                                    "{} - {}",
                                                    format_mm_ss(seg.start),
                                                    format_mm_ss(seg.end)
                                                );

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
                        // 底部 4 宫格四格等分：`.flex_1()` 必须与 `.min_w(px(0.0))` 成对出现——
                        // 只写 `flex_1` 时 flex 项默认的 `min-width:auto` 会按内容最小宽度顶住，
                        // 「SenseVoice INT8」那一格会把其余三格挤窄。
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
                                        .min_w(px(0.0))
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("总体进度"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{:.1}%", display_ratio * 100.0)))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child(format!("{} / {}", format_mm_ss(cur_sec), format_mm_ss(total_dur)))),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("预计耗时"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(eta))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child(if is_sensevoice { "极速 14.5x 实时倍速" } else { "Whisper 并行解码" })),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .gap_0p5()
                                        .child(div().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_muted()).child("音频推流"))
                                        .child(div().text_size(px(Theme::TEXT_STAT)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child("纯内存管道"))
                                        .child(div().text_size(px(Theme::TEXT_CAPTION)).text_color(Theme::text_secondary()).child("0 磁盘 I/O · 0 磨损")),
                                )
                                .child(
                                    primitives::card_sm()
                                        .flex_1()
                                        .min_w(px(0.0))
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
                                                        .child(format!("时长：{}", dur_str)),
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
                                                .child(format!("命中本地转写缓存（含 {} 句字幕）", cached.segments.len())),
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
                                                primitives::btn("开始转写", primitives::BtnSize::Lg, primitives::BtnVariant::Primary)
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
                        .gap(px(Theme::SPACE_4))
                        .p(px(Theme::PAGE_PAD))
                        .child(
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
                                        .text_size(px(Theme::TEXT_DISPLAY))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_blue())
                                        .child("+"),
                                )
                                .child(primitives::page_title("导入音视频文件"))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
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
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(Theme::SPACE_2))
                                                .child(
                                                    primitives::chip_clickable("批量导入", false, false)
                                                        .id("idle-batch-import-btn")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.choose_batch_files(cx);
                                                        })),
                                                )
                                                .child(
                                                    primitives::chip_clickable("导入文件夹", false, false)
                                                        .id("idle-batch-folder-btn")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.choose_batch_folder(cx);
                                                        })),
                                                )
                                                .child(
                                                    primitives::chip_clickable("从视频库选择", false, false)
                                                        .id("idle-goto-library-btn")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.active_tab = WorkspaceTab::Library;
                                                            this.state.refresh_recent_tasks();
                                                            cx.notify();
                                                        })),
                                                )
                                                .child(
                                                    primitives::chip_clickable("导入已有字幕", false, false)
                                                        .id("idle-import-subtitle-btn")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.import_subtitle_file(cx);
                                                        })),
                                                ),
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
    /// 报告本身走 [`MainWindow::derived_snapshot`]：那份快照按
    /// 「`segments_revision` + 低置信阈值 + 术语表 + 目标语言 + 样式 / 时长」失效，
    /// 术语违规由同一份条目现算——剪辑台的高亮用的也是这条判据，于是同一份数据在
    /// 两个页面给出同一个数字（不会「这页 3 句、那页 5 句」），而这次 `O(n)`
    /// 遍历只在字幕、术语表或阈值真的变了时付一次（原先每帧一次）。
    ///
    /// 返回 `Arc` 而不是克隆整份报告：`QualityReport` 里是四个 `Vec`，逐帧深拷贝
    /// 等于把省下的遍历又还回去。
    fn quality_report_snapshot(&mut self) -> Option<Arc<QualityReport>> {
        if self.state.segments.is_empty() {
            return None;
        }
        Some(self.derived_snapshot().quality_report.clone())
    }

    /// 「下一处」的落点：当前选中句之后的第一处待复核；已是最后一处则回卷到第一处。
    /// 一处待复核都没有时返回 `None`（`jump_to_quality_issue` 会安全忽略）。
    ///
    /// `category` 为 `None` 时跨全部四类翻查（原有的「下一处」语义）；
    /// 为 `Some(cat)` 时**只在该类内**推进——用户往往只在意某一类问题
    /// （例如只盯低置信句），跨类跳会把他引到不关心的行上。
    ///
    /// 边界（空类、当前句本身是问题、越过末尾回卷）全部交给纯函数
    /// [`crate::subtitle::segment::next_issue_in`]，界面层不重复实现一遍。
    fn quality_next_issue_in(&mut self, category: Option<QualityCategory>) -> Option<usize> {
        let report = self.quality_report_snapshot()?;
        let current = self.state.selected_segment_index;
        match category {
            Some(cat) => {
                let indices = cat.indices(&report);
                crate::subtitle::segment::next_issue_in(current, &indices)
            }
            None => {
                let all = report.all_issues();
                crate::subtitle::segment::next_issue_in(current, &all)
            }
        }
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
        // 「开始复核」的第一跳落点。必须在这里算好：下面给每个胶囊挂 `cx.listener`
        // 时要可变借用 `this`，闭包里再读 `report` 就与借用打架
        // （与 `render_batch_queue_panel` 先抽纯值是同一个处理）。
        let first_issue = report.first_issue();

        // 四类判据的展示配置：低置信走玫红、术语违规走琥珀（与剪辑台的琥珀色行提示同色）、
        // 未翻译走蓝、空/超短句走中性灰。配色全部取自 Theme 的 tint_* / accent_* 阶梯，
        // 调用点不出现裸色值。
        // 元组字段（用别名收窄，免得 clippy 判 `type_complexity`）：
        // 分类 / 句数 / 该类第一处序号 / 浅底 / 描边 / 前景。
        // 分类用枚举而不是再抄一遍中文标签：标签统一由 `QualityCategory::label()` 给出，
        // 与下面的「只看这类」按钮同源（抄成字面量就会出现「胶囊叫空/超短句、
        // 按钮叫碎片句」这种两处各写一份的漂移）。
        type QualityChip = (QualityCategory, usize, Option<usize>, Rgba, Rgba, Rgba);
        let rows: [QualityChip; 4] = [
            (
                QualityCategory::LowConfidence,
                report.low_confidence.len(),
                report.low_confidence.first().copied(),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            ),
            (
                QualityCategory::GlossaryViolation,
                report.glossary_violations.len(),
                report.glossary_violations.first().copied(),
                Theme::tint_warn_soft(),
                Theme::tint_warn_border(),
                Theme::accent_orange(),
            ),
            (
                QualityCategory::Untranslated,
                report.untranslated.len(),
                report.untranslated.first().copied(),
                Theme::tint_blue_soft(),
                Theme::tint_blue_border(),
                Theme::accent_blue(),
            ),
            (
                QualityCategory::EmptyOrShort,
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
        // 唯一的例外是低置信判据**全无置信度数据**时：那时「0 句」是把「没测」说成了
        // 「通过」，文案改由 `quality_chip_label` 给「未评估」（判据见 `QualityReport`）。
        let confidence_unavailable = report.confidence_unavailable();
        let chips = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .children(rows.into_iter().enumerate().map(
                |(slot, (category, count, first, soft, border, text))| {
                    let label = quality_chip_label(category, count, confidence_unavailable);
                    if count == 0 {
                        return primitives::tag_tinted(
                            label,
                            Theme::tint_neutral(),
                            Theme::tint_neutral_border(),
                            Theme::text_muted(),
                        )
                        .into_any_element();
                    }
                    primitives::tag_tinted(label, soft, border, text)
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

        let coverage = if scored > 0 {
            format!("置信度已评估 {scored} 句")
        } else {
            String::new()
        };

        // 按类翻查：用户往往只关心某一类问题（例如只盯低置信句）。给每一类一个
        // 「只看这类」按钮，点一下就从当前句开始在这类内部往前推。空类不生成按钮——
        // 点了也只会原地不动，不如藏掉。按钮文案用 `QualityCategory::label()`，
        // 与上面的胶囊同源，不会出现「胶囊叫空/超短句、按钮叫碎片句」这种不一致。
        let category_jump_buttons: Vec<AnyElement> = QualityCategory::ALL
            .iter()
            // 只问「这一类有没有条目」：`QualityCategory::indices()` 会克隆整份下标
            // 列表再逐个去重，四个类别就是每帧四次分配 + 四次去重扫描（`category_has_issues`）。
            .filter(|cat| category_has_issues(&report, **cat))
            .map(|cat| {
                let cat = *cat;
                primitives::btn(
                    format!("只看{}", cat.label()),
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id(("quality-jump-cat", cat as usize))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    let next = this.quality_next_issue_in(Some(cat));
                    this.jump_to_quality_issue(cx, next);
                }))
                .into_any_element()
            })
            .collect();

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
                        .child("质检通过"),
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
                                let next = this.quality_next_issue_in(None);
                                this.jump_to_quality_issue(cx, next);
                            })),
                        )
                        // 导出复核表：质检结论此前只活在界面里，没法交给别人复核。
                        // 放在这一排而不是塞进导出栏，是因为它属于「质检」这个动作的
                        // 收尾，用户在这里刚看完结论、顺手导出最自然。
                        .child(
                            primitives::btn(
                                "导出复核表 CSV",
                                primitives::BtnSize::Sm,
                                primitives::BtnVariant::Secondary,
                            )
                            .id("quality-export-csv")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_review_sheet(crate::subtitle::qc::QcFormat::Csv, cx);
                            })),
                        )
                        .child(
                            primitives::btn(
                                "Markdown",
                                primitives::BtnSize::Sm,
                                primitives::BtnVariant::Secondary,
                            )
                            .id("quality-export-md")
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_review_sheet(
                                    crate::subtitle::qc::QcFormat::Markdown,
                                    cx,
                                );
                            })),
                        )
                        .children(category_jump_buttons),
                )
                .into_any_element()
        };

        // 修复区块先渲染（需要 &mut self），再拼进卡片尾部
        let cleanup_section = self.render_cleanup_section(cx);
        Some(
            primitives::card_sm()
                .w_full()
                .max_w(px(Theme::CONTENT_MAX_W))
                .gap(px(Theme::SPACE_2))
                .child(header)
                .child(body)
                .children((!coverage.is_empty()).then(|| {
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(Theme::text_muted())
                        .child(coverage)
                }))
                .child(cleanup_section)
                .into_any_element(),
        )
    }

    /// 「修复常见错误」区块：四类机械修复的勾选 + 一键应用 + 相邻重复句折叠。
    ///
    /// 挂在质检卡片里而不是另开一张卡：用户的动线是「看到问题 → 顺手修掉」，
    /// 结论与修复入口分家，他就得在两个地方来回找。计数直接来自与落地同一套的
    /// 纯函数（`cleanup::plan`），按钮上的数字就是点下去真正会改的处数。
    fn render_cleanup_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::subtitle::cleanup::CleanupRule;

        let rules = self.state.cleanup_rules;
        // 命中清单与重复句分组都是**整表扫描**（`cleanup::plan` 对每一句、每一侧都跑一遍
        // 四条规则并各自分配字符串；`duplicate_groups` 再逐句比对），而这张卡片在就绪态
        // 每帧都要重画。按「字幕版本 + 勾选规则」缓存成一份快照：两个入参恰好就是
        // 结果的全部依据（`plan` 吃 segments + rules，`groups` 只吃 segments）。
        let preview = {
            let key = (self.state.segments_revision, rules);
            let state = &self.state;
            cx.update_default_global::<TranscribeDerivedCache, _>(|cache, _| {
                cleanup_preview_cached(&mut cache.cleanup, key, || CleanupPreview {
                    plan: state.cleanup_preview(),
                    groups: state.duplicate_cue_groups(),
                })
            })
        };
        let plan = &preview.plan;
        let groups = &preview.groups;
        // 每组至少 2 句，可删的是「组内除第一句以外」的那些
        let duplicate_cues: usize = groups.iter().map(|g| g.indices.len() - 1).sum();
        let note = self.state.cleanup_note.clone();
        // 样例先算成 String：下面给按钮挂监听时要可变借用 `this`，闭包里再读 plan
        // 就与借用打架（与质检胶囊同一种处理）。
        let sample = plan.first().map(|hit| {
            let after = if hit.after.trim().is_empty() {
                "（清空）".to_string()
            } else {
                truncate_for_ui(&hit.after)
            };
            format!(
                "例（{}·{}）：{} → {}",
                hit.target.label(),
                hit.rule.label(),
                truncate_for_ui(&hit.before),
                after
            )
        });

        let chips = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .children(
                CleanupRule::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(slot, rule)| {
                        // 零处也照样显示：用户要看得出「这一类确实检查过、结果是 0」，
                        // 而不是被悄悄藏掉；勾选态与命中数分开表达，才知道自己关了哪条。
                        primitives::chip_clickable(
                            format!("{} {}", rule.label(), plan.count_of(rule)),
                            rules.enabled(rule),
                            false,
                        )
                        .id(("cleanup-rule", slot))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.state.toggle_cleanup_rule(rule);
                            cx.notify();
                        }))
                        .into_any_element()
                    }),
            );

        let apply_enabled = !plan.is_empty();
        let apply_btn = primitives::btn_state(
            format!("应用修复（{} 处）", plan.len()),
            primitives::BtnSize::Sm,
            primitives::BtnVariant::Primary,
            apply_enabled,
        )
        .id("cleanup-apply")
        .when(apply_enabled, |d| {
            d.on_click(cx.listener(|this, _, _, cx| {
                // 不跳页、不换选中：修复是「原地改字」，把用户甩到剪辑台会打断
                // 他刚在看的质检结论；结果提示就落在这个区块下面。
                this.state.apply_cleanup();
                cx.notify();
            }))
        });

        let fold_enabled = duplicate_cues > 0;
        let fold_btn = primitives::btn_state(
            if fold_enabled {
                format!("折叠重复句（删 {duplicate_cues} 句）")
            } else {
                "无重复句".to_string()
            },
            primitives::BtnSize::Sm,
            primitives::BtnVariant::Secondary,
            fold_enabled,
        )
        .id("cleanup-fold-duplicates")
        .when(fold_enabled, |d| {
            d.on_click(cx.listener(|this, _, _, cx| {
                this.state.fold_duplicate_segments();
                cx.notify();
            }))
        });

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_2))
            .child(primitives::divider())
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child("修复常见错误"),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(format!(
                                "{} 句可修 · {} 组重复句",
                                plan.affected_segments(),
                                groups.len()
                            )),
                    ),
            )
            .child(chips)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(apply_btn)
                    .child(fold_btn),
            )
            .children(sample.map(|text| {
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(text)
            }))
            .children(note.map(|text| {
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::accent_mint())
                    .child(text)
            }))
            .into_any_element()
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
        let rows: Vec<QueueRow> = self
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
                QueueRow {
                    idx,
                    name: item.name.clone(),
                    label: item.state.label(),
                    duration: item.duration,
                    kind,
                    progress: queue_progress_view(item),
                    stage: item.stage.clone(),
                }
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
                            .child(
                                primitives::btn("导入文件夹", primitives::BtnSize::Sm, primitives::BtnVariant::Secondary)
                                    .id("queue-add-folder-btn")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_batch_folder(cx)),
                                    ),
                            )
                            // 只有批量模式在跑时才显示「终止批量」：单文件转写也会置
                            // `is_processing`，若按它判断，单文件转写时这里会误显示「终止批量」
                            // （真正该用的「终止转写」在进度抽屉里）。用 `batch_running` 才准。
                            .child(if batch_running {
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
                                    // 禁用态（队列为空或已在转写）不能再挂 on_click：
                                    // 否则点下去会走 start_batch_queue → 置 Failed 弹出全屏错误横幅，
                                    // 与「按钮外观已禁用」自相矛盾。
                                    .when(can_start, |s| {
                                        s.on_click(
                                            cx.listener(|this, _, _, cx| this.start_batch_queue(cx)),
                                        )
                                    })
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
                    .track_scroll(&self.batch_queue_scroll)
                    // 内嵌队列自己吃掉滚轮：还有余量时不让事件继续冒泡到外层页面壳，
                    // 避免「队列与页面一起滚」（判据同编辑台字幕清单）。
                    // 到边界时放行，由页面接管。
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        let delta_y = match event.delta {
                            ScrollDelta::Lines(p) => p.y,
                            ScrollDelta::Pixels(p) => f32::from(p.y),
                        };
                        let offset_y = f32::from(this.batch_queue_scroll.offset().y);
                        let max_h = f32::from(this.batch_queue_scroll.max_offset().height);
                        if crate::ui::primitives::should_consume_scroll(delta_y, offset_y, max_h) {
                            cx.stop_propagation();
                        }
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_1))
                    .children(rows.into_iter().map(|row| {
                        let QueueRow {
                            idx,
                            name,
                            label,
                            duration: dur,
                            kind,
                            progress,
                            stage,
                        } = row;
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
                        // 有进度就多挂一行（进度条 + 百分比 + 阶段名），没有就保持
                        // 原来的单行卡片：排序、间距、圆角、描边一律走同一套值，
                        // 只是外层从「一行」变成「一列」，不改既有布局体系。
                        let progress_line = progress
                            .map(|p| render_queue_progress(p, stage.as_deref()));
                        let has_progress = progress_line.is_some();
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
                            // 进度条**整行宽**，所以它必须待在文件名行之外：
                            // 塞进那一行里会被右侧的时长 / 状态 / 移除按钮挤成
                            // 短短一截，而卡片底下又要空出大片留白。
                            .when_some(progress_line, |s, line| {
                                s.flex()
                                    .flex_col()
                                    .gap(px(Theme::SPACE_1))
                                    .child(
                                        div()
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .gap(px(Theme::SPACE_2))
                                            .child(primitives::stat_dot_sm(dot)),
                                    )
                                    .child(line)
                            })
                            .when(!has_progress, |s| {
                                s.flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .child(primitives::stat_dot_sm(dot))
                            })
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

/// 一条队列行的渲染数据（纯值）。
///
/// 为什么先抽出来：行内容若还借着 `self.state`，下面给「移除」按钮挂
/// `cx.listener` 要的可变借用就会和它打架。
#[derive(Debug)]
struct QueueRow {
    idx: usize,
    name: String,
    label: String,
    duration: f64,
    /// 0 等待 / 1 转写中 / 2 已完成 / 3 失败
    kind: u8,
    /// 本条进度（`None` = 还没开始跑，不画进度）
    progress: Option<QueueProgressView>,
    /// 管线给的阶段名，仅「转写中」时非空
    stage: Option<String>,
}

/// 「本条进度」在队列卡片上的呈现取值（**纯数据**，便于单测）。
#[derive(Debug, Clone, PartialEq)]
struct QueueProgressView {
    /// 进度条填充比例（0.0 ~ 1.0）。由百分比换算而来，因为 `progress_bar`
    /// 吃的是比率，而队列里存的是百分比。
    ratio: f32,
    /// 百分比整数。用整数是刻意的：`{:.1}%` 会在同一帧里反复改宽度，
    /// 卡片右侧整列状态文字会跟着左右抖动。
    percent: u8,
    /// 是否画出进度条（只有「没满格」才会走到构造点，因此恒为 `true`；
    /// 保留这个字段是为了让渲染侧不必自己再判一次满格）
    show_bar: bool,
}

/// 一条队列项在卡片上要画的进度：`None` = 既不画条也不显示百分比。
///
/// **满格（100%）也返回 `None`**：成功条目的状态文案「已完成 12 句」已经把结果
/// 说清了，再挂一根满条 + 100% 只是噪声。失败条目不受此限——它停在部分进度上，
/// 那个数字恰恰说明「跑到哪儿才炸的」。`AppState::finish_active_queue_item`
/// 成功时把进度定格成 100 而不是 1.0，就是为了和这里的判据共用「百分比」这套单位。
fn queue_progress_view(item: &QueueItem) -> Option<QueueProgressView> {
    match item.state {
        QueueState::Pending => return None,
        // 转写中 / 已完成 / 失败三种都走同一条判定：失败停在出事的百分比上，
        // 「跑到 62% 才炸」和「一开始就炸」对用户是两条完全不同的信息。
        QueueState::Running | QueueState::Done { .. } | QueueState::Failed(_) => {}
    }
    // 队列里存的就是 0~100 的百分比，这里只夹一次、换算一次
    let pct = item.progress.clamp(0.0, 100.0);
    let ratio = (pct / 100.0) as f32;
    // 满格 = 没有任何新信息可报：结果文案已经说了「已完成 12 句」，再画一根
    // 满条 + 100% 只是噪声。**失败不在此列**——失败停在部分进度上，那个数字
    // 恰恰说明「跑到哪儿才炸的」。
    if pct >= 100.0 || !pct.is_finite() {
        return None;
    }
    Some(QueueProgressView {
        ratio,
        percent: if pct > 0.0 { pct.ceil() as u8 } else { 0 },
        show_bar: true,
    })
}

/// 队列卡片第二行：本条进度条 + 百分比 + 阶段名。
///
/// 复用 `primitives::progress_bar`（轨道色 / 填充色 / 高度与其它进度条同源），
/// 只补一行文字与间距，不改现有布局体系。
fn render_queue_progress(p: QueueProgressView, stage: Option<&str>) -> Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(Theme::SPACE_2))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .child(primitives::progress_bar(p.ratio)),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(Theme::TEXT_SMALL))
                .text_color(Theme::text_muted())
                .child(format!("{}%", p.percent)),
        )
        .when_some(stage.map(str::to_string), |s, stage| {
            s.child(
                div()
                    .flex_shrink_0()
                    .max_w(px(Theme::QUEUE_LABEL_MAX_W))
                    .truncate()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(stage),
            )
        })
}

/// 流式看板里「当前行是不是最后一行」的判据（`i` 从 0 起，纯函数便于单测）。
///
/// 为什么要单独判空：看板外层用 `seg_count`（数据源计数）决定显示占位文案还是
/// 字幕窗口，而窗口列表后面还会 `take(3)`，两者口径可能不一致——`seg_count > 0`
/// 时窗口仍可能为空。旧写法 `i == total_items - 1` 在空列表时会回绕成
/// `usize::MAX`，`i != usize::MAX` 恰好恒真才没 panic，判据本身已经悬空。
fn is_last_stream_row(i: usize, total: usize) -> bool {
    total > 0 && i.checked_add(1) == Some(total)
}

/// 预览样例里的一段文本：压成单行、只留头 24 个字符。
///
/// 样例是给用户「确认改的是哪一类东西」用的，不是给他读全文的；一条长句铺开会把
/// 下面的按钮挤出可视区，所以宁可截断。
fn truncate_for_ui(text: &str) -> String {
    const MAX_CHARS: usize = 24;
    // 换行与连续空白先压成一个空格：字幕文本里的多空格/换行在样例里没有信息量，
    // 原样带出来只会让「例：…」这一行看起来像排版坏了。
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = flat.trim();
    let mut out: String = flat.chars().take(MAX_CHARS).collect();
    if flat.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

/// 流式看板回显的最近行数。
///
/// 够看清识别节奏，又不至于每来一句就把整列布局重排一遍。
const STREAM_TAIL_ROWS: usize = 3;

// ==================== 步骤条 / 状态药丸（纯判定） ====================

/// 步骤条上一步所处的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepPhase {
    /// 已经完成
    Done,
    /// 当前所处的一步
    Current,
    /// 还没到
    Upcoming,
}

/// 转写工作台顶部四步步骤条的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StepChain {
    import: StepPhase,
    transcribe: StepPhase,
    edit: StepPhase,
    export: StepPhase,
}

impl StepChain {
    /// 由真实状态推出四步各自处在哪一阶段（纯判定，便于单测）。
    ///
    /// 三个入参就是全部事实：
    /// - `file_loaded`：`transcribe_file.is_some()`——已经导入了一支媒体；
    /// - `processing`：`ProcessStatus::Processing`——正在转写（单文件与批量都会置位）；
    /// - `subtitles_ready`：`!segments.is_empty() && selected_file == transcribe_file`——
    ///   这份字幕确实属于当前这支媒体；判据与质检卡片一致，刚换了还没转写的新片时
    ///   不会拿上一支的字幕充数。
    ///
    /// 抽出来的理由：旧写法有三处与真实状态对不上。
    /// 1. 第 1 步的条件是 `!processing && transcribe_file.is_some()`——转写一开始它反而
    ///    退回灰色，等于告诉用户「你没导入过文件」；
    /// 2. 第 2 步只在 `processing` 时点亮——转写跑完、字幕已经出来，它立刻熄灭，
    ///    看着像没跑成；
    /// 3. 第 3 步的条件是 `active_tab == Editor`，而本页只在 Generate 页渲染，该条件
    ///    恒为假——第 3 步永远灰着。
    ///
    /// 第 4 步（导出工程）没有任何状态源可用（`ProcessStatus` 里没有「已导出」），
    /// 因此恒为 `Upcoming`：宁可它一直暗着，也不假装知道用户导出没导出。
    fn of(file_loaded: bool, processing: bool, subtitles_ready: bool) -> StepChain {
        let import = if file_loaded {
            StepPhase::Done
        } else {
            StepPhase::Current
        };
        let transcribe = if processing {
            StepPhase::Current
        } else if subtitles_ready && file_loaded {
            StepPhase::Done
        } else if file_loaded {
            // 已导入、还没转写：下一步就是它。
            StepPhase::Current
        } else {
            StepPhase::Upcoming
        };
        // 第 3 步要求字幕已经在手边：正在转写时（哪怕是重转同一支）也还没轮到校对。
        let edit = if subtitles_ready && file_loaded && !processing {
            StepPhase::Current
        } else {
            StepPhase::Upcoming
        };
        StepChain {
            import,
            transcribe,
            edit,
            export: StepPhase::Upcoming,
        }
    }
}

/// 步骤条上的一步：序号圆点 + 文案。
///
/// 四步只有「序号 / 文案 / 强调色」不同，形状与配色必须同源——此前四份手写副本已经
/// 漂移成「第 2 步的高亮文案是薄荷色、第 3 步却是灰字」。
///
/// 三态配色：
/// - `Upcoming`：中性翻转底槽 + `text_primary` 数字（底槽是翻转的，数字必须跟着翻，
///   不能用恒白的 `text_on_saturated`——浅色主题下那是白字压浅底）；
/// - `Done` / `Current`：该步强调色的实心圆点。薄荷是浅底，配恒深的 `text_on_accent`；
///   蓝（`accent_blue`）是深底，必须配恒白的 `text_on_saturated`；
/// - `Current` 额外把文案也染成强调色并加粗，`Done` 的文案走 `text_primary`。
fn step_item(
    number: &'static str,
    label: &'static str,
    phase: StepPhase,
    accent: Rgba,
    on_accent: Rgba,
) -> Div {
    let (dot_bg, dot_fg, label_color, label_weight) = match phase {
        StepPhase::Upcoming => (
            Theme::bg_hover_strong(),
            Theme::text_primary(),
            Theme::text_secondary(),
            FontWeight::MEDIUM,
        ),
        StepPhase::Done => (accent, on_accent, Theme::text_primary(), FontWeight::MEDIUM),
        StepPhase::Current => (accent, on_accent, accent, FontWeight::BOLD),
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(Theme::STEP_DOT))
                .h(px(Theme::STEP_DOT))
                .rounded_full()
                .bg(dot_bg)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(Theme::TEXT_SMALL))
                .font_weight(FontWeight::BOLD)
                .text_color(dot_fg)
                .child(number),
        )
        .child(
            div()
                .text_size(px(Theme::TEXT_BODY))
                .font_weight(label_weight)
                .text_color(label_color)
                .child(label),
        )
}

/// 步骤条右侧状态药丸的形态（纯判定，便于单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPill {
    /// 空闲待命：没有媒体、也没有任务在跑
    Idle,
    /// 已导入、等转写：药丸显示文件名
    FileReady,
    /// 单文件转写中
    Transcribing,
    /// 批量转写中
    BatchTranscribing,
}

impl StatusPill {
    /// 进行中两态的徽标文案；`Idle` / `FileReady` 不走徽标（分别显示「就绪」与文件名）。
    ///
    /// 文案与底部状态栏的主按钮同词（「转写中… / 批量转写中…」）：同一件事不该在
    /// 顶部说一套、底部说另一套。
    fn transcribing_label(self) -> Option<&'static str> {
        match self {
            StatusPill::Transcribing => Some("转写中…"),
            StatusPill::BatchTranscribing => Some("批量转写中…"),
            StatusPill::Idle | StatusPill::FileReady => None,
        }
    }
}

/// 由真实状态推出状态药丸（纯判定，便于单测）。
///
/// `batch_running` 必须单独判：单文件转写与批量转写都会置 `is_processing`，只说
/// 「转写中」会让用户以为整批没跑起来（队列面板也是靠这个字段区分「终止批量」与
/// 「开始全部」的）。
fn transcribe_status_pill(
    is_processing: bool,
    batch_running: bool,
    file_loaded: bool,
) -> StatusPill {
    if is_processing {
        if batch_running {
            StatusPill::BatchTranscribing
        } else {
            StatusPill::Transcribing
        }
    } else if file_loaded {
        StatusPill::FileReady
    } else {
        StatusPill::Idle
    }
}

/// 质检胶囊的文案（纯函数，便于单测）。
///
/// 唯一的边界是低置信：`QualityReport::confidence_unavailable()` 表示**一句都没带置信度**，
/// 此时「低置信 0 句」是把「没测」说成了「通过」，改说「未评估」（导出复核表用的是
/// 同一条判据）。其余三类 0 句确实是「检查过、结果是 0」，照常保留计数。
fn quality_chip_label(
    category: QualityCategory,
    count: usize,
    confidence_unavailable: bool,
) -> String {
    if category == QualityCategory::LowConfidence && confidence_unavailable && count == 0 {
        return format!("{} 未评估", category.label());
    }
    format!("{} {count} 句", category.label())
}

/// 某一类质检问题有没有条目（纯判定，零分配）。
///
/// `QualityCategory::indices()` 会克隆整份下标列表再逐个去重，而这里只问「有没有」：
/// 去重只可能让列表变短、不会把非空变空，所以直接看分类对应的桶即可。
fn category_has_issues(report: &QualityReport, category: QualityCategory) -> bool {
    match category {
        QualityCategory::LowConfidence => !report.low_confidence.is_empty(),
        QualityCategory::GlossaryViolation => !report.glossary_violations.is_empty(),
        QualityCategory::Untranslated => !report.untranslated.is_empty(),
        QualityCategory::EmptyOrShort => !report.empty_or_short.is_empty(),
    }
}

// ==================== 「修复常见错误」快照缓存 ====================

/// 「修复常见错误」区块的现值：一次整表扫描的命中清单 + 相邻重复句分组。
#[derive(Debug, Clone, PartialEq)]
struct CleanupPreview {
    plan: CleanupPlan,
    groups: Vec<DuplicateGroup>,
}

/// 缓存键：`(segments_revision, cleanup_rules)`。
///
/// `segments_revision` 覆盖所有会改字幕的动作（编辑 / 拆分合并 / 应用修复 / 换工程 /
/// 载入缓存）；`cleanup_rules` 覆盖勾选变化——规则变了，命中清单就变了。
type CleanupPreviewKey = (u64, CleanupRules);

/// 转写页的跨帧缓存，存在 GPUI 全局里（`App::update_default_global`）。
///
/// 为什么不放进 `MainWindow`：它的结构体定义在 `ui/mod.rs`，而本轮改动只碰
/// `views/transcribe.rs`；GPUI 全局随 App 生命周期存活，等价于视图级状态
/// （与视频库缩略图状态的做法一致）。
#[derive(Default)]
struct TranscribeDerivedCache {
    cleanup: Option<(CleanupPreviewKey, Arc<CleanupPreview>)>,
}

impl Global for TranscribeDerivedCache {}

/// 缓存取数：键一致直接复用上次那个 `Arc`，否则现算并换入。
///
/// 抽成自由函数（不吃 `AppState`）是为了能在无 GUI 上下文的单测里钉死两条性质：
/// 「同键命中不重算、复用同一个 `Arc`」与「字幕版本或勾选规则变了必重算」。
fn cleanup_preview_cached(
    cache: &mut Option<(CleanupPreviewKey, Arc<CleanupPreview>)>,
    key: CleanupPreviewKey,
    build: impl FnOnce() -> CleanupPreview,
) -> Arc<CleanupPreview> {
    if let Some((cached_key, value)) = cache.as_ref() {
        if *cached_key == key {
            return Arc::clone(value);
        }
    }
    let value = Arc::new(build());
    *cache = Some((key, Arc::clone(&value)));
    value
}

#[cfg(test)]
mod tests {
    use super::{
        category_has_issues, cleanup_preview_cached, is_last_stream_row, quality_chip_label,
        queue_progress_view, transcribe_status_pill, truncate_for_ui, CleanupPreview,
        CleanupPreviewKey, QueueProgressView, StatusPill, StepChain, StepPhase,
    };
    use crate::app::state::{QueueItem, QueueState};
    use crate::subtitle::cleanup::{CleanupPlan, CleanupRule, CleanupRules};
    use crate::subtitle::segment::{QualityCategory, QualityReport};
    use crate::ui::split_mm_ss;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 回归（P1）：数据计数与实际渲染行数口径不一致时，流式看板不得 panic。
    /// 空窗口下「没有最后一行」是显式语义，而不是靠回绕后的比较侥幸恒假。
    #[test]
    fn streaming_dashboard_survives_count_without_segments() {
        // 窗口为空：任何行都不是最后一行，不发生回绕
        assert!(!is_last_stream_row(0, 0));
        assert!(!is_last_stream_row(7, 0));

        // 窗口非空：只有最后一行命中
        assert!(is_last_stream_row(0, 1));
        assert!(!is_last_stream_row(0, 2));
        assert!(is_last_stream_row(2, 3));
        assert!(!is_last_stream_row(1, 3));
    }

    /// 边界：秒 → `mm:ss` 的换算在负数 / `NaN` / `0` / 跨分钟 / 跨小时下
    /// 都不许 panic 或回绕（`as u32` 的饱和语义由 `split_mm_ss` 显式兜住）。
    #[test]
    fn streaming_dashboard_time_badge_survives_bad_durations() {
        assert_eq!(split_mm_ss(0.0), (0, 0));
        assert_eq!(split_mm_ss(-1.0), (0, 0));
        assert_eq!(split_mm_ss(f64::NAN), (0, 0));
        assert_eq!(split_mm_ss(f64::INFINITY), (0, 0));
        assert_eq!(split_mm_ss(59.9), (0, 59));
        assert_eq!(split_mm_ss(60.0), (1, 0));
        assert_eq!(split_mm_ss(61.0), (1, 1));
        assert_eq!(split_mm_ss(3661.0), (61, 1));
    }

    // ── 批量队列「本条进度」 ────────────────────────────────────────────

    /// 造一条队列项：直接落状态与进度，不碰磁盘。
    fn queue_item(state: QueueState, progress: f64) -> QueueItem {
        let mut item = QueueItem::new(PathBuf::from("a.mp4"));
        item.state = state;
        item.progress = progress;
        item
    }

    /// 「等待中」的条目不许显示进度：它还没开跑，一根 0% 的条只会让人以为
    /// 这一条卡住了。
    #[test]
    fn pending_item_shows_no_progress() {
        assert!(queue_progress_view(&queue_item(QueueState::Pending, 0.0)).is_none());
        assert!(
            queue_progress_view(&queue_item(QueueState::Pending, 0.5)).is_none(),
            "等待中的条目即便残留了进度值也不该显示"
        );
    }

    /// 转写中：条 + 百分比都要有；上一帧的残留值也不能画出越界的条。
    #[test]
    fn running_item_shows_matching_bar_and_percentage() {
        let view = queue_progress_view(&queue_item(QueueState::Running, 37.5)).expect("要画进度");

        // `ratio` 走布局引擎的 0.0~1.0，`percent` 是给用户看的 0~100，两者同源
        assert!((f64::from(view.ratio) - 0.375).abs() < 1e-6);
        assert_eq!(view.percent, 38, "37.5% 向上取整成 38，宁可显得多一点");
        assert!(view.show_bar, "没满格就必须画条");

        // 极端值：越界的百分比被夹回 [0,100]；满格（100）走另一条分支（见下）
        assert!(queue_progress_view(&queue_item(QueueState::Running, 500.0)).is_none());
        assert_eq!(
            queue_progress_view(&queue_item(QueueState::Running, -1.0))
                .unwrap()
                .percent,
            0
        );
    }

    /// 收尾规则：成功 = 满格，**不再画任何进度**；失败 = 停在出事那一刻，
    /// 百分比与进度条都留着（「跑到 62% 才炸」本身就是有用的信息）。
    #[test]
    fn settled_items_hide_full_progress_and_keep_failure_percent() {
        let done = queue_progress_view(&queue_item(QueueState::Done { segments: 3 }, 100.0));
        assert!(done.is_none(), "满格不再画条，连百分比也不必显示");

        let failed =
            queue_progress_view(&queue_item(QueueState::Failed("坏文件".to_string()), 62.0))
                .expect("失败条目要保留停在的百分比");
        assert_eq!(failed.percent, 62);
        assert!(failed.show_bar);
    }

    /// 渲染函数不许 panic，也不能自己造阶段名：阶段名一律原样透传。
    #[test]
    fn progress_row_renders_with_and_without_stage() {
        let view = QueueProgressView {
            ratio: 0.5,
            percent: 50,
            show_bar: true,
        };
        let _ = super::render_queue_progress(view, Some("语音识别"));
        let _ = super::render_queue_progress(
            QueueProgressView {
                ratio: 0.0,
                percent: 0,
                show_bar: true,
            },
            None,
        );
    }

    /// 修复样例的展示截断：压单行、短句原样、长句留头 24 字并补省略号。
    #[test]
    fn cleanup_sample_is_truncated_for_display() {
        assert_eq!(truncate_for_ui("  你好  \n世界  "), "你好 世界");
        assert_eq!(truncate_for_ui("短句"), "短句");
        let long = "一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十";
        let out = truncate_for_ui(long);
        assert_eq!(out.chars().count(), 25, "24 个字 + 省略号");
        assert!(out.ends_with('…'), "截断要有省略号，让用户知道还有后半句");
        assert!(long.starts_with(&out[..out.len() - '…'.len_utf8()]));
    }

    // ── 步骤条 / 状态药丸 ──────────────────────────────────────────────

    /// 回归：导入完成之后开始转写，第 1 步不许退回灰色。
    ///
    /// 旧条件是 `!is_processing && transcribe_file.is_some()`：按下「开始转写」的那一刻
    /// 第 1 步就暗掉，等于说用户没导入过文件。
    #[test]
    fn step_chain_keeps_import_done_while_transcribing() {
        let steps = StepChain::of(true, true, false);
        assert_eq!(steps.import, StepPhase::Done);
        assert_eq!(steps.transcribe, StepPhase::Current);
        assert_eq!(steps.edit, StepPhase::Upcoming);
        assert_eq!(steps.export, StepPhase::Upcoming);
    }

    /// 转写跑完（字幕已就绪）第 2 步是「完成」、第 3 步成为当前步。
    ///
    /// 旧写法第 2 步只在 `is_processing` 时亮，跑完立刻熄灭；第 3 步又挂在
    /// `active_tab == Editor` 上，而本页只在 Generate 页渲染，条件恒为假。
    #[test]
    fn step_chain_marks_transcribe_done_and_edit_current() {
        let steps = StepChain::of(true, false, true);
        assert_eq!(steps.import, StepPhase::Done);
        assert_eq!(steps.transcribe, StepPhase::Done);
        assert_eq!(steps.edit, StepPhase::Current);
    }

    /// 当前步永远指向「下一步该干什么」：没导入停在第 1 步，导入未转写停在第 2 步。
    #[test]
    fn step_chain_points_at_the_next_action() {
        let idle = StepChain::of(false, false, false);
        assert_eq!(idle.import, StepPhase::Current);
        assert_eq!(idle.transcribe, StepPhase::Upcoming);
        assert_eq!(idle.edit, StepPhase::Upcoming);

        // 字幕属于另一支片子时调用点传 `subtitles_ready = false`，落到这里：
        // 已导入、还没转写。
        let staged = StepChain::of(true, false, false);
        assert_eq!(staged.import, StepPhase::Done);
        assert_eq!(staged.transcribe, StepPhase::Current);
        assert_eq!(staged.edit, StepPhase::Upcoming);
    }

    /// 没有媒体却残留了字幕：链条不许自相矛盾（不能第 2 步「完成」而第 1 步还「当前」）。
    #[test]
    fn step_chain_stays_coherent_without_media() {
        let steps = StepChain::of(false, false, true);
        assert_eq!(steps.import, StepPhase::Current);
        assert_eq!(steps.transcribe, StepPhase::Upcoming);
        assert_eq!(steps.edit, StepPhase::Upcoming);
    }

    /// 状态药丸：批量与单文件转写必须说两种话（旧写法两者都只说「正在转写…」）。
    #[test]
    fn status_pill_separates_batch_from_single() {
        assert_eq!(
            transcribe_status_pill(true, false, true),
            StatusPill::Transcribing
        );
        assert_eq!(
            transcribe_status_pill(true, true, true),
            StatusPill::BatchTranscribing
        );
        assert_eq!(
            transcribe_status_pill(false, false, true),
            StatusPill::FileReady
        );
        assert_eq!(
            transcribe_status_pill(false, false, false),
            StatusPill::Idle
        );

        // 只有进行中两态走徽标
        assert_eq!(
            StatusPill::Transcribing.transcribing_label(),
            Some("转写中…")
        );
        assert_eq!(
            StatusPill::BatchTranscribing.transcribing_label(),
            Some("批量转写中…")
        );
        assert_eq!(StatusPill::FileReady.transcribing_label(), None);
        assert_eq!(StatusPill::Idle.transcribing_label(), None);
    }

    // ── 质检 / 修复 ────────────────────────────────────────────────────

    /// 低置信判据全无置信度数据时说「未评估」；其余三类 0 句照常保留计数。
    #[test]
    fn quality_chip_label_marks_missing_confidence_as_unavailable() {
        assert_eq!(
            quality_chip_label(QualityCategory::LowConfidence, 0, true),
            "低置信 未评估",
            "一句都没带置信度时，「0 句」把「没测」说成了「通过」"
        );
        assert_eq!(
            quality_chip_label(QualityCategory::LowConfidence, 0, false),
            "低置信 0 句",
            "有置信度数据、只是没命中：这才是「检查过，结果是 0」"
        );
        assert_eq!(
            quality_chip_label(QualityCategory::LowConfidence, 3, false),
            "低置信 3 句"
        );
        assert_eq!(
            quality_chip_label(QualityCategory::Untranslated, 0, true),
            "未翻译 0 句",
            "能力缺口只对低置信成立，别的类别不受影响"
        );
    }

    /// 分类判空只读分类对应的桶：同一句重复出现时不能因为去重而改变「有没有」的结论。
    #[test]
    fn category_has_issues_reads_buckets_without_cloning() {
        let report = QualityReport {
            low_confidence: vec![3],
            glossary_violations: Vec::new(),
            untranslated: vec![1, 1],
            empty_or_short: Vec::new(),
            confidence_scored: 2,
            confidence_missing: 0,
        };
        assert!(category_has_issues(&report, QualityCategory::LowConfidence));
        assert!(!category_has_issues(
            &report,
            QualityCategory::GlossaryViolation
        ));
        assert!(category_has_issues(&report, QualityCategory::Untranslated));
        assert!(!category_has_issues(&report, QualityCategory::EmptyOrShort));
    }

    /// 「修复常见错误」快照缓存的两条性质：同键命中不重算且复用同一个 `Arc`，
    /// 字幕版本或勾选规则一变必重算。
    #[test]
    fn cleanup_preview_cache_hits_same_key_and_rebuilds_on_change() {
        let rules = CleanupRules::default();
        let mut cache: Option<(CleanupPreviewKey, Arc<CleanupPreview>)> = None;
        let builds = std::cell::Cell::new(0u32);
        let make = |builds: &std::cell::Cell<u32>| {
            builds.set(builds.get() + 1);
            CleanupPreview {
                plan: CleanupPlan::default(),
                groups: Vec::new(),
            }
        };

        let first = cleanup_preview_cached(&mut cache, (7, rules), || make(&builds));
        let second = cleanup_preview_cached(&mut cache, (7, rules), || make(&builds));
        assert_eq!(builds.get(), 1, "同一个键第二次取数不许重算");
        assert!(
            Arc::ptr_eq(&first, &second),
            "命中要复用同一个 Arc，而不是重新包一份"
        );

        let third = cleanup_preview_cached(&mut cache, (8, rules), || make(&builds));
        assert_eq!(builds.get(), 2, "字幕版本变了必重算");
        assert!(!Arc::ptr_eq(&second, &third));

        let mut toggled = CleanupRules::default();
        toggled.toggle(CleanupRule::ALL[0]);
        assert_ne!(toggled, rules, "前提：这一下确实改了勾选");
        let fourth = cleanup_preview_cached(&mut cache, (8, toggled), || make(&builds));
        assert_eq!(builds.get(), 3, "规则变了命中清单就变了，必须重算");
        assert!(!Arc::ptr_eq(&third, &fourth));
    }
}
