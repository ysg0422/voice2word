//! 类似剪映 / Premiere 风格的多轨视频剪辑与字幕校对工作台
//! 提供：
//! 1. 顶部工作模式 Tab 切换 (智能生成 vs 剪辑校对)
//! 2. 视频画面同步监视器 (支持帧级预览与电影级字幕叠加)
//! 3. 字幕属性检查器 (支持实时修改错字、增删标点、时间微调、拆分与合并)
//! 4. 专业多轨时间轴 (时间刻度标尺、红/青色垂直指针游标、视频轨与字幕胶囊块)

use gpui::prelude::*;
use gpui::*;
use crate::app::state::WorkspaceTab;
use crate::utils::time::{format_duration_short, seconds_to_srt_time};
use super::theme::Theme;
use super::MainWindow;

impl MainWindow {
    /// 渲染顶部模式切换栏 (Tab 导航器)
    pub(crate) fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active_tab;
        let seg_count = self.state.segments.len();

        div()
            .id("app-tab-bar")
            .w_full()
            .h(px(42.0))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .px_4()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            // iOS 分段切换胶囊 (Segmented Control)
            .child(
                div()
                    .bg(rgb(0x18181e))
                    .p(px(2.5))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x282832))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .id("tab-btn-editor")
                            .px_3p5()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Editor {
                                rgb(0x2c2c36)
                            } else {
                                rgb(0x00000000)
                            })
                            .text_size(px(12.0))
                            .font_weight(if active == WorkspaceTab::Editor {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Editor {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(|s| s.text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Editor;
                                this.trigger_extract_frame(cx);
                                cx.notify();
                            }))
                            .child("🎬 剪辑校对"),
                    )
                    .child(
                        div()
                            .id("tab-btn-generate")
                            .px_3p5()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Generate {
                                rgb(0x2c2c36)
                            } else {
                                rgb(0x00000000)
                            })
                            .text_size(px(12.0))
                            .font_weight(if active == WorkspaceTab::Generate {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Generate {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(|s| s.text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Generate;
                                cx.notify();
                            }))
                            .child("⚡ 语音转写"),
                    )
                    .child(
                        div()
                            .id("tab-btn-library")
                            .px_3p5()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Library {
                                rgb(0x2c2c36)
                            } else {
                                rgb(0x00000000)
                            })
                            .text_size(px(12.0))
                            .font_weight(if active == WorkspaceTab::Library {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if active == WorkspaceTab::Library {
                                rgb(0xffffff)
                            } else {
                                Theme::text_secondary()
                            })
                            .hover(|s| s.text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Library;
                                this.state.refresh_recent_tasks();
                                cx.notify();
                            }))
                            .child("📚 视频库"),
                    ),
            )
            // 右侧极简导出胶囊按钮
            .child(
                div()
                    .id("quick-export-srt-btn")
                    .px_3p5()
                    .py_1()
                    .rounded_full()
                    .bg(if seg_count > 0 { Theme::accent_mint() } else { Theme::bg_card() })
                    .text_color(if seg_count > 0 { rgb(0x09090b) } else { Theme::text_muted() })
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.state.segments.is_empty() {
                            this.export_subtitles(cx);
                        }
                    }))
                    .child("💾 导出字幕"),
            )
    }

    /// 渲染类似剪映 / Premiere 风格的剪辑工作区布局
    pub(crate) fn render_editor_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("editor-workspace-layout")
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .overflow_hidden()
            // 上半区：视频监视器 (左侧 58%) + 字幕属性检查器 (右侧 42%)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(self.render_video_monitor(cx))
                    .child(self.render_subtitle_inspector(cx)),
            )
            // 下半区：专业多轨时间轴 (固定高度 230px)
            .child(self.render_multitrack_timeline(cx))
    }

    /// 渲染专业视频监视器 (Preview Monitor)
    pub(crate) fn render_video_monitor(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let cur_time = self.state.current_time;
        let tot_time = self.state.total_duration.max(1.0);
        let active_seg = self.state.get_active_segment().cloned();
        let is_playing = self.state.is_playing;

        div()
            .id("editor-video-monitor")
            .w(relative(0.58))
            .h_full()
            .bg(rgb(0x09090b))
            .border_r_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 监视器标头
            .child(
                div()
                    .h(px(36.0))
                    .px_4()
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_primary())
                            .child("视频预览"),
                    )
                    .child(
                        div()
                            .id("monitor-ffplay-btn")
                            .px_3()
                            .py_0p5()
                            .rounded_full()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.play_video(cx);
                            }))
                            .child("▶ 外部播放"),
                    ),
            )
            // 16:9 监视器视口屏幕
            .child(
                div()
                    .id("monitor-viewport-screen")
                    .flex_1()
                    .w_full()
                    .relative()
                    .bg(rgb(0x050507))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    // 视频画面展示
                    .child(
                        if let Some(ref frame_path) = self.state.preview_frame_path {
                            if frame_path.exists() {
                                div()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        img(frame_path.clone())
                                            .size_full()
                                    )
                            } else {
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_size(px(24.0)).child("⏳"))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(Theme::text_muted())
                                            .child("正在提取对应视频帧..."),
                                    )
                            }
                        } else if self.state.selected_file.is_some() {
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(div().text_size(px(28.0)).child("🎞️"))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(Theme::accent_mint())
                                        .child("正在同步视频预览画面..."),
                                )
                        } else {
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_size(px(32.0))
                                        .child("🎬"),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .text_color(Theme::text_muted())
                                        .child("拖动下方时间轴指针，画面与字幕将在此实时同步"),
                                )
                        }
                    )
                    // 电影级高对比度字幕覆盖居中层
                    .child(
                        div()
                            .absolute()
                            .bottom(px(18.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .px_6()
                            .child(
                                if let Some(seg) = active_seg {
                                    div()
                                        .px_4()
                                        .py_1p5()
                                        .rounded_md()
                                        .bg(rgba(0x000000dd))
                                        .border_1()
                                        .border_color(rgba(0xffffff28))
                                        .text_size(px(15.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(0xffffff))
                                        .text_align(TextAlign::Center)
                                        .child(seg.display_text().to_string())
                                } else {
                                    div()
                                }
                            ),
                    ),
            )
            // 监视器底部播放控制器与时间码显示
            .child(
                div()
                    .h(px(48.0))
                    .px_4()
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    // iOS 紧凑媒体控制条
                    .child(
                        div()
                            .bg(rgb(0x16161c))
                            .p(px(2.5))
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(0x282832))
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            // 上一句
                            .child(
                                div()
                                    .id("ctrl-prev-seg")
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_prev_segment(cx);
                                    }))
                                    .child("⏮ 上句"),
                            )
                            // 快退 1 秒
                            .child(
                                div()
                                    .id("ctrl-step-back")
                                    .px_2p5()
                                    .py_1()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let target = (this.state.current_time - 1.0).max(0.0);
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }))
                                    .child("◀ 1s"),
                            )
                            // 走帧播放 / 暂停 (主要突出胶囊)
                            .child(
                                div()
                                    .id("ctrl-play-pause")
                                    .px_4()
                                    .py_1()
                                    .rounded_full()
                                    .bg(if is_playing { Theme::accent_orange() } else { Theme::accent_mint() })
                                    .text_color(rgb(0x09090b))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .hover(|s| s.opacity(0.9))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_play_preview(cx);
                                    }))
                                    .child(if is_playing { "⏸ 暂停" } else { "▶ 走帧" }),
                            )
                            // 快进 1 秒
                            .child(
                                div()
                                    .id("ctrl-step-fwd")
                                    .px_2p5()
                                    .py_1()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let target = this.state.current_time + 1.0;
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }))
                                    .child("1s ▶"),
                            )
                            // 下一句
                            .child(
                                div()
                                    .id("ctrl-next-seg")
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_next_segment(cx);
                                    }))
                                    .child("下句 ⏭"),
                            ),
                    )
                    // 右侧时间码 (iOS 胶囊卡片)
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(0x16161c))
                            .border_1()
                            .border_color(rgb(0x282832))
                            .flex()
                            .items_center()
                            .gap_1()
                            .font_family("Consolas")
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .text_color(Theme::accent_mint())
                                    .font_weight(FontWeight::BOLD)
                                    .child(seconds_to_srt_time(cur_time)),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child("/"),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_secondary())
                                    .child(seconds_to_srt_time(tot_time)),
                            ),
                    ),
            )
    }

    /// 渲染字幕属性检查器与错别字编辑区 (Inspector)
    pub(crate) fn render_subtitle_inspector(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let sel_idx = self.state.selected_segment_index;
        let cur_seg = sel_idx.and_then(|idx| self.state.segments.iter().find(|s| s.index == idx)).cloned();
        let cur_text = self.state.editing_text.clone();

        div()
            .id("editor-subtitle-inspector")
            .w(relative(0.42))
            .h_full()
            .bg(Theme::bg_sidebar())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 标头
            .child(
                div()
                    .h(px(36.0))
                    .px_4()
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text_primary())
                            .child("字幕编辑"),
                    )
                    .child(
                        div()
                            .px_2p5()
                            .py_0p5()
                            .rounded_full()
                            .bg(rgb(0x181820))
                            .border_1()
                            .border_color(rgb(0x282832))
                            .text_size(px(11.0))
                            .text_color(Theme::text_secondary())
                            .child(if let Some(idx) = sel_idx {
                                format!("{} / {} 句", idx, self.state.segments.len())
                            } else {
                                format!("{} 句", self.state.segments.len())
                            }),
                    ),
            )
            // 属性面板主体
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    // 1. 时间戳精确微调卡片 (iOS Inset Card)
                    .child(
                        if let Some(ref seg) = cur_seg {
                            div()
                                .p_3p5()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .gap_2p5()
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(Theme::text_muted())
                                        .child("时间微调"),
                                )
                                // 开始时间微调
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(Theme::text_secondary())
                                                .child(format!("开始: {}", seconds_to_srt_time(seg.start))),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .id("btn-adj-start-m5")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(-0.5, 0.0);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("-0.5s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-start-m1")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(-0.1, 0.0);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("-0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-start-p1")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.1, 0.0);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("+0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-start-p5")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.5, 0.0);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("+0.5s"),
                                                ),
                                        ),
                                )
                                // 结束时间微调
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(Theme::text_secondary())
                                                .child(format!("结束: {}", seconds_to_srt_time(seg.end))),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .id("btn-adj-end-m5")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, -0.5);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("-0.5s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-end-m1")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, -0.1);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("-0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-end-p1")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, 0.1);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("+0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-adj-end-p5")
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_full()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .text_size(px(10.0))
                                                        .text_color(Theme::text_secondary())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, 0.5);
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        }))
                                                        .child("+0.5s"),
                                                ),
                                        ),
                                )
                        } else {
                            div()
                                .p_4()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .text_size(px(12.0))
                                .text_color(Theme::text_muted())
                                .child("点击下方时间轴或列表任选一句字幕进行编辑")
                        }
                    )
                    // 2. 文本内容编辑与错字校正卡片 (iOS Inset Card)
                    .child(
                        if cur_seg.is_some() {
                            div()
                                .p_3p5()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .gap_2p5()
                                .child(
                                    div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_muted())
                                            .child("字幕文本"),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .child(
                                                div()
                                                    .id("btn-popup-edit-text")
                                                    .px_3()
                                                    .py_0p5()
                                                    .rounded_full()
                                                    .bg(rgba(0x10b98120))
                                                    .border_1()
                                                    .border_color(Theme::accent_mint())
                                                    .cursor_pointer()
                                                    .text_size(px(11.0))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(Theme::accent_mint())
                                                    .hover(|s| s.bg(rgba(0x10b98135)))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.prompt_edit_text(cx);
                                                    }))
                                                    .child("✏️ 中文输入"),
                                            )
                                            .child(
                                                div()
                                                    .id("btn-paste-clipboard")
                                                    .px_3()
                                                    .py_0p5()
                                                    .rounded_full()
                                                    .bg(rgb(0x181820))
                                                    .border_1()
                                                    .border_color(rgb(0x282832))
                                                    .cursor_pointer()
                                                    .text_size(px(11.0))
                                                    .text_color(Theme::text_secondary())
                                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        if let Some(item) = cx.read_from_clipboard() {
                                                            if let Some(text) = item.text() {
                                                                this.state.editing_text = text;
                                                                this.state.save_selected_text();
                                                                cx.notify();
                                                            }
                                                        }
                                                    }))
                                                    .child("粘贴"),
                                            ),
                                    ),
                                )
                                // 字幕文本显示与键盘直接键入容器
                                .child({
                                    let is_focused = self.is_text_focused;
                                    div()
                                        .id("subtitle-text-editor-box")
                                        .track_focus(&self.text_focus)
                                        .min_h(px(60.0))
                                        .p_3()
                                        .rounded_lg()
                                        .bg(Theme::bg_sidebar())
                                        .border_1()
                                        .border_color(if is_focused { Theme::accent_mint() } else { Theme::border() })
                                        .cursor_text()
                                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                            window.focus(&this.text_focus);
                                            this.is_text_focused = true;
                                            cx.notify();
                                        }))
                                        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                            let key = &event.keystroke.key;
                                            if event.keystroke.modifiers.control {
                                                if key == "v" {
                                                    if let Some(item) = cx.read_from_clipboard() {
                                                        if let Some(text) = item.text() {
                                                            this.state.editing_text.push_str(&text);
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    }
                                                } else if key == "c" {
                                                    let item = ClipboardItem::new_string(this.state.editing_text.clone());
                                                    cx.write_to_clipboard(item);
                                                }
                                                return;
                                            }

                                            if key == "backspace" {
                                                this.state.editing_text.pop();
                                                this.state.save_selected_text();
                                                cx.notify();
                                            } else if key == "enter" {
                                                this.state.save_selected_text();
                                                this.is_text_focused = false;
                                                cx.notify();
                                            } else if key == "space" {
                                                this.state.editing_text.push(' ');
                                                this.state.save_selected_text();
                                                cx.notify();
                                            } else if key.chars().count() == 1 {
                                                this.state.editing_text.push_str(key);
                                                this.state.save_selected_text();
                                                cx.notify();
                                            }
                                        }))
                                        .text_size(px(14.0))
                                        .line_height(relative(1.4))
                                        .text_color(Theme::text_primary())
                                        .child(if cur_text.is_empty() {
                                            if is_focused {
                                                "▌".to_string()
                                            } else {
                                                "点击直接输入字幕...".to_string()
                                            }
                                        } else {
                                            if is_focused {
                                                format!("{}▌", cur_text)
                                            } else {
                                                cur_text
                                            }
                                        })
                                })
                                // 标点快捷修正栏
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .child(self.render_punct_btn("，", cx))
                                                .child(self.render_punct_btn("。", cx))
                                                .child(self.render_punct_btn("？", cx))
                                                .child(self.render_punct_btn("！", cx))
                                                .child(self.render_punct_btn("、", cx))
                                                .child(self.render_punct_btn("“", cx))
                                                .child(self.render_punct_btn("”", cx)),
                                        )
                                        .child(
                                            div()
                                                .id("btn-del-last-char")
                                                .px_2p5()
                                                .py_0p5()
                                                .rounded_full()
                                                .bg(rgb(0x181820))
                                                .border_1()
                                                .border_color(rgb(0x282832))
                                                .cursor_pointer()
                                                .text_size(px(10.0))
                                                .text_color(Theme::accent_orange())
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.editing_text.pop();
                                                    this.state.save_selected_text();
                                                    cx.notify();
                                                }))
                                                .child("⌫ 删末字"),
                                        ),
                                )
                                // 片段操作操作栏：保存、拆分、合并、删除 (iOS 胶囊组)
                                .child(
                                    div()
                                        .pt_1()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .id("btn-save-text")
                                                .flex_1()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::accent_mint())
                                                .cursor_pointer()
                                                .text_size(px(12.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x09090b))
                                                .text_align(TextAlign::Center)
                                                .hover(|s| s.opacity(0.9))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.save_selected_text();
                                                    cx.notify();
                                                }))
                                                .child("💾 保存修改"),
                                        )
                                        .child(
                                            div()
                                                .id("btn-split-seg")
                                                .px_3p5()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.split_selected_segment();
                                                    this.trigger_extract_frame(cx);
                                                    cx.notify();
                                                }))
                                                .child("✂ 拆分"),
                                        )
                                        .child(
                                            div()
                                                .id("btn-merge-seg")
                                                .px_3p5()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.merge_selected_with_next();
                                                    this.trigger_extract_frame(cx);
                                                    cx.notify();
                                                }))
                                                .child("🔗 合并"),
                                        )
                                        .child(
                                            div()
                                                .id("btn-del-seg")
                                                .px_3()
                                                .py_1p5()
                                                .rounded_full()
                                                .bg(rgba(0xf43f5e15))
                                                .border_1()
                                                .border_color(rgba(0xf43f5e30))
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(rgb(0xf43f5e))
                                                .hover(|s| s.bg(rgba(0xf43f5e30)))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.delete_selected_segment();
                                                    this.trigger_extract_frame(cx);
                                                    cx.notify();
                                                }))
                                                .child("🗑"),
                                        ),
                                )
                        } else {
                            div()
                        }
                    )
                    // 3. 字幕速查微缩列表 (iOS Inset List)
                    .child({
                        let total_segs = self.state.segments.len();
                        let focus_idx = sel_idx.or_else(|| self.state.get_active_segment().map(|s| s.index)).unwrap_or(1);
                        let window_size = if total_segs > 1000 {
                            4
                        } else if total_segs > 100 {
                            6
                        } else {
                            10
                        };
                        let start_idx = focus_idx.saturating_sub(window_size / 2).max(1);
                        let end_idx = (start_idx + window_size).min(total_segs);
                        let visible_segments: Vec<_> = self.state.segments.iter()
                            .filter(|s| s.index >= start_idx && s.index <= end_idx)
                            .cloned()
                            .collect();

                        div()
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_muted())
                                            .child(format!(
                                                "字幕列表 ({}-{} / {})",
                                                start_idx, end_idx, total_segs
                                            )),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .id("list-jump-prev-10")
                                                    .px_2p5()
                                                    .py_0p5()
                                                    .rounded_full()
                                                    .bg(rgb(0x181820))
                                                    .border_1()
                                                    .border_color(rgb(0x282832))
                                                    .cursor_pointer()
                                                    .text_size(px(10.0))
                                                    .text_color(Theme::text_secondary())
                                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        let target = focus_idx.saturating_sub(10).max(1);
                                                        this.state.select_segment(target);
                                                        this.trigger_extract_frame(cx);
                                                        cx.notify();
                                                    }))
                                                    .child("◀ 10句"),
                                            )
                                            .child(
                                                div()
                                                    .id("list-jump-next-10")
                                                    .px_2p5()
                                                    .py_0p5()
                                                    .rounded_full()
                                                    .bg(rgb(0x181820))
                                                    .border_1()
                                                    .border_color(rgb(0x282832))
                                                    .cursor_pointer()
                                                    .text_size(px(10.0))
                                                    .text_color(Theme::text_secondary())
                                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        let target = (focus_idx + 10).min(total_segs);
                                                        this.state.select_segment(target);
                                                        this.trigger_extract_frame(cx);
                                                        cx.notify();
                                                    }))
                                                    .child("10句 ▶"),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .id("inspector-segments-scroll")
                                    .h(px(180.0))
                                    .overflow_hidden()
                                    .border_1()
                                    .border_color(Theme::border())
                                    .rounded_xl()
                                    .bg(Theme::bg_card())
                                    .flex()
                                    .flex_col()
                                    .children(visible_segments.into_iter().map(|seg| {
                                        let seg_idx = seg.index;
                                        let is_selected = sel_idx == Some(seg_idx);
                                        let is_playing_here = self.state.current_time >= seg.start && self.state.current_time <= seg.end;
                                        div()
                                            .id(("insp-seg-item", seg_idx))
                                            .px_2p5()
                                            .py_1p5()
                                            .border_b_1()
                                            .border_color(Theme::border())
                                            .cursor_pointer()
                                            .bg(if is_selected {
                                                Theme::bg_hover()
                                            } else if is_playing_here {
                                                rgba(0x10b98118)
                                            } else {
                                                rgb(0x00000000)
                                            })
                                            .hover(|s| s.bg(Theme::bg_hover()))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.state.select_segment(seg_idx);
                                                this.trigger_extract_frame(cx);
                                                cx.notify();
                                            }))
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .w(px(32.0))
                                                    .text_size(px(10.0))
                                                    .font_family("Consolas")
                                                    .text_color(if is_selected {
                                                        Theme::accent_mint()
                                                    } else {
                                                        Theme::text_muted()
                                                    })
                                                    .child(format!("#{:03}", seg_idx)),
                                            )
                                            .child(
                                                div()
                                                    .w(px(110.0))
                                                    .text_size(px(10.0))
                                                    .font_family("Consolas")
                                                    .text_color(Theme::text_secondary())
                                                    .child(format!(
                                                        "{}-{}",
                                                        seconds_to_srt_time(seg.start),
                                                        seconds_to_srt_time(seg.end)
                                                    )),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_size(px(12.0))
                                                    .text_color(if is_selected {
                                                        Theme::text_primary()
                                                    } else {
                                                        Theme::text_secondary()
                                                    })
                                                    .child(seg.display_text().to_string()),
                                            )
                                    })),
                            )
                    }),
            )
    }

    /// 标点快捷注入按钮 (iOS Pill Chip)
    fn render_punct_btn(&mut self, punct: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(punct)
            .px_2p5()
            .py_0p5()
            .rounded_full()
            .bg(rgb(0x181820))
            .border_1()
            .border_color(rgb(0x282832))
            .cursor_pointer()
            .text_size(px(11.0))
            .text_color(Theme::text_primary())
            .hover(|s| s.bg(Theme::bg_hover()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.editing_text.push_str(punct);
                this.state.save_selected_text();
                cx.notify();
            }))
            .child(punct)
    }

    /// 渲染专业多轨剪辑时间轴 (Multi-track Timeline - 剪映/Premiere风格)
    pub(crate) fn render_multitrack_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let total_dur = self.state.total_duration.max(1.0);
        let cur_time = self.state.current_time;
        let progress_ratio = (cur_time / total_dur).clamp(0.0, 1.0) as f32;
        let sel_idx = self.state.selected_segment_index;
        let active_seg = self.state.get_active_segment().cloned();
        let cur_seg = sel_idx.and_then(|idx| self.state.segments.iter().find(|s| s.index == idx)).cloned();

        div()
            .id("editor-multitrack-timeline")
            .h(px(230.0))
            .w_full()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 时间轴顶栏工具区
            .child(
                div()
                    .h(px(36.0))
                    .px_4()
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
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
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("时间轴"),
                            )
                            .child(
                                div()
                                    .px_2p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgb(0x181820))
                                    .border_1()
                                    .border_color(rgb(0x282832))
                                    .text_size(px(11.0))
                                    .font_family("Consolas")
                                    .text_color(Theme::accent_mint())
                                    .font_weight(FontWeight::BOLD)
                                    .child(seconds_to_srt_time(cur_time)),
                            ),
                    )
                    // 快捷跳转分段容器 (iOS Segmented Style)
                    .child(
                        div()
                            .bg(rgb(0x16161c))
                            .p(px(2.0))
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(0x282832))
                            .flex()
                            .items_center()
                            .gap(px(1.0))
                            .child(self.render_jump_btn("0%", 0.0, cx))
                            .child(self.render_jump_btn("25%", total_dur * 0.25, cx))
                            .child(self.render_jump_btn("50%", total_dur * 0.50, cx))
                            .child(self.render_jump_btn("75%", total_dur * 0.75, cx))
                            .child(self.render_jump_btn("100%", total_dur, cx)),
                    ),
            )
            // 时间刻度标尺 (Time Ruler - 点击/拖拽任意位置精确定位)
            .child(
                div()
                    .id("timeline-time-ruler")
                    .h(px(24.0))
                    .w_full()
                    .bg(rgb(0x131316))
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .pl(px(70.0)) // 留出左侧轨道标号宽度
                    .relative()
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        let win_w = window.viewport_size().width;
                        this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                    }))
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                        }
                    }))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this, event: &MouseUpEvent, window, cx| {
                        let win_w = window.viewport_size().width;
                        this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                    }))
                    .children((0usize..=10).map(|i| {
                        let ratio = i as f64 / 10.0;
                        let t = total_dur * ratio;
                        div()
                            .id(("ruler-tick-btn", i))
                            .absolute()
                            .left(relative(ratio as f32))
                            .h_full()
                            .flex()
                            .flex_col()
                            .items_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.seek_to(t);
                                this.trigger_extract_frame(cx);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(1.0))
                                    .h(px(6.0))
                                    .bg(Theme::text_muted()),
                            )
                            .child(
                                div()
                                    .text_size(px(9.0))
                                    .font_family("Consolas")
                                    .text_color(Theme::text_muted())
                                    .child(format_duration_short(t)),
                            )
                    })),
            )
            // 核心多轨道区域 (Video Track + Subtitle Track)
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .relative()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_2()
                    // 1. 视频主轨道 (V1 Video Track)
                    .child(
                        div()
                            .h(px(36.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w(px(70.0))
                                    .pl_3()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_secondary())
                                    .child("V1 视频"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .mr_4()
                                    .rounded_xl()
                                    .bg(rgb(0x1a1a20))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .relative()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(progress_ratio))
                                            .bg(rgba(0x10b98124)),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .top_1()
                                            .left_2()
                                            .text_size(px(10.0))
                                            .text_color(Theme::text_muted())
                                            .child(format!(
                                                "原视频流 · 总时长 {}",
                                                format_duration_short(total_dur)
                                            )),
                                    ),
                            ),
                    )
                    // 2. 字幕专用轨道 (T1 Subtitle Track - 轻量化智能按需渲染，消除卡顿)
                    .child(
                        div()
                            .h(px(46.0))
                            .w_full()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w(px(70.0))
                                    .pl_3()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::accent_mint())
                                    .child("T1 字幕"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h_full()
                                    .mr_4()
                                    .rounded_xl()
                                    .bg(rgb(0x18181f))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .relative()
                                    .overflow_hidden()
                                    // 进度指示底色
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(progress_ratio))
                                            .bg(rgba(0x10b98115)),
                                    )
                                    // 轨道概览文字标签 (实时显示当前字幕信息)
                                    .child(
                                        div()
                                            .absolute()
                                            .top_1()
                                            .left_2()
                                            .text_size(px(10.0))
                                            .text_color(Theme::text_muted())
                                            .child(if let Some(ref seg) = active_seg {
                                                format!(
                                                    "字幕轨 · 共 {} 句 · 当前播放: #{:03} [{}] {}",
                                                    self.state.segments.len(),
                                                    seg.index,
                                                    seconds_to_srt_time(seg.start),
                                                    seg.display_text()
                                                )
                                            } else if let Some(ref seg) = cur_seg {
                                                format!(
                                                    "字幕轨 · 共 {} 句 · 当前选中: #{:03} [{}] {}",
                                                    self.state.segments.len(),
                                                    seg.index,
                                                    seconds_to_srt_time(seg.start),
                                                    seg.display_text()
                                                )
                                            } else {
                                                format!("字幕轨 · 共 {} 句字幕", self.state.segments.len())
                                            }),
                                    )
                                    // 仅渲染当前时间附近/选中的核心字幕色块 (彻底消除 1400 句密集堆叠卡顿)
                                    .children({
                                        let relevant_segments: Vec<_> = if self.state.segments.len() <= 20 {
                                            self.state.segments.iter().collect()
                                        } else {
                                            // 优化过滤条件：扩大可见范围到 ±15 秒，提升预加载效果
                                            self.state.segments.iter().filter(|seg| {
                                                sel_idx == Some(seg.index)
                                                    || (cur_time >= seg.start && cur_time <= seg.end)
                                                    || (cur_time >= seg.start - 15.0 && cur_time <= seg.end + 15.0)
                                            }).collect()
                                        };

                                        relevant_segments.into_iter().map(|seg| {
                                            let seg_idx = seg.index;
                                            let start_r = (seg.start / total_dur).clamp(0.0, 1.0) as f32;
                                            let width_r = (((seg.end - seg.start) / total_dur).clamp(0.015, 1.0) as f32).max(0.02);
                                            let is_selected = sel_idx == Some(seg_idx);
                                            let is_active = cur_time >= seg.start && cur_time <= seg.end;
                                            div()
                                                .id(("timeline-clip", seg_idx))
                                                .absolute()
                                                .top(px(18.0))
                                                .bottom(px(3.0))
                                                .left(relative(start_r))
                                                .w(relative(width_r))
                                                .min_w(px(50.0))
                                                .rounded_md()
                                                .bg(if is_selected {
                                                    rgb(0x2563eb)
                                                } else if is_active {
                                                    rgb(0x059669)
                                                } else {
                                                    rgb(0x374151)
                                                })
                                                .border_1()
                                                .border_color(if is_selected {
                                                    rgb(0xffffff)
                                                } else if is_active {
                                                    Theme::accent_mint()
                                                } else {
                                                    rgba(0xffffff20)
                                                })
                                                .cursor_pointer()
                                                .px_1p5()
                                                .flex()
                                                .items_center()
                                                .overflow_hidden()
                                                .hover(|s| s.opacity(0.85))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.state.select_segment(seg_idx);
                                                    this.trigger_extract_frame(cx);
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .text_color(rgb(0xffffff))
                                                        .child(seg.display_text().to_string()),
                                                )
                                        })
                                    }),
                            ),
                    )
                    // 3. 贯穿全轨的交互响应层与播放游标指针 (Playhead / CTI & 点击/拖拽任意位置精确定位)
                    .child(
                        div()
                            .id("timeline-playhead-interactive-surface")
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(70.0)) // 避开左侧轨道标号
                            .right(px(16.0)) // 避开右侧边距
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                let win_w = window.viewport_size().width;
                                this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                            }))
                            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                                if event.pressed_button == Some(MouseButton::Left) {
                                    let win_w = window.viewport_size().width;
                                    this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                                }
                            }))
                            .on_mouse_up(MouseButton::Left, cx.listener(|this, event: &MouseUpEvent, window, cx| {
                                let win_w = window.viewport_size().width;
                                this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                            }))
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(progress_ratio))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .text_color(Theme::accent_mint())
                                            .child("▼"),
                                    )
                                    .child(
                                        div()
                                            .w(px(2.0))
                                            .flex_1()
                                            .bg(Theme::accent_mint()),
                                    ),
                            ),
                    ),
            )
    }

    /// 快捷跳转预设按钮 (iOS Segmented Pill)
    fn render_jump_btn(&mut self, label: &'static str, target_time: f64, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(label)
            .px_2p5()
            .py_0p5()
            .rounded_full()
            .cursor_pointer()
            .text_size(px(10.0))
            .text_color(Theme::text_secondary())
            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.seek_to(target_time);
                this.trigger_extract_frame(cx);
                cx.notify();
            }))
            .child(label)
    }

    /// 统一的时间轴鼠标点击与拖拽精确定位逻辑
    pub(crate) fn seek_by_mouse_x(&mut self, mouse_x: Pixels, window_width: Pixels, is_drag: bool, cx: &mut Context<Self>) {
        if self.state.total_duration <= 0.0 {
            return;
        }
        let left_pad = px(70.0); // 左侧轨道名称标签占用宽度
        let right_pad = px(16.0); // 右侧留白边距
        let track_w = window_width - left_pad - right_pad;
        if track_w <= px(10.0) {
            return;
        }
        let ratio = ((mouse_x - left_pad) / track_w).clamp(0.0, 1.0) as f64;
        let target_time = ratio * self.state.total_duration;
        self.state.seek_to(target_time);

        if is_drag {
            // 拖拽过程中节流：每 40ms（25帧极速高刷）发起一次单帧抽取请求，极致跟手零延迟
            if self.last_drag_extract.elapsed().as_millis() >= 40 {
                self.last_drag_extract = std::time::Instant::now();
                self.trigger_extract_frame(cx);
            }
        } else {
            // 单击或拖动松手：立刻发起抽取
            self.last_drag_extract = std::time::Instant::now();
            self.trigger_extract_frame(cx);
        }
        cx.notify();
    }

    /// 弹出原生 Windows 输入对话框进行字幕文本修改（完美支持搜狗/微软等中文输入法）
    pub(crate) fn prompt_edit_text(&mut self, cx: &mut Context<Self>) {
        let current_text = self.state.editing_text.clone();
        let prompt_title = "Voice2Word - 修改字幕文本";
        let prompt_msg = "请输入修改后的字幕内容（支持中文输入法/粘贴）：";

        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                use std::process::Command;
                let safe_msg = prompt_msg.replace('\'', "''");
                let safe_title = prompt_title.replace('\'', "''");
                let safe_default = current_text.replace('\'', "''");

                let script = format!(
                    "Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.Interaction]::InputBox('{}', '{}', '{}')",
                    safe_msg, safe_title, safe_default
                );

                let output = Command::new("powershell")
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(&script)
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !text.is_empty() {
                            Some(text)
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }).await;

            if let Some(new_text) = res {
                let _ = this.update(cx, |this, cx| {
                    this.state.editing_text = new_text;
                    this.state.save_selected_text();
                    cx.notify();
                });
            }
        }).detach();
    }
}
