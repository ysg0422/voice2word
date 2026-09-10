//! 性能基准指标详细回溯模态弹窗

use gpui::prelude::*;
use gpui::*;

use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::types::BenchmarkDialogInfo;
use super::super::MainWindow;

impl MainWindow {
    pub(crate) fn render_benchmark_dialog(&mut self, info: BenchmarkDialogInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let m = &info.metrics;
        let dur_str = format_duration_short(m.video_duration);

        div()
            .id("benchmark-dialog-backdrop")
            .absolute()
            .inset_0()
            .bg(rgba(0x000000cc))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("benchmark-dialog-card")
                    .w(px(520.0))
                    .p_7()
                    .rounded_2xl()
                    .bg(rgb(0x1a1a22))
                    .border_1()
                    .border_color(rgb(0x323242))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
                                    .child(
                                        div()
                                            .w(px(10.0))
                                            .h(px(10.0))
                                            .rounded_full()
                                            .bg(Theme::accent_mint()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(18.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .child("全流程性能统计与基准 (Benchmark)"),
                                    ),
                            )
                            .child(
                                div()
                                    .id("benchmark-modal-close-btn")
                                    .w(px(28.0))
                                    .h(px(28.0))
                                    .rounded_full()
                                    .bg(rgb(0x282834))
                                    .cursor_pointer()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(rgb(0x323244)).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.benchmark_dialog = None;
                                        cx.notify();
                                    }))
                                    .child("✕"),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(Theme::text_secondary())
                            .child(format!("任务视频：{} · 时长 {}", info.file_name, dur_str)),
                    )
                    // 核心指标面板
                    .child(
                        div()
                            .w_full()
                            .p_4()
                            .rounded_xl()
                            .bg(rgb(0x131318))
                            .border_1()
                            .border_color(rgb(0x282834))
                            .flex()
                            .flex_col()
                            .gap_2p5()
                            .child(Self::render_metric_row("1. FFmpeg 音频处理", m.ffmpeg_audio_sec, m.total_elapsed_sec, Theme::text_secondary()))
                            .child(Self::render_metric_row("2. Silero VAD 语音检测", m.vad_sec, m.total_elapsed_sec, Theme::accent_blue()))
                            .child(Self::render_metric_row("3. Whisper 核心转写", m.whisper_sec, m.total_elapsed_sec, Theme::accent_mint()))
                            .child(Self::render_metric_row(
                                format!("4. {}", m.polish_engine_name.as_deref().unwrap_or("标点/AI润色")),
                                m.qwen_sec,
                                m.total_elapsed_sec,
                                Theme::text_secondary(),
                            ))
                            .child(Self::render_metric_row("5. 字幕生成与写出", m.srt_export_sec, m.total_elapsed_sec, Theme::text_muted()))
                            .child(
                                div()
                                    .pt_2()
                                    .mt_1()
                                    .border_t_1()
                                    .border_color(rgb(0x242430))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .child("全流程总耗时"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(15.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("{:.1} 秒", m.total_elapsed_sec)),
                                    ),
                            ),
                    )
                    // 分析说明提示
                    .child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x181820))
                            .text_size(px(11.5))
                            .text_color(Theme::text_muted())
                            .child("提示：以上数据已作为基准线归档。可针对耗时占比最高的阶段进行定向优化（如提升 Whisper 线程、调整 VAD 参数或关闭 LLM 润色）。"),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .child(
                                div()
                                    .id("benchmark-dialog-confirm-btn")
                                    .px_5()
                                    .py_2()
                                    .rounded_full()
                                    .bg(Theme::accent_mint())
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(0x09090b))
                                    .hover(|s| s.opacity(0.9))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.benchmark_dialog = None;
                                        cx.notify();
                                    }))
                                    .child("知道了"),
                            ),
                    ),
            )
    }
}
