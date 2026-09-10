//! 媒体库与历史任务视图

use gpui::prelude::*;
use gpui::*;

use crate::app::state::WorkspaceTab;
use crate::subtitle::SubtitleWriter;
use crate::utils::time::format_duration_short;
use super::super::theme::Theme;
use super::super::MainWindow;

impl MainWindow {
    /// 渲染历史视频库 (视频资产管理与一键载入工作台)
    pub(crate) fn render_library_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let tasks = self.state.recent_tasks.clone();
        let total_count = tasks.len();

        div()
            .id("library-workspace-layout")
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .bg(Theme::bg_app())
            .p_6()
            .gap_4()
            .overflow_hidden()
            // 顶部标头栏
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(20.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("视频库"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgb(0x1e1e24))
                                    .border_1()
                                    .border_color(rgb(0x2a2a32))
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .child(format!("{} 项", total_count)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("library-import-btn")
                                    .px_4()
                                    .py_1p5()
                                    .rounded_full()
                                    .bg(Theme::accent_mint())
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(0x09090b))
                                    .hover(|s| s.opacity(0.9))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.active_tab = WorkspaceTab::Generate;
                                        cx.notify();
                                    }))
                                    .child("导入视频"),
                            )
                            .child(
                                div()
                                    .id("library-refresh-btn")
                                    .px_3p5()
                                    .py_1p5()
                                    .rounded_full()
                                    .bg(Theme::bg_card())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .cursor_pointer()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.refresh_recent_tasks();
                                        cx.notify();
                                    }))
                                    .child("刷新"),
                            ),
                    ),
            )
            // 视频卡片列表区域
            .child(
                if tasks.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(15.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(Theme::text_secondary())
                                .child("暂无解析历史"),
                        )
                        .child(
                            div()
                                .id("empty-lib-goto-gen")
                                .mt_2()
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
                                    this.state.active_tab = WorkspaceTab::Generate;
                                    cx.notify();
                                }))
                                .child("导入视频转写"),
                        )
                        .into_any_element()
                } else {
                    div()
                        .id("library-cards-scroll")
                        .flex_1()
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .children(tasks.into_iter().map(|task| {
                            let task_id = task.id;
                            let task_clone = task.clone();
                            let task_export = task.clone();
                            let seg_len = task.segments.len();
                            let dur_str = format_duration_short(task.duration);
                            let sample_text = task.segments.first()
                                .map(|s| s.display_text().to_string())
                                .unwrap_or_else(|| "无字幕内容".to_string());

                            div()
                                .id(("lib-card", task_id as usize))
                                .p_4()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .hover(|s| s.border_color(Theme::border_light()))
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_4()
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_3()
                                        .flex_1()
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .w(px(48.0))
                                                .h(px(48.0))
                                                .rounded_lg()
                                                .bg(rgb(0x18181e))
                                                .border_1()
                                                .border_color(Theme::border())
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(Theme::text_muted())
                                                .child("MP4"),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap_1()
                                                .flex_1()
                                                .overflow_hidden()
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
                                                                .child(task.file_name.clone()),
                                                        )
                                                        .child(
                                                            div()
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded(px(3.0))
                                                                .bg(rgba(0x2dd4bf20))
                                                                .text_size(px(10.0))
                                                                .text_color(Theme::accent_mint())
                                                                .child("已完成"),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_3()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_muted())
                                                        .child(if let Some(ref m) = task.metrics {
                                                            format!("{} · {} 句 · 耗时 {:.1}s · {}", dur_str, seg_len, m.total_elapsed_sec, task.created_at)
                                                        } else {
                                                            format!("{} · {} 句 · {}", dur_str, seg_len, task.created_at)
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("\"{}\"", sample_text)),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .children(task.metrics.clone().map(|m| {
                                            let fname = task.file_name.clone();
                                            div()
                                                .id(("lib-metrics-btn", task_id as usize))
                                                .px_3()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(rgb(0x1a2420))
                                                .border_1()
                                                .border_color(rgba(0x10b98140))
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(Theme::accent_mint())
                                                .hover(|s| s.bg(rgb(0x22322a)))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.benchmark_dialog = Some(crate::ui::types::BenchmarkDialogInfo {
                                                        file_name: fname.clone(),
                                                        metrics: m.clone(),
                                                    });
                                                    cx.notify();
                                                }))
                                                .child("耗时详情")
                                        }))
                                        .child(
                                            div()
                                                .id(("lib-edit-btn", task_id as usize))
                                                .px_4()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::accent_mint())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x09090b))
                                                .hover(|s| s.opacity(0.9))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.state.load_task(&task_clone);
                                                    this.trigger_extract_frame(cx);
                                                    this.ensure_preview_proxy(cx);
                                                    cx.notify();
                                                }))
                                                .child("剪辑"),
                                        )
                                        .child(
                                            div()
                                                .id(("lib-export-btn", task_id as usize))
                                                .px_3p5()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(move |_this, _, _, cx| {
                                                    let task_name = task_export.file_name.clone();
                                                    let segs = task_export.segments.clone();
                                                    cx.spawn(async move |_this, _cx| {
                                                        if let Some(handle) = rfd::AsyncFileDialog::new()
                                                            .set_file_name(&format!("{}.srt", task_name))
                                                            .add_filter("SubRip Subtitle", &["srt"])
                                                            .save_file()
                                                            .await
                                                        {
                                                            let save_path = handle.path().to_path_buf();
                                                            let _ = SubtitleWriter::write_srt(&segs, &save_path);
                                                        }
                                                    })
                                                    .detach();
                                                }))
                                                .child("导出"),
                                        )
                                        .child(
                                            div()
                                                .id(("lib-del-btn", task_id as usize))
                                                .px_3()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::accent_red())
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.state.delete_task_record(task_id);
                                                    if this.state.selected_file.is_some() {
                                                        this.trigger_extract_frame(cx);
                                                    }
                                                    cx.notify();
                                                }))
                                                .child("删除"),
                                        ),
                                )
                        }))
                        .into_any_element()
                }
            )
    }
}
