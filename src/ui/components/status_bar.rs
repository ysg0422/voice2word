//! 底部状态与操作栏部件

use gpui::prelude::*;
use gpui::*;

use crate::app::state::ProcessStatus;
use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染底部状态与操作栏 (iOS Minimal Toolbar 规范)
    pub(crate) fn render_bottom_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        let has_batch = !self.state.batch_queue.is_empty();
        let can_start = (has_file || has_batch) && !is_processing;
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
            .w_full()
            .h(px(Theme::STATUS_BAR_H))
            .flex_shrink_0()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .px(px(Theme::PAGE_PAD))
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
                                                .text_size(px(Theme::TEXT_BODY))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(Theme::text_primary())
                                                .child(stage_text.to_string()),
                                        )
                                        .child(
                                            if total_dur > 0.0 {
                                                div()
                                                    .text_size(px(Theme::TEXT_SMALL))
                                                    .text_color(Theme::text_secondary())
                                                    .child(format!("{cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02}"))
                                            } else {
                                                div()
                                            }
                                        )
                                        .child(
                                            primitives::badge_accent(format!(
                                                "{:.1}%",
                                                display_ratio * 100.0
                                            )),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(Theme::PROGRESS_W))
                                        .child(primitives::progress_bar(display_ratio as f32)),
                                )
                        } else if self.state.transcribe_file.is_some() {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(primitives::stat_dot_sm(Theme::accent_blue()))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_BODY))
                                        .text_color(Theme::text_secondary())
                                        .child("文件已就绪"),
                                )
                        } else if has_batch {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(primitives::stat_dot_sm(Theme::accent_mint()))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_BODY))
                                        .text_color(Theme::text_secondary())
                                        .child(format!(
                                            "批量队列已就绪 · {} 个待处理",
                                            self.state.queue_pending_count()
                                        )),
                                )
                        } else {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(primitives::stat_dot_sm(Theme::bg_dot_idle()))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_BODY))
                                        .text_color(Theme::text_muted())
                                        .child("就绪"),
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
                        // 「播放预览」：禁用态只置灰不接交互，形状不变
                        primitives::pill_btn_outline_state("播放预览", can_play)
                            .id("play-video-btn")
                            .when(can_play, |d| {
                                d.on_click(cx.listener(|this, _, _, cx| this.play_video(cx)))
                            }),
                    )
                    .child(
                        // 「开始处理 / 开始全部」：实心薄荷胶囊，禁用时退化为中性底槽
                        primitives::pill_btn_solid_state(
                            if is_processing {
                                if self.state.batch_running { "批量处理中..." } else { "处理中..." }
                            } else if has_file {
                                "开始处理"
                            } else if has_batch {
                                "开始全部"
                            } else {
                                "开始处理"
                            },
                            Theme::accent_mint(),
                            can_start,
                        )
                        .id("start-pipeline-btn")
                        .when(can_start, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                if this.state.transcribe_file.is_some() {
                                    this.start_processing(cx);
                                } else {
                                    this.start_batch_queue(cx);
                                }
                            }))
                        }),
                    ),
            )
    }
}
