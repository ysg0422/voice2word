//! 语音转写主工作台视图

use gpui::prelude::*;
use gpui::*;

use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::MainWindow;

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

        div()
            .flex_1()
            .h_full()
            .bg(Theme::bg_panel())
            .p_6()
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
                            .text_size(px(18.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("语音转写"),
                    )
                    .child(
                        if let Some(ref file) = self.state.transcribe_file {
                            div()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .text_size(px(11.0))
                                .text_color(Theme::text_secondary())
                                .child(file.file_name().and_then(|s| s.to_str()).unwrap_or("视频").to_string())
                        } else {
                            div()
                        }
                    ),
            )
            // 核心状态展示区 (完全不渲染庞大表格，保证极致丝滑零卡顿)
            .child(
                if is_processing {
                    let (stage, progress, detail) = match &self.state.status {
                        ProcessStatus::Processing { stage, progress, detail } => {
                            (stage.clone(), *progress, detail.clone())
                        }
                        _ => ("正在全速转写中...".to_string(), 0.0, "".to_string()),
                    };

                    let eta = self.state.whisper_eta_label();
                    let gpu = self.state.hardware.use_gpu_pipeline();
                    div()
                        .id("lightweight-processing-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_5()
                        .child(
                            div()
                                .w(px(48.0))
                                .h(px(48.0))
                                .rounded_full()
                                .bg(rgba(0x10b98118))
                                .border_1()
                                .border_color(rgba(0x10b98133))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(div().w(px(12.0)).h(px(12.0)).rounded_full().bg(Theme::accent_mint())),
                        )
                        .child(
                            div()
                                .text_size(px(20.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child(stage),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(Theme::text_secondary())
                                .child(if detail.trim().is_empty() {
                                    "Whisper 正在转写，请稍候".to_string()
                                } else {
                                    detail
                                }),
                        )
                        // 大进度条
                        .child(
                            div()
                                .w(px(520.0))
                                .h(px(10.0))
                                .rounded_full()
                                .bg(rgb(0x18181c))
                                .border_1()
                                .border_color(Theme::border())
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .w(relative(progress.clamp(0.0, 1.0) as f32))
                                        .bg(Theme::accent_mint()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("总体进度"))
                                        .child(div().text_size(px(18.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{:.0}%", progress * 100.0))),
                                )
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("预计耗时"))
                                        .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(eta)),
                                )
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_xl()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("推理后端"))
                                        .child(div().text_size(px(15.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child(if gpu { "GPU" } else { "CPU" })),
                                ),
                        )
                        .into_any_element()
                } else if let Some(ref file) = self.state.transcribe_file {
                    // 已就绪待处理卡片 (用户刚选中了新视频，准备转写)
                    let fname = file.file_name().and_then(|s| s.to_str()).unwrap_or("视频").to_string();
                    let dur_str = format_duration_short(self.state.transcribe_duration);

                    div()
                        .id("transcribe-file-ready-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .child(
                            div()
                                .w(px(52.0))
                                .h(px(52.0))
                                .rounded_full()
                                .bg(rgba(0x38bdf81a))
                                .border_1()
                                .border_color(rgba(0x38bdf833))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(14.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::accent_blue())
                                .child("FILE"),
                        )
                        .child(
                            div()
                                .text_size(px(18.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child(fname),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(Theme::text_secondary())
                                .child(format!("时长 {} · 文件已就绪", dur_str)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .mt_2()
                                .child(
                                    div()
                                        .id("ready-start-process-btn")
                                        .px_6()
                                        .py_2p5()
                                        .rounded_full()
                                        .bg(Theme::accent_mint())
                                        .cursor_pointer()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(0x09090b))
                                        .hover(|s| s.opacity(0.9))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.start_processing(cx);
                                        }))
                                        .child("开始处理"),
                                )
                                .child(
                                    div()
                                        .id("ready-repick-file-btn")
                                        .px_5()
                                        .py_2p5()
                                        .rounded_full()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .cursor_pointer()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Theme::text_secondary())
                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_file(cx);
                                        }))
                                        .child("重新选择"),
                                ),
                        )
                        .into_any_element()
                } else {
                    // 空闲就绪引导卡片 (干净清爽，与上次任务完全解耦，等待导入新视频)
                    div()
                        .id("lightweight-idle-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(
                            div()
                                .w(px(48.0))
                                .h(px(48.0))
                                .rounded_full()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(14.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_muted())
                                .child("+"),
                        )
                        .child(
                            div()
                                .text_size(px(16.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(Theme::text_primary())
                                .child("导入音视频文件"),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(Theme::text_muted())
                                .child("支持 MP4, MKV, MOV, WAV, MP3 等格式，已完成的任务已归档在视频库"),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .mt_2()
                                .child(
                                    div()
                                        .id("idle-pick-file-btn")
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
                                            this.choose_file(cx);
                                        }))
                                        .child("选择文件"),
                                )
                                .child(
                                    div()
                                        .id("idle-goto-library-btn")
                                        .px_4()
                                        .py_2()
                                        .rounded_full()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .cursor_pointer()
                                        .text_size(px(12.0))
                                        .text_color(Theme::text_secondary())
                                        .hover(|s| s.bg(Theme::bg_hover()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.state.active_tab = WorkspaceTab::Library;
                                            this.state.refresh_recent_tasks();
                                            cx.notify();
                                        }))
                                        .child("从视频库选择"),
                                ),
                        )
                        .into_any_element()
                },
            )
    }
}
