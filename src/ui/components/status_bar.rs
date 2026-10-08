//! 底部状态与操作栏部件

use gpui::prelude::*;
use gpui::*;

use super::super::primitives;
use super::super::theme::Theme;
use super::super::MainWindow;
use crate::app::state::ProcessStatus;

impl MainWindow {
    /// 全局失败提示条。
    ///
    /// `ProcessStatus::Failed` 由 16 处产生（导出失败、预览启动失败、剪映注入失败、
    /// 未识别出字幕……），但**底部状态栏只挂在「语音转写」页**（`views/transcribe.rs`）。
    /// 用户在剪辑台点「导出」失败、在视频库删记录失败时，错误串确实写进了
    /// `state.status`，却没有任何控件能显示它——界面一片安静，用户只会认为按钮坏了。
    ///
    /// 这里把失败态提升为一条**跨页常驻**的错误条，挂在标题栏正下方。点「关闭」
    /// 把状态复位为 `Idle`（与「文件已就绪」等常规态一致），错误串随之消失。
    /// 只渲染 `Failed` 一种，不影响其余状态在转写页的正常展示。
    /// 字幕写库失败横幅。
    ///
    /// 单列一条而不是塞进 `state.status`：那个状态表示「转写失败了」，
    /// 与「改动没能存盘」是两件互不相干的事——转写成功、但编辑保存失败，
    /// 是完全可能的组合。而且这条要**常驻可见**直到用户处理，不能因为
    /// 一次转写状态变化就被覆盖掉。
    pub(crate) fn render_db_error_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.state.db_write_error.clone() else {
            return div().into_any_element();
        };

        div()
            .id("db-error-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_red_soft())
            .border_b_1()
            .border_color(Theme::tint_red_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_red()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_red())
                    // 说明后果，而不只是报错：用户需要知道「现在关程序会丢东西」
                    .child(format!(
                        "{msg}（改动尚未写入历史库，请检查磁盘空间或文件权限）"
                    )),
            )
            .child(
                div()
                    .id("db-error-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py_0p5()
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::accent_red())
                    .hover(|s| s.bg(Theme::tint_red_border()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.db_write_error = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    pub(crate) fn render_error_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let ProcessStatus::Failed(msg) = &self.state.status else {
            return div().into_any_element();
        };

        div()
            .id("global-error-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::tint_red_soft())
            .border_b_1()
            .border_color(Theme::tint_red_border())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_red()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_red())
                    .child(msg.clone()),
            )
            .child(
                div()
                    .id("global-error-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py_0p5()
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::accent_red())
                    .hover(|s| s.bg(Theme::tint_red_border()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.status = ProcessStatus::Idle;
                        cx.notify();
                    }))
                    .child("关闭"),
            )
            .into_any_element()
    }

    /// 一次性中性提示条（成功类反馈）。
    ///
    /// 与 [`Self::render_error_banner`] 分开：那条是「出错了」（红），这条是
    /// 「操作完成了，但结果需要告知」（中性色）。典型场景是视频库删掉记录后
    /// 说明「当前工程被换成了哪一条」——操作本身成功，不该报成错误。
    pub(crate) fn render_notice_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(msg) = self.notice.clone() else {
            return div().into_any_element();
        };

        div()
            .id("global-notice-banner")
            .w_full()
            .flex_shrink_0()
            .px(px(Theme::PAGE_PAD))
            .py(px(Theme::SPACE_2))
            .bg(Theme::bg_raised())
            .border_b_1()
            .border_color(Theme::border_mid())
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(primitives::stat_dot_sm(Theme::accent_mint()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::text_secondary())
                    .child(msg),
            )
            .child(
                div()
                    .id("global-notice-banner-dismiss")
                    .flex_shrink_0()
                    .px(px(Theme::SPACE_2))
                    .py_0p5()
                    .rounded(px(Theme::RADIUS_SM))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.notice = None;
                        cx.notify();
                    }))
                    .child("知道了"),
            )
            .into_any_element()
    }

    /// 渲染底部状态与操作栏 (iOS Minimal Toolbar 规范)
    pub(crate) fn render_bottom_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let has_file = self.state.transcribe_file.is_some();
        let has_batch = !self.state.batch_queue.is_empty();
        let can_start = (has_file || has_batch) && !is_processing;
        let can_export = !self.state.segments.is_empty();
        let can_play = self.state.selected_file.is_some() && can_export && !is_processing;

        // 只取阶段名与进度即可：`detail` 从未被消费（下游只用 stage/progress），
        // 而它是 `String`——每帧 `clone()` 一个可能非空的详情串纯属浪费。改为只借用。
        let (stage_text, progress_val) = match &self.state.status {
            ProcessStatus::Idle => ("就绪", 0.0),
            ProcessStatus::Processing {
                stage, progress, ..
            } => (stage.as_str(), *progress),
            ProcessStatus::Completed => ("转写完成", 1.0),
            ProcessStatus::Failed(_) => ("出错", 0.0),
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
            .child(div().flex().items_center().gap_3().child(if is_processing {
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
                            .child(if total_dur > 0.0 {
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_secondary())
                                    .child(format!(
                                        "{cur_mm:02}:{cur_ss:02} / {tot_mm:02}:{tot_ss:02}"
                                    ))
                            } else {
                                div()
                            })
                            .child(primitives::badge_accent(format!(
                                "{:.1}%",
                                display_ratio * 100.0
                            ))),
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
            }))
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
                                if self.state.batch_running {
                                    "批量处理中..."
                                } else {
                                    "处理中..."
                                }
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
