//! 转写完成模态提示弹窗与性能基准指标展示

use gpui::prelude::*;
use gpui::*;

use crate::app::state::WorkspaceTab;
use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::types::CompletionDialogInfo;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染全屏转写完成提醒弹窗 (包含 5 阶段耗时统计、直接进入剪辑、导出字幕或继续导入下一个)
    pub(crate) fn render_completion_dialog(&mut self, info: CompletionDialogInfo, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("completion-dialog-backdrop")
            .absolute()
            .inset_0()
            .bg(rgba(0x000000cc))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("completion-dialog-card")
                    .w(px(500.0))
                    .p_7()
                    .rounded_2xl()
                    .bg(rgb(0x1a1a22))
                    .border_1()
                    .border_color(rgb(0x323242))
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_4()
                    .child(
                        // 顶部对勾状态符
                        div()
                            .w(px(52.0))
                            .h(px(52.0))
                            .rounded_full()
                            .bg(rgba(0x10b9811f))
                            .border_1()
                            .border_color(rgba(0x10b9813f))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(22.0))
                            .text_color(Theme::accent_mint())
                            .child("✓"),
                    )
                    .child(
                        div()
                            .text_size(px(19.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("转写完成"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(info.file_name),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .child(format!(
                                        "共 {} 句字幕 · 视频时长 {}",
                                        info.segment_count,
                                        format_duration_short(info.total_duration)
                                    )),
                            ),
                    )
                    // ── 性能统计 5 阶段基准看板 ──
                    .children(info.metrics.map(|m| {
                        div()
                            .w_full()
                            .p_3p5()
                            .rounded_xl()
                            .bg(rgb(0x131318))
                            .border_1()
                            .border_color(rgb(0x282834))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .pb_1p5()
                                    .border_b_1()
                                    .border_color(rgb(0x22222c))
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("阶段耗时基准分析 (Baseline)"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.5))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("总耗时: {:.1} 秒", m.total_elapsed_sec)),
                                    ),
                            )
                            .child(Self::render_metric_row("FFmpeg 音频处理", m.ffmpeg_audio_sec, m.total_elapsed_sec, Theme::text_secondary()))
                            .child(Self::render_metric_row("VAD 语音检测", m.vad_sec, m.total_elapsed_sec, Theme::accent_blue()))
                            .child(Self::render_metric_row("Whisper 核心转写", m.whisper_sec, m.total_elapsed_sec, Theme::accent_mint()))
                            .child(Self::render_metric_row(
                                m.polish_engine_name.clone().unwrap_or_else(|| "标点/AI润色".to_string()),
                                m.qwen_sec,
                                m.total_elapsed_sec,
                                Theme::text_secondary(),
                            ))
                            .child(Self::render_metric_row("字幕导出写出", m.srt_export_sec, m.total_elapsed_sec, Theme::text_muted()))
                    }))
                    // 操作按钮组
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap_3()
                            .mt_2()
                            .child(
                                div()
                                    .id("modal-goto-editor-btn")
                                    .px_5()
                                    .py_2()
                                    .rounded_full()
                                    .bg(Theme::accent_mint())
                                    .cursor_pointer()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0x09090b))
                                    .hover(|s| s.opacity(0.9))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.completion_dialog = None;
                                        this.state.active_tab = WorkspaceTab::Editor;
                                        this.trigger_extract_frame(cx);
                                        this.ensure_preview_proxy(cx);
                                        cx.notify();
                                    }))
                                    .child("进入剪辑校对"),
                            )
                            .child(
                                div()
                                    .id("modal-export-btn")
                                    .px_4()
                                    .py_2()
                                    .rounded_full()
                                    .bg(Theme::bg_card())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .cursor_pointer()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .hover(|s| s.bg(Theme::bg_hover()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.export_subtitles(cx);
                                    }))
                                    .child("导出字幕"),
                            )
                            .child(
                                div()
                                    .id("modal-next-video-btn")
                                    .px_4()
                                    .py_2()
                                    .rounded_full()
                                    .bg(rgb(0x282834))
                                    .border_1()
                                    .border_color(rgb(0x3a3a4c))
                                    .cursor_pointer()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .hover(|s| s.bg(rgb(0x323242)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.completion_dialog = None;
                                        this.state.active_tab = WorkspaceTab::Generate;
                                        this.choose_file(cx);
                                    }))
                                    .child("导入下一个视频"),
                            ),
                    ),
            )
    }

    pub(crate) fn render_metric_row(
        label: impl Into<SharedString>,
        sec: f64,
        total: f64,
        dot_color: gpui::Rgba,
    ) -> impl IntoElement {
        let label_str: SharedString = label.into();
        let pct = if total > 0.0 { (sec / total * 100.0).clamp(0.0, 100.0) } else { 0.0 };
        div()
            .flex()
            .items_center()
            .justify_between()
            .text_size(px(11.5))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(5.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(dot_color),
                    )
                    .child(div().text_color(Theme::text_secondary()).child(label_str)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child(format!("{:.1} 秒", sec)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(Theme::text_muted())
                            .child(format!("({:.0}%)", pct)),
                    ),
            )
    }
}
