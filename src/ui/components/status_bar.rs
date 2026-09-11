//! 底部状态与操作栏部件

use gpui::prelude::*;
use gpui::*;

use crate::app::state::ProcessStatus;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染底部状态与操作栏 (iOS Minimal Toolbar 规范)
    pub(crate) fn render_bottom_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let can_start = self.state.transcribe_file.is_some() && !is_processing;
        let can_export = !self.state.segments.is_empty();
        let can_play = self.state.selected_file.is_some() && can_export && !is_processing;

        let (stage_text, progress_val, _detail_text) = match &self.state.status {
            ProcessStatus::Idle => ("就绪", 0.0, String::new()),
            ProcessStatus::Processing { stage, progress, detail } => {
                (stage.as_str(), *progress, detail.clone())
            }
            ProcessStatus::Completed => ("转写完成", 1.0, String::new()),
            ProcessStatus::Failed(e) => ("出错", 0.0, e.clone()),
        };

        div()
            .h(px(64.0))
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .px_6()
            .flex()
            .items_center()
            .justify_between()
            // 左侧状态指示
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        if is_processing {
                            let total_dur = self.state.transcribe_duration;
                            let cur_sec = self.state.streaming_current_sec;
                            let cur_mm = (cur_sec / 60.0) as u32;
                            let cur_ss = (cur_sec % 60.0) as u32;
                            let tot_mm = (total_dur / 60.0) as u32;
                            let tot_ss = (total_dur % 60.0) as u32;
                            let display_ratio = if total_dur > 0.0 && cur_sec > 0.0 {
                                (cur_sec / total_dur).clamp(0.0, 1.0)
                            } else {
                                progress_val.clamp(0.0, 1.0)
                            };

                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(Theme::text_primary())
                                                .child(stage_text.to_string()),
                                        )
                                        .child(
                                            if total_dur > 0.0 {
                                                div()
                                                    .text_size(px(11.0))
                                                    .text_color(Theme::text_secondary())
                                                    .child(format!("{cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02}"))
                                            } else {
                                                div()
                                            }
                                        )
                                        .child(
                                            div()
                                                .px_2()
                                                .py_0p5()
                                                .rounded_full()
                                                .bg(rgba(0x10b98120))
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::accent_mint())
                                                .child(format!("{:.1}%", display_ratio * 100.0)),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(260.0))
                                        .h(px(4.0))
                                        .rounded_full()
                                        .bg(rgb(0x22222a))
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .h_full()
                                                .w(relative(display_ratio as f32))
                                                .rounded_full()
                                                .bg(Theme::accent_mint()),
                                        ),
                                )
                        } else if self.state.transcribe_file.is_some() {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(7.0))
                                        .h(px(7.0))
                                        .rounded_full()
                                        .bg(Theme::accent_blue()),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(Theme::text_secondary())
                                        .child("新视频已就绪，点击右下角「开始处理」"),
                                )
                        } else {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(7.0))
                                        .h(px(7.0))
                                        .rounded_full()
                                        .bg(rgb(0x3a3a44)),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(Theme::text_muted())
                                        .child("Voice2Word 智能转写引擎就绪 · 等待导入新视频"),
                                )
                        }
                    )
            )
            // 右侧核心操作按钮 (iOS 椭圆胶囊 Pill Buttons)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .id("play-video-btn")
                            .px_4()
                            .py_1p5()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(if can_play { Theme::bg_card() } else { rgb(0x1a1a22) })
                            .text_color(if can_play { Theme::text_primary() } else { Theme::text_muted() })
                            .border_1()
                            .border_color(Theme::border())
                            .text_size(px(12.0))
                            .hover(|s| s.bg(Theme::bg_hover()))
                            .on_click(cx.listener(|this, _, _, cx| this.play_video(cx)))
                            .child("播放预览"),
                    )
                    .child(
                        div()
                            .id("export-subtitles-btn")
                            .px_4()
                            .py_1p5()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(if can_export { Theme::bg_card() } else { rgb(0x1a1a22) })
                            .text_color(if can_export { Theme::text_primary() } else { Theme::text_muted() })
                            .border_1()
                            .border_color(Theme::border())
                            .text_size(px(12.0))
                            .hover(|s| s.bg(Theme::bg_hover()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_subtitles(cx);
                            }))
                            .child("导出字幕"),
                    )
                    .child(
                        div()
                            .id("start-pipeline-btn")
                            .px_5()
                            .py_1p5()
                            .rounded_full()
                            .cursor_pointer()
                            .bg(if can_start {
                                Theme::accent_mint()
                            } else {
                                rgb(0x282832)
                            })
                            .text_color(if can_start {
                                rgb(0x09090b)
                            } else {
                                Theme::text_muted()
                            })
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_processing(cx);
                            }))
                            .child(if is_processing {
                                "处理中..."
                            } else {
                                "开始处理"
                            }),
                    ),
            )
    }
}
