//! 性能基准指标详细回溯模态弹窗

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::types::BenchmarkDialogInfo;
use super::super::MainWindow;
use crate::utils::time::format_duration_short;

impl MainWindow {
    pub(crate) fn render_benchmark_dialog(
        &mut self,
        info: BenchmarkDialogInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let m = &info.metrics;
        let dur_str = format_duration_short(m.video_duration);

        primitives::modal_scrim()
            .id("benchmark-dialog-backdrop")
            .child(
                // 面板外壳统一走 modal_card：圆角 / 描边 / 纵向排布与全站卡片同源，
                // 弹窗只是底色再抬一档（bg_raised）并加投影浮起。
                primitives::modal_card(Theme::DIALOG_W, Theme::PAGE_PAD)
                    .id("benchmark-dialog-card")
                    .gap(px(Theme::SPACE_4))
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
                                    .child(primitives::stat_dot(Theme::accent_mint()))
                                    .child(primitives::page_title(
                                        "全流程性能统计与基准 (Benchmark)",
                                    )),
                            )
                            .child(
                                // 圆形关闭键：走 icon_close_btn 原语，与完成弹窗同源
                                primitives::icon_close_btn()
                                    .id("benchmark-modal-close-btn")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.benchmark_dialog = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY_LG))
                            .text_color(Theme::text_secondary())
                            .child(format!("任务视频：{} · 时长 {}", info.file_name, dur_str)),
                    )
                    // 核心指标面板
                    .child(
                        primitives::card_with_pad(Theme::SPACE_4)
                            .w_full()
                            .bg(Theme::bg_sidebar())
                            .border_color(Theme::border_mid())
                            .gap(px(Theme::SPACE_2))
                            .child(Self::render_metric_row(
                                &format!(
                                    "1. {}",
                                    m.audio_process_name.as_deref().unwrap_or("FFmpeg 音频处理")
                                ),
                                m.ffmpeg_audio_sec,
                                m.total_elapsed_sec,
                                Theme::text_secondary(),
                            ))
                            .child(Self::render_metric_row(
                                &format!(
                                    "2. {}",
                                    m.vad_engine_name
                                        .as_deref()
                                        .unwrap_or("Silero VAD 语音检测")
                                ),
                                m.vad_sec,
                                m.total_elapsed_sec,
                                Theme::accent_blue(),
                            ))
                            .child(Self::render_metric_row(
                                &format!(
                                    "3. {}",
                                    m.asr_engine_name.as_deref().unwrap_or("Whisper 核心转写")
                                ),
                                m.whisper_sec,
                                m.total_elapsed_sec,
                                Theme::accent_mint(),
                            ))
                            .child(Self::render_metric_row(
                                format!(
                                    "4. {}",
                                    m.polish_engine_name.as_deref().unwrap_or("标点/AI润色")
                                ),
                                m.qwen_sec,
                                m.total_elapsed_sec,
                                Theme::text_secondary(),
                            ))
                            .child(Self::render_metric_row(
                                &format!(
                                    "5. {}",
                                    m.export_name.as_deref().unwrap_or("字幕生成与写出")
                                ),
                                m.srt_export_sec,
                                m.total_elapsed_sec,
                                Theme::text_muted(),
                            ))
                            .child(
                                div()
                                    .pt(px(Theme::SPACE_2))
                                    .mt(px(Theme::SPACE_1))
                                    .border_t_1()
                                    .border_color(Theme::border_subtle())
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY_LG))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .child("全流程总耗时"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_TITLE))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(if m.total_elapsed_sec <= 0.00001 {
                                                "0 ms".to_string()
                                            } else if m.total_elapsed_sec < 0.001 {
                                                "< 1 ms".to_string()
                                            } else if m.total_elapsed_sec < 0.1 {
                                                format!(
                                                    "{:.0} ms",
                                                    (m.total_elapsed_sec * 1000.0).round()
                                                )
                                            } else {
                                                format!("{:.1} 秒", m.total_elapsed_sec)
                                            }),
                                    ),
                            ),
                    )
                    .child(
                        div().flex().justify_end().child(
                            primitives::btn_clickable(
                                "知道了",
                                primitives::BtnSize::Md,
                                primitives::BtnVariant::Primary,
                            )
                            .id("benchmark-dialog-confirm-btn")
                            .px(px(Theme::SPACE_5))
                            .rounded_full()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.benchmark_dialog = None;
                                cx.notify();
                            })),
                        ),
                    ),
            )
    }
}
