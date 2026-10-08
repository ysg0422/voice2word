//! 转写完成模态提示弹窗与性能基准指标展示

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::{BatchSummary, CompletionDialogInfo};
use super::super::MainWindow;
use crate::app::state::WorkspaceTab;
use crate::utils::time::format_duration_short;

impl MainWindow {
    /// 渲染全屏转写完成提醒弹窗 (包含 5 阶段耗时统计、直接进入剪辑、导出字幕或继续导入下一个)
    pub(crate) fn render_completion_dialog(
        &mut self,
        info: CompletionDialogInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        primitives::modal_scrim()
            .id("completion-dialog-backdrop")
            .child(
                // 面板外壳走 modal_card：圆角 / 描边 / 纵向排布与全站卡片同源，
                // 弹窗只是底色再抬一档（bg_raised）并加投影浮起。
                primitives::modal_card(Theme::DIALOG_W_WIDE, Theme::PAGE_PAD)
                    .id("completion-dialog-card")
                    .items_center()
                    .gap(px(Theme::SPACE_4))
                    .child(
                        // 顶部对勾状态符
                        div()
                            .w(px(Theme::ICON_BOX_LG))
                            .h(px(Theme::ICON_BOX_LG))
                            .rounded_full()
                            .bg(Theme::tint_mint_soft())
                            .border_1()
                            .border_color(Theme::tint_mint_border())
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_DISPLAY))
                            .text_color(Theme::accent_mint())
                            .child("✓"),
                    )
                    .child(primitives::page_title(if info.batch.is_some() {
                        "批量转写完成"
                    } else {
                        "转写完成"
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(Theme::SPACE_1))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_TITLE))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(info.file_name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(Theme::text_secondary())
                                    .child(if let Some(ref b) = info.batch {
                                        format!(
                                            "{} 个文件 · {} 成功 / {} 失败 · 共 {} 句字幕",
                                            b.total, b.done, b.failed, b.segments
                                        )
                                    } else {
                                        format!(
                                            "共 {} 句字幕 · 视频时长 {}",
                                            info.segment_count,
                                            format_duration_short(info.total_duration)
                                        )
                                    }),
                            ),
                    )
                    // ── 批量汇总看板（批量模式）/ 单文件性能统计看板 ──
                    .children(
                        info.batch
                            .clone()
                            .map(|summary| Self::render_batch_summary(summary).into_any_element()),
                    )
                    .children(info.metrics.map(|m| {
                        let speedup = if m.total_elapsed_sec > 0.0 {
                            info.total_duration / m.total_elapsed_sec
                        } else {
                            1.0
                        };
                        let saved_mins =
                            (info.total_duration - m.total_elapsed_sec).max(0.0) / 60.0;
                        primitives::card_with_pad(Theme::SPACE_4)
                            .w_full()
                            .bg(Theme::bg_sidebar())
                            .border_color(Theme::border_mid())
                            .gap(px(Theme::SPACE_3))
                            // 顶部总耗时与加速比高光行
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .pb(px(Theme::SPACE_2))
                                    .border_b_1()
                                    .border_color(Theme::border_subtle())
                                    .child(
                                        div().flex().items_center().gap(px(Theme::SPACE_2)).child(
                                            div()
                                                .text_size(px(Theme::TEXT_TITLE))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_primary())
                                                .child(format!(
                                                    "全链路耗时: {:.1} 秒",
                                                    m.total_elapsed_sec
                                                )),
                                        ),
                                    )
                                    .child(primitives::badge_accent(format!(
                                        "{:.1}x 极速加速比",
                                        speedup
                                    ))),
                            )
                            // 4 宫格快速指标卡
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::SPACE_2))
                                    .child(Self::render_batch_stat(
                                        "有声 / 总时长",
                                        format!(
                                            "{} / {}",
                                            format_duration_short(m.voiced_duration_sec),
                                            format_duration_short(m.video_duration),
                                        ),
                                        Theme::accent_blue(),
                                    ))
                                    .child(Self::render_batch_stat(
                                        "节省等待时间",
                                        format!("约 {:.1} 分钟", saved_mins),
                                        Theme::accent_mint(),
                                    ))
                                    .child(Self::render_batch_stat(
                                        "音频传输管道",
                                        "纯内存 0 I/O".to_string(),
                                        Theme::accent_blue(),
                                    ))
                                    .child(Self::render_batch_stat(
                                        "字幕规范",
                                        "高精标点".to_string(),
                                        Theme::text_primary(),
                                    )),
                            )
                            // 分阶段耗时明细
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(Theme::SPACE_1_5))
                                    .pt(px(Theme::SPACE_1))
                                    .child(Self::render_metric_row(
                                        m.audio_process_name
                                            .clone()
                                            .unwrap_or_else(|| "音频处理".to_string()),
                                        m.ffmpeg_audio_sec,
                                        m.total_elapsed_sec,
                                        Theme::text_secondary(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.vad_engine_name
                                            .clone()
                                            .unwrap_or_else(|| "VAD 语音切片".to_string()),
                                        m.vad_sec,
                                        m.total_elapsed_sec,
                                        Theme::accent_blue(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.asr_engine_name
                                            .clone()
                                            .unwrap_or_else(|| "核心转写".to_string()),
                                        m.whisper_sec,
                                        m.total_elapsed_sec,
                                        Theme::accent_mint(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.polish_engine_name
                                            .clone()
                                            .unwrap_or_else(|| "标点与润色".to_string()),
                                        m.qwen_sec,
                                        m.total_elapsed_sec,
                                        Theme::text_secondary(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.export_name
                                            .clone()
                                            .unwrap_or_else(|| "字幕写出".to_string()),
                                        m.srt_export_sec,
                                        m.total_elapsed_sec,
                                        Theme::text_muted(),
                                    )),
                            )
                    }))
                    // 操作按钮组
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(Theme::SPACE_2))
                            .mt(px(Theme::SPACE_2))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .gap(px(Theme::SPACE_3))
                                    .mt(px(Theme::SPACE_2))
                                    .child(
                                        primitives::btn_clickable(
                                            "进入剪辑校对",
                                            primitives::BtnSize::Lg,
                                            primitives::BtnVariant::Primary,
                                        )
                                        .id("modal-goto-editor-btn")
                                        .px(px(Theme::SPACE_8))
                                        .rounded(px(Theme::RADIUS_XL))
                                        .font_weight(FontWeight::BOLD)
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.completion_dialog = None;
                                                this.state.active_tab = WorkspaceTab::Editor;
                                                this.trigger_extract_frame(cx);
                                                this.ensure_preview_proxy(cx);
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .children(if info.batch.is_some() {
                                        Some(
                                            primitives::btn_clickable(
                                                "关闭并清空队列",
                                                primitives::BtnSize::Lg,
                                                primitives::BtnVariant::Secondary,
                                            )
                                            .id("modal-clear-batch-btn")
                                            .rounded(px(Theme::RADIUS_XL))
                                            .on_click(
                                                cx.listener(|this, _, _, cx| {
                                                    this.completion_dialog = None;
                                                    this.state.clear_batch_queue();
                                                    cx.notify();
                                                }),
                                            ),
                                        )
                                    } else {
                                        None
                                    }),
                            ),
                    ),
            )
    }

    /// 批量完成看板：替代单文件性能指标，把「这一批到底成了几个、败了几个」讲清楚
    pub(crate) fn render_batch_summary(summary: BatchSummary) -> impl IntoElement {
        let BatchSummary {
            total,
            done,
            failed,
            segments,
            total_duration,
            failed_names,
        } = summary;
        let success_rate = if total > 0 {
            done as f64 / total as f64 * 100.0
        } else {
            0.0
        };

        primitives::card_with_pad(Theme::SPACE_4)
            .w_full()
            .bg(Theme::bg_sidebar())
            .border_color(Theme::border_mid())
            .gap(px(Theme::SPACE_3))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb(px(Theme::SPACE_2))
                    .border_b_1()
                    .border_color(Theme::border_subtle())
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_TITLE))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(format!("累计产出 {segments} 句字幕")),
                    )
                    .child(primitives::badge_accent(format!(
                        "成功率 {success_rate:.0}%"
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(Self::render_batch_stat(
                        "文件总数",
                        format!("{total}"),
                        Theme::text_primary(),
                    ))
                    .child(Self::render_batch_stat(
                        "已完成",
                        format!("{done}"),
                        Theme::accent_mint(),
                    ))
                    .child(Self::render_batch_stat(
                        "失败",
                        format!("{failed}"),
                        if failed > 0 {
                            Theme::accent_red()
                        } else {
                            Theme::text_muted()
                        },
                    ))
                    .child(Self::render_batch_stat(
                        "累计时长",
                        format_duration_short(total_duration),
                        Theme::accent_blue(),
                    )),
            )
            .children(if failed_names.is_empty() {
                None
            } else {
                Some(
                    primitives::card_with_pad(Theme::SPACE_2)
                        .w_full()
                        .rounded(px(Theme::RADIUS_LG))
                        .bg(Theme::tint_red_soft())
                        .border_color(Theme::tint_red_border())
                        .gap(px(Theme::SPACE_1))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::accent_red())
                                .child("以下文件未成功，可在队列里再点「开始全部」重试："),
                        )
                        .children(failed_names.into_iter().take(6).map(|name| {
                            div()
                                .text_size(px(Theme::TEXT_SMALL))
                                .text_color(Theme::text_secondary())
                                .child(name)
                        })),
                )
            })
    }

    fn render_batch_stat(
        label: &'static str,
        value: String,
        color: gpui::Rgba,
    ) -> impl IntoElement {
        // 宫格统计块：card_sm 提供内边距与圆角，这里只把内容居中并改用凹槽底色
        primitives::card_sm()
            .flex_1()
            .bg(Theme::bg_inset())
            .border_color(Theme::border_mid())
            .items_center()
            // 标签与数值紧贴（card_sm 默认 12px 行距对 8px 内边距的小宫格太松）
            .gap(px(Theme::SPACE_1))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_BODY_LG))
                    .font_weight(FontWeight::BOLD)
                    .text_color(color)
                    .child(value),
            )
    }

    pub(crate) fn render_metric_row(
        label: impl Into<SharedString>,
        sec: f64,
        total: f64,
        dot_color: gpui::Rgba,
    ) -> impl IntoElement {
        let label_str: SharedString = label.into();
        let pct = if total > 0.0 {
            (sec / total * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        let time_str = if sec <= 0.00001 {
            "0 ms".to_string()
        } else if sec < 0.001 {
            "< 1 ms".to_string()
        } else if sec < 0.1 {
            format!("{:.0} ms", (sec * 1000.0).round())
        } else {
            format!("{:.1} 秒", sec)
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(Theme::TEXT_BODY))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(primitives::stat_dot_sm(dot_color))
                    .child(div().text_color(Theme::text_secondary()).child(label_str)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(Theme::SPACE_2))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child(time_str),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(format!("({:.0}%)", pct)),
                    ),
            )
    }
}
