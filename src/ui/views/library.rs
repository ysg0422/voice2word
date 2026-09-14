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
            .bg(Theme::bg_panel())
            .p_6()
            .gap_5()
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
                                    .text_color(Theme::text_primary())
                                    .child("视频库"),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(0x1e1e28))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_muted())
                                    .child(format!("{} 项", total_count)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2p5()
                            .child(
                                div()
                                    .id("library-import-btn")
                                    .px_5()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(Theme::accent_mint())
                                    .cursor_pointer()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0x09090b))
                                    .hover(|s| s.opacity(0.88))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.active_tab = WorkspaceTab::Generate;
                                        cx.notify();
                                    }))
                                    .child("导入视频"),
                            )
                            .child(
                                div()
                                    .id("library-refresh-btn")
                                    .px_4()
                                    .py_2()
                                    .rounded_lg()
                                    .bg(Theme::bg_card())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .cursor_pointer()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()).border_color(Theme::border_light()))
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
                    self.render_library_empty(cx)
                } else {
                    self.render_library_cards(tasks, cx)
                }
            )
    }

    /// 空状态
    fn render_library_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(16.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_secondary())
                    .child("暂无解析历史"),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(Theme::text_muted())
                    .child("导入视频文件后，转写记录将在此显示"),
            )
            .child(
                div()
                    .id("empty-lib-goto-gen")
                    .mt_2()
                    .px_6()
                    .py_2p5()
                    .rounded_lg()
                    .bg(Theme::accent_mint())
                    .cursor_pointer()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0x09090b))
                    .hover(|s| s.opacity(0.88))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.state.active_tab = WorkspaceTab::Generate;
                        cx.notify();
                    }))
                    .child("导入视频开始转写"),
            )
            .into_any_element()
    }

    /// 卡片列表
    fn render_library_cards(
        &self,
        tasks: Vec<crate::storage::db::TaskRecord>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("library-cards-scroll")
            .flex_1()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .children(tasks.into_iter().map(|task| {
                self.render_library_card(task, cx)
            }))
            .into_any_element()
    }

    /// 单条视频卡片 —— 横向布局：缩略图 | 信息区 | 操作按钮
    fn render_library_card(
        &self,
        task: crate::storage::db::TaskRecord,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let task_id = task.id;
        let task_clone = task.clone();
        let task_export = task.clone();
        let seg_len = task.segments.len();
        let dur_str = format_duration_short(task.duration);

        // 文件扩展名
        let ext = task.file_name
            .rsplit('.')
            .next()
            .unwrap_or("MP4")
            .to_uppercase();

        // 格式化文件大小 (从 file_path 尝试获取)
        let file_size_str = std::fs::metadata(&task.file_path)
            .ok()
            .map(|m| {
                let bytes = m.len();
                if bytes >= 1_073_741_824 {
                    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
                } else if bytes >= 1_048_576 {
                    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
                } else {
                    format!("{:.0} KB", bytes as f64 / 1024.0)
                }
            })
            .unwrap_or_else(|| "未知大小".to_string());

        // 摘录首条字幕文本
        let sample_text = task.segments.first()
            .map(|s| {
                let t = s.display_text().to_string();
                if t.len() > 80 { format!("{}...", &t[..77]) } else { t }
            })
            .unwrap_or_else(|| "无字幕内容".to_string());

        // 格式化日期（取日期部分）
        let date_display = if task.created_at.len() >= 10 {
            task.created_at[..10].to_string()
        } else {
            task.created_at.clone()
        };

        div()
            .id(("lib-card", task_id as usize))
            .w_full()
            .p_4()
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .hover(|s| s.border_color(Theme::border_light()).bg(rgb(0x242428)))
            .flex()
            .flex_row()
            .gap_4()
            // ── 左侧：缩略图占位 ──
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(180.0))
                    .h(px(108.0))
                    .rounded_lg()
                    .bg(rgb(0x12121a))
                    .border_1()
                    .border_color(rgb(0x1e1e2a))
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_col()
                    .gap_1()
                    // 格式标识
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(0x1c1c28))
                            .border_1()
                            .border_color(rgb(0x2a2a38))
                            .text_size(px(13.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_muted())
                            .child(ext),
                    )
                    // 时长标签
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_muted())
                            .child(dur_str.clone()),
                    ),
            )
            // ── 中间：信息区 ──
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_between()
                    .gap_2p5()
                    // 文件名 + 状态
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2p5()
                                    // 文件名 (大字)
                                    .child(
                                        div()
                                            .text_size(px(16.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .overflow_hidden()
                                            .child(task.file_name.clone()),
                                    )
                                    // 已完成标记
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .w(px(8.0))
                                            .h(px(8.0))
                                            .rounded_full()
                                            .bg(Theme::accent_mint()),
                                    ),
                            )
                            // 元数据标签行
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    // 时长标签
                                    .child(Self::render_meta_pill("时长", &dur_str))
                                    // 文件大小
                                    .child(Self::render_meta_pill("大小", &file_size_str))
                                    // 字幕段数
                                    .child(Self::render_meta_pill("字幕", &format!("{} 句", seg_len)))
                                    // 处理耗时 (如果有 metrics)
                                    .children(task.metrics.as_ref().map(|m| {
                                        Self::render_meta_pill("耗时", &format!("{:.1}s", m.total_elapsed_sec))
                                    })),
                            ),
                    )
                    // 字幕预览文本
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(Theme::text_muted())
                            .overflow_hidden()
                            .child(sample_text),
                    )
                    // 日期
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgba(0xffffff33))
                            .child(date_display),
                    ),
            )
            // ── 右侧：操作按钮组 ──
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_center()
                    .gap_2()
                    // 耗时详情 (if metrics)
                    .children(task.metrics.clone().map(|m| {
                        let fname = task.file_name.clone();
                        div()
                            .id(("lib-metrics-btn", task_id as usize))
                            .w(px(110.0))
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .bg(rgb(0x1a2420))
                            .border_1()
                            .border_color(rgba(0x10b98130))
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::accent_mint())
                            .hover(|s| s.bg(rgb(0x22322a)).border_color(rgba(0x10b98150)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.benchmark_dialog = Some(crate::ui::types::BenchmarkDialogInfo {
                                    file_name: fname.clone(),
                                    metrics: m.clone(),
                                });
                                cx.notify();
                            }))
                            .flex()
                            .justify_center()
                            .child("耗时详情")
                    }))
                    // 剪辑按钮 (主操作)
                    .child(
                        div()
                            .id(("lib-edit-btn", task_id as usize))
                            .w(px(110.0))
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .bg(Theme::accent_mint())
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(0x09090b))
                            .hover(|s| s.opacity(0.88))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.load_task(&task_clone);
                                this.trigger_extract_frame(cx);
                                this.ensure_preview_proxy(cx);
                                cx.notify();
                            }))
                            .flex()
                            .justify_center()
                            .child("剪辑校对"),
                    )
                    // 导出按钮
                    .child(
                        div()
                            .id(("lib-export-btn", task_id as usize))
                            .w(px(110.0))
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .bg(rgb(0x1c1c26))
                            .border_1()
                            .border_color(Theme::border())
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).border_color(Theme::border_light()))
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
                            .flex()
                            .justify_center()
                            .child("导出字幕"),
                    )
                    // 删除按钮
                    .child(
                        div()
                            .id(("lib-del-btn", task_id as usize))
                            .w(px(110.0))
                            .px_3()
                            .py_1p5()
                            .rounded_lg()
                            .bg(rgba(0xf43f5e0a))
                            .border_1()
                            .border_color(rgba(0xf43f5e20))
                            .cursor_pointer()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::accent_red())
                            .hover(|s| s.bg(rgba(0xf43f5e18)).border_color(rgba(0xf43f5e38)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.delete_task_record(task_id);
                                if this.state.selected_file.is_some() {
                                    this.trigger_extract_frame(cx);
                                }
                                cx.notify();
                            }))
                            .flex()
                            .justify_center()
                            .child("删除"),
                    ),
            )
    }

    /// 元数据标签 (圆角小药丸)：标签名 + 值
    fn render_meta_pill(label: &str, value: &str) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2p5()
            .py_1()
            .rounded_md()
            .bg(rgb(0x1a1a24))
            .border_1()
            .border_color(rgb(0x252530))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(Theme::text_muted())
                    .child(format!("{}:", label)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .child(value.to_string()),
            )
    }
}
