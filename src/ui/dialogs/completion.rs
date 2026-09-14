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
                    .w(px(580.0))
                    .p_6()
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
                    // ── 性能统计与成就看板 ──
                    .children(info.metrics.map(|m| {
                        let speedup = if m.total_elapsed_sec > 0.0 { info.total_duration / m.total_elapsed_sec } else { 1.0 };
                        let saved_mins = (info.total_duration - m.total_elapsed_sec).max(0.0) / 60.0;
                        div()
                            .w_full()
                            .p_4()
                            .rounded_xl()
                            .bg(rgb(0x131318))
                            .border_1()
                            .border_color(rgb(0x282834))
                            .flex()
                            .flex_col()
                            .gap_3()
                            // 顶部总耗时与加速比高光行
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .pb_2()
                                    .border_b_1()
                                    .border_color(rgb(0x22222c))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_size(px(14.0))
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(Theme::text_primary())
                                                    .child(format!("全链路耗时: {:.1} 秒", m.total_elapsed_sec)),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px_2p5()
                                            .py_1()
                                            .rounded_full()
                                            .bg(rgba(0x10b9811c))
                                            .border_1()
                                            .border_color(rgba(0x10b98138))
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("{:.1}x 极速加速比", speedup)),
                                    ),
                            )
                            // 3 宫格快速指标卡
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .p_2()
                                            .rounded_lg()
                                            .bg(rgb(0x181820))
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .child(div().text_size(px(9.5)).text_color(Theme::text_muted()).child("节省等待时间"))
                                            .child(div().text_size(px(13.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("约 {:.1} 分钟", saved_mins))),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .p_2()
                                            .rounded_lg()
                                            .bg(rgb(0x181820))
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .child(div().text_size(px(9.5)).text_color(Theme::text_muted()).child("音频传输管道"))
                                            .child(div().text_size(px(13.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child("纯内存 0 I/O")),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .p_2()
                                            .rounded_lg()
                                            .bg(rgb(0x181820))
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .child(div().text_size(px(9.5)).text_color(Theme::text_muted()).child("字幕规范"))
                                            .child(div().text_size(px(13.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child("高精标点")),
                                    ),
                            )
                            // 分阶段耗时明细
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .pt_1()
                                    .child(Self::render_metric_row(
                                        m.audio_process_name.clone().unwrap_or_else(|| "音频处理".to_string()),
                                        m.ffmpeg_audio_sec,
                                        m.total_elapsed_sec,
                                        Theme::text_secondary(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.vad_engine_name.clone().unwrap_or_else(|| "VAD 语音切片".to_string()),
                                        m.vad_sec,
                                        m.total_elapsed_sec,
                                        Theme::accent_blue(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.asr_engine_name.clone().unwrap_or_else(|| "核心转写".to_string()),
                                        m.whisper_sec,
                                        m.total_elapsed_sec,
                                        Theme::accent_mint(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.polish_engine_name.clone().unwrap_or_else(|| "标点与润色".to_string()),
                                        m.qwen_sec,
                                        m.total_elapsed_sec,
                                        Theme::text_secondary(),
                                    ))
                                    .child(Self::render_metric_row(
                                        m.export_name.clone().unwrap_or_else(|| "字幕写出".to_string()),
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
                            .gap_2p5()
                            .mt_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .mt_2()
                                    .child(
                                        div()
                                            .id("modal-goto-editor-btn")
                                            .px_8()
                                            .py_2p5()
                                            .rounded_xl()
                                            .bg(Theme::accent_mint())
                                            .cursor_pointer()
                                            .text_size(px(13.5))
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
                                    ),
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
                            .child(time_str),
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
