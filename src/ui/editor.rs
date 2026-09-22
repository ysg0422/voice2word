//! 类似剪映 / Premiere 风格的多轨视频剪辑与字幕校对工作台
//! 提供：
//! 1. 顶部工作模式 Tab 切换 (智能生成 vs 剪辑校对)
//! 2. 视频画面同步监视器 (支持帧级预览与电影级字幕叠加)
//! 3. 字幕属性检查器 (支持实时修改错字、增删标点、时间微调、拆分与合并)
//! 4. 专业多轨时间轴 (时间刻度标尺、红/青色垂直指针游标、视频轨与字幕胶囊块)

use gpui::prelude::*;
use gpui::*;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use std::sync::Arc;
use crate::app::state::{WorkspaceTab, ProcessStatus};
use crate::utils::time::{format_duration_short, seconds_to_hms, seconds_to_timestamp};
use super::theme::Theme;
use super::{EditorExportFormat, MainWindow};

impl MainWindow {
    /// 渲染顶部模式切换栏 (Tab 导航器)
    #[allow(dead_code)]
    pub(crate) fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.state.active_tab;
        let _seg_count = self.state.segments.len();

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
                            .h(px(28.0))
                            .px_3p5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Editor {
                                rgb(0x2c2c36)
                            } else {
                                rgba(0x00000000)
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
                            .hover(move |s| {
                                if active != WorkspaceTab::Editor {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Editor;
                                this.trigger_extract_frame(cx);
                                cx.notify();
                            }))
                            .child("剪辑校对"),
                    )
                    .child(
                        div()
                            .id("tab-btn-generate")
                            .h(px(28.0))
                            .px_3p5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Generate {
                                rgb(0x2c2c36)
                            } else {
                                rgba(0x00000000)
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
                            .hover(move |s| {
                                if active != WorkspaceTab::Generate {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Generate;
                                cx.notify();
                            }))
                            .child("语音转写"),
                    )
                    .child(
                        div()
                            .id("tab-btn-library")
                            .h(px(28.0))
                            .px_3p5()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if active == WorkspaceTab::Library {
                                rgb(0x2c2c36)
                            } else {
                                rgba(0x00000000)
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
                            .hover(move |s| {
                                if active != WorkspaceTab::Library {
                                    s.bg(rgba(0xffffff0d)).text_color(Theme::text_primary())
                                } else {
                                    s
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Library;
                                this.state.refresh_recent_tasks();
                                cx.notify();
                            }))
                            .child("视频库"),
                    ),
            )
    }

    /// 渲染类似剪映 / Premiere 风格的剪辑工作区布局 (左中右之 中：视频与时间轴，右：字幕属性)
    pub(crate) fn render_editor_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });

        div()
            .id("editor-workspace-layout")
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            .child(
                if is_processing {
                    div()
                        .w_full()
                        .h(px(34.0))
                        .px_4()
                        .bg(rgba(0x10b98118))
                        .border_b_1()
                        .border_color(rgba(0x10b98133))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().w(px(7.0)).h(px(7.0)).rounded_full().bg(Theme::accent_mint()))
                                .child(
                                    div()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(Theme::accent_mint())
                                        .child("语音转写进行中，完成后自动同步到剪辑工作台"),
                                ),
                        )
                        .child(
                            div()
                                .id("switch-to-generator-banner-btn")
                                .cursor_pointer()
                                .px_3()
                                .py_0p5()
                                .rounded_full()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .text_size(px(11.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(Theme::text_primary())
                                .hover(|s| s.bg(Theme::bg_hover()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.active_tab = WorkspaceTab::Generate;
                                    cx.notify();
                                }))
                                .child("查看进度"),
                        )
                } else {
                    div()
                }
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    // 左侧：视频监视器 (二分之左 50%)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w(px(380.0))
                            .border_r_1()
                            .border_color(Theme::border())
                            .overflow_hidden()
                            .child(self.render_video_monitor(cx)),
                    )
                    // 右侧：字幕配置与多语言列表 (二分之右 50%)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w(px(460.0))
                            .overflow_hidden()
                            .child(self.render_subtitle_inspector(cx)),
                    ),
            )
            // 底部：时间轴 (全宽横跨)
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
            .w_full()
            .flex_1()
            .min_h(px(280.0))
            .bg(rgb(0x09090b))
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("视频预览"),
                            )
                            .child(
                                if is_playing {
                                    div()
                                        .px_2()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(rgba(0x10b98118))
                                        .border_1()
                                        .border_color(rgba(0x10b98144))
                                        .text_size(px(10.0))
                                        .text_color(Theme::accent_mint())
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("播放中")
                                } else {
                                    div()
                                }
                            ),
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
                            .child("独立窗口"),
                    ),
            )
            // 监视器视口屏幕：视频画面按真实宽高比等比适配居中（分辨率探测前回退 16:9）
            .child(
                div()
                    .id("monitor-viewport-screen")
                    .flex_1()
                    .w_full()
                    .min_h(px(180.0))
                    .relative()
                    .bg(rgb(0x050507))
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    // 画面容器 (stage)：与视频画面等比，是字幕覆盖层的定位基准，
                    // 保证字幕始终压在画面上而不是视口黑边里
                    .child({
                        let video_aspect = self.state.video_aspect();
                        let mut stage = div().relative().w_full().max_h_full();
                        stage.style().aspect_ratio = Some(video_aspect);
                        stage
                            // 视频画面展示 (实时内嵌播放 vs 静态时间轴帧)
                            .child(self.render_monitor_picture())
                            // 电影级高对比度字幕覆盖层：锚定画面底部（画面高度 5.5%，随画面等比缩放）
                            .child(
                                div()
                                    .absolute()
                                    .bottom(relative(0.055))
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .justify_center()
                                    .px_6()
                                    .child(
                                        if let Some(seg) = active_seg {
                                            div()
                                                // 限宽折行：超长句在画面内自动换行居中，绝不横向溢出画面
                                                .max_w_full()
                                                .min_w_0()
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
                                                .line_height(px(22.0))
                                                .child(seg.display_text().to_string())
                                        } else {
                                            div()
                                        }
                                    )
                            )
                    }),
            )
            // 监视器底部播放控制器与时间码显示
            .child(
                div()
                    .h(px(52.0))
                    .px_4()
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    // 左侧占位 (保证正中间对齐)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_start(),
                    )
                    // 中间：iOS 紧凑媒体控制条 (居中 + 放大主要播放按钮)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                div()
                                    .bg(rgb(0x16161c))
                                    .p(px(3.0))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(0x282832))
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    // 上一句
                                    .child(
                                        div()
                                            .id("ctrl-prev-seg")
                                            .px_3p5()
                                            .py_1p5()
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_size(px(11.5))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.jump_prev_segment(cx);
                                            }))
                                            .child("上句"),
                                    )
                                    // 快退 1 秒
                                    .child(
                                        div()
                                            .id("ctrl-step-back")
                                            .px_3()
                                            .py_1p5()
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_size(px(11.5))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.halt_preview_playback();
                                                let target = (this.state.current_time - 1.0).max(0.0);
                                                this.state.seek_to(target);
                                                this.trigger_extract_frame(cx);
                                                cx.notify();
                                            }))
                                            .child("-1s"),
                                    )
                                    // 实时播放 / 暂停 (高亮突出放大按钮)
                                    .child(
                                        div()
                                            .id("ctrl-play-pause")
                                            .px_6()
                                            .py_1p5()
                                            .rounded_full()
                                            .bg(if is_playing { Theme::accent_orange() } else { Theme::accent_mint() })
                                            .text_color(rgb(0x09090b))
                                            .font_weight(FontWeight::BOLD)
                                            .cursor_pointer()
                                            .text_size(px(13.0))
                                            .hover(|s| s.opacity(0.9))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_play_preview(cx);
                                            }))
                                            .child(if is_playing { "暂停" } else { "播放" }),
                                    )
                                    // 快进 1 秒
                                    .child(
                                        div()
                                            .id("ctrl-step-fwd")
                                            .px_3()
                                            .py_1p5()
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_size(px(11.5))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.halt_preview_playback();
                                                let target = this.state.current_time + 1.0;
                                                this.state.seek_to(target);
                                                this.trigger_extract_frame(cx);
                                                cx.notify();
                                            }))
                                            .child("+1s"),
                                    )
                                    // 下一句
                                    .child(
                                        div()
                                            .id("ctrl-next-seg")
                                            .px_3p5()
                                            .py_1p5()
                                            .rounded_full()
                                            .cursor_pointer()
                                            .text_size(px(11.5))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.jump_next_segment(cx);
                                            }))
                                            .child("下句"),
                                    ),
                            ),
                    )
                    // 右侧时间码 (iOS 胶囊卡片，右对齐)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_end()
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
                                            .child(seconds_to_hms(cur_time)),
                                    )
                                    .child(
                                        div()
                                            .text_color(Theme::text_muted())
                                            .child("/"),
                                    )
                                    .child(
                                        div()
                                            .text_color(Theme::text_secondary())
                                            .child(seconds_to_hms(tot_time)),
                                    ),
                            ),
                    ),
            )
    }

    /// 监视器画面内容：实时内嵌播放帧 vs 静态时间轴帧 vs 占位提示。
    /// 帧尺寸随视频真实宽高比动态变化，纹理构建与缓存均以本帧自带尺寸为准。
    fn render_monitor_picture(&mut self) -> Div {
        let live_frame = self.state.video_player.get_frame();
        let live_version = self.state.video_player.frame_version();

        if let Some(frame) = live_frame {
            // GPUI RenderImage 底层纹理严格要求 BGRA 格式（wgpu::TextureFormat::Bgra8Unorm），FFmpeg 已按 bgra 直出。
            // 按 (帧版本, 尺寸) 缓存 RenderImage：未变直接复用（零拷贝、零纹理重传），仅新帧到来时重建一次。
            let (frame_w, frame_h) = (frame.width, frame.height);
            let cached_hit = match self.cached_live_image.as_ref() {
                Some((version, width, height, cached_img))
                    if *version == live_version
                        && *width == frame_w
                        && *height == frame_h =>
                {
                    Some(cached_img.clone())
                }
                _ => None,
            };
            let render_img: Option<Arc<RenderImage>> = match cached_hit {
                Some(cached_img) => Some(cached_img),
                None => {
                    let data = match Arc::try_unwrap(frame) {
                        Ok(f) => f.data,
                        Err(arc) => arc.data.clone(),
                    };
                    match ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(frame_w, frame_h, data) {
                        Some(buffer) => {
                            let new_img = Arc::new(RenderImage::new(
                                SmallVec::from_elem(Frame::new(buffer), 1),
                            ));
                            self.cached_live_image =
                                Some((live_version, frame_w, frame_h, new_img.clone()));
                            Some(new_img)
                        }
                        None => None,
                    }
                }
            };
            if let Some(render_img) = render_img {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(img(render_img).size_full())
            } else {
                div().size_full()
            }
        } else if let Some(ref frame_path) = self.state.preview_frame_path {
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
                    .gap_1p5()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(Theme::text_muted())
                            .child("正在提取视频帧..."),
                    )
            }
        } else if self.state.selected_file.is_some() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_1p5()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(Theme::accent_mint())
                        .child("正在同步视频画面..."),
                )
        } else {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_1p5()
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(Theme::text_muted())
                        .child("拖动时间轴或点击播放，画面将在此实时呈现"),
                )
        }
    }

    /// 渲染字幕属性检查器与错别字编辑区 (Inspector)
    /// 渲染字幕属性检查器与多语言配置表格 (Inspector & Table)
    pub(crate) fn render_subtitle_inspector(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let sel_idx = self.state.selected_segment_index;
        let cur_seg = sel_idx.and_then(|idx| self.state.segments.iter().find(|s| s.index == idx)).cloned();
        let cur_text = self.state.editing_text.clone();
        let is_style_open = self.is_subtitle_style_open;

        div()
            .id("editor-subtitle-inspector")
            .flex_1()
            .min_w(px(460.0))
            .h_full()
            .bg(Theme::bg_sidebar())
            .flex()
            .flex_col()
            .overflow_hidden()
            // ── 顶部标头栏 ──
            .child(
                div()
                    .h(px(40.0))
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
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("字幕配置与多语言列表"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgb(0x181820))
                                    .border_1()
                                    .border_color(rgb(0x282832))
                                    .text_size(px(11.0))
                                    .text_color(Theme::text_secondary())
                                    .child(if let Some(idx) = sel_idx {
                                        format!("已选 #{}/共 {} 句", idx, self.state.segments.len())
                                    } else {
                                        format!("共 {} 句", self.state.segments.len())
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            // 样式排版设置切换开关
                            .child(
                                div()
                                    .id("btn-toggle-subtitle-style")
                                    .px_2p5()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(if is_style_open { rgba(0x6366f122) } else { Theme::bg_card() })
                                    .border_1()
                                    .border_color(if is_style_open { Theme::accent_primary() } else { Theme::border() })
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if is_style_open { Theme::accent_primary() } else { Theme::text_secondary() })
                                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.is_subtitle_style_open = !this.is_subtitle_style_open;
                                        cx.notify();
                                    }))
                                    .child(if is_style_open { "收起样式配置" } else { "样式排版配置" }),
                            ),
                    ),
            )
            // ── 属性面板主体 ──
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    // 1. 折叠式全局字幕样式与排版配置卡片
                    .child(
                        if is_style_open {
                            let cur_style = self.state.subtitle_style.clone();

                            div()
                                .p_3()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .gap_2p5()
                                // 卡片标题
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(11.5))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(Theme::text_muted())
                                                .child("全局字幕样式与排版"),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .text_color(Theme::accent_mint())
                                                .child(format!("当前: {}", cur_style.preset_name)),
                                        ),
                                )
                                // 1) 风格预设切牌
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_secondary())
                                                .child("视觉预设:"),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_1()
                                                .gap_1p5()
                                                .children([
                                                    ("白字黑影", "白字黑影"),
                                                    ("黄字黑边", "黄字黑边"),
                                                    ("半透明黑框", "半透明黑框"),
                                                    ("电影沉浸", "电影沉浸"),
                                                ].into_iter().enumerate().map(|(idx, (_id_name, label))| {
                                                    let is_sel = cur_style.preset_name == label;
                                                    div()
                                                        .id(("preset-btn", idx))
                                                        .flex_1()
                                                        .py_1()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(11.0))
                                                        .text_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { rgb(0x181820) })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { rgb(0x282832) })
                                                        .text_color(if is_sel { rgb(0x0a0a0f) } else { Theme::text_secondary() })
                                                        .hover(|s| if !is_sel { s.bg(Theme::bg_hover()).text_color(Theme::text_primary()) } else { s })
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.state.subtitle_style.preset_name = label.to_string();
                                                            cx.notify();
                                                        }))
                                                        .child(label)
                                                }))
                                        )
                                )
                                // 2) 参数快捷组 (字间距, 字号, 底边距, 单行字数)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_3()
                                        // 字号
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().text_size(px(10.5)).text_color(Theme::text_secondary()).child("字号:"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .flex()
                                                        .gap_1()
                                                        .children([18, 24, 28, 32].into_iter().map(|val| {
                                                            let is_sel = cur_style.font_size == val;
                                                            div()
                                                                .id(("font-sz", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(10.5))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { rgb(0x181820) })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { rgb(0x282832) })
                                                                .text_color(if is_sel { rgb(0x0a0a0f) } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.subtitle_style.font_size = val;
                                                                    cx.notify();
                                                                }))
                                                                .child(format!("{}", val))
                                                        }))
                                                )
                                        )
                                        // 字间距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().text_size(px(10.5)).text_color(Theme::text_secondary()).child("间距:"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .flex()
                                                        .gap_1()
                                                        .children([0, 1, 2, 4].into_iter().map(|val| {
                                                            let is_sel = cur_style.letter_spacing == val;
                                                            div()
                                                                .id(("letter-sp", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(10.5))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { rgb(0x181820) })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { rgb(0x282832) })
                                                                .text_color(if is_sel { rgb(0x0a0a0f) } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.subtitle_style.letter_spacing = val;
                                                                    cx.notify();
                                                                }))
                                                                .child(format!("{}px", val))
                                                        }))
                                                )
                                        )
                                        // 底边距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().text_size(px(10.5)).text_color(Theme::text_secondary()).child("底距:"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .flex()
                                                        .gap_1()
                                                        .children([20, 40, 60, 80].into_iter().map(|val| {
                                                            let is_sel = cur_style.bottom_margin == val;
                                                            div()
                                                                .id(("bot-mg", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(10.5))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { rgb(0x181820) })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { rgb(0x282832) })
                                                                .text_color(if is_sel { rgb(0x0a0a0f) } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.subtitle_style.bottom_margin = val;
                                                                    cx.notify();
                                                                }))
                                                                .child(format!("{}", val))
                                                        }))
                                                )
                                        )
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 2. 选中文段快速编辑与微调条 (选中时呈现)
                    .child(
                        if let Some(ref seg) = cur_seg {
                            let seg_idx = seg.index;
                            let start_ts = seconds_to_timestamp(seg.start);
                            let end_ts = seconds_to_timestamp(seg.end);
                            let dur = seg.duration();

                            div()
                                .p_3()
                                .rounded_xl()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .gap_2p5()
                                // 第一行：文段编号、时间范围与右侧主操作按钮组 (保存修改、弹窗编辑、删除)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .bg(rgba(0x10b98120))
                                                        .text_size(px(11.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child(format!("#{:03}", seg_idx)),
                                                )
                                                .child(
                                                    div()
                                                        .font_family("Consolas")
                                                        .text_size(px(11.5))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("{} - {} ({:.2}s)", start_ts, end_ts, dur)),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .flex_shrink_0()
                                                .child(
                                                    div()
                                                        .id("btn-open-prompt-edit")
                                                        .px_2p5()
                                                        .py_1()
                                                        .rounded_md()
                                                        .bg(rgb(0x181820))
                                                        .border_1()
                                                        .border_color(rgb(0x282832))
                                                        .cursor_pointer()
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.prompt_edit_text(cx);
                                                        }))
                                                        .child(
                                                            div()
                                                                .text_size(px(11.0))
                                                                .text_color(Theme::text_secondary())
                                                                .child("弹窗编辑"),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-save-text-top")
                                                        .px_3()
                                                        .py_1()
                                                        .rounded_md()
                                                        .bg(Theme::accent_mint())
                                                        .cursor_pointer()
                                                        .text_size(px(11.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(rgb(0x09090b))
                                                        .hover(|s| s.opacity(0.9))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }))
                                                        .child("保存修改"),
                                                )
                                                .child(
                                                    div()
                                                        .id("btn-del-seg")
                                                        .px_2p5()
                                                        .py_1()
                                                        .rounded_md()
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
                                                        .child("删除"),
                                                ),
                                        ),
                                )
                                // 第二行：全宽行内交互输入框 (宽敞易读，不拥挤)
                                .child(
                                    div()
                                        .w_full()
                                        .child({
                                            let is_focused = self.is_text_focused;
                                            let total_chars = cur_text.chars().count();
                                            let cursor_pos = self.text_cursor_pos.min(total_chars);

                                            div()
                                                .id("subtitle-text-editor-box")
                                                .w_full()
                                                .track_focus(&self.text_focus)
                                                .min_h(px(34.0))
                                                .px_3()
                                                .py_1()
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
                                                    let total_chars = this.state.editing_text.chars().count();
                                                    let cursor = this.text_cursor_pos.min(total_chars);

                                                    if event.keystroke.modifiers.control {
                                                        if key == "v" {
                                                            if let Some(item) = cx.read_from_clipboard() {
                                                                if let Some(text) = item.text() {
                                                                    let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                                    let insert_chars: Vec<char> = text.chars().collect();
                                                                    let ins_len = insert_chars.len();
                                                                    chars.splice(cursor..cursor, insert_chars);
                                                                    this.state.editing_text = chars.into_iter().collect();
                                                                    this.text_cursor_pos = cursor + ins_len;
                                                                    this.state.save_selected_text();
                                                                    cx.notify();
                                                                }
                                                            }
                                                        } else if key == "c" {
                                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(this.state.editing_text.clone()));
                                                            return;
                                                        } else if key == "a" {
                                                            this.text_cursor_pos = total_chars;
                                                            cx.notify();
                                                            return;
                                                        }
                                                        return;
                                                    }

                                                    if key == "backspace" {
                                                        if cursor > 0 && total_chars > 0 {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.remove(cursor - 1);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.text_cursor_pos = cursor - 1;
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    } else if key == "delete" {
                                                        if cursor < total_chars {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.remove(cursor);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    } else if key == "left" {
                                                        if cursor > 0 {
                                                            this.text_cursor_pos = cursor - 1;
                                                            cx.notify();
                                                        }
                                                    } else if key == "right" {
                                                        if cursor < total_chars {
                                                            this.text_cursor_pos = cursor + 1;
                                                            cx.notify();
                                                        }
                                                    } else if key == "home" {
                                                        this.text_cursor_pos = 0;
                                                        cx.notify();
                                                    } else if key == "end" {
                                                        this.text_cursor_pos = total_chars;
                                                        cx.notify();
                                                    } else if key == "enter" {
                                                        this.state.save_selected_text();
                                                        cx.notify();
                                                    } else if key.chars().count() == 1 {
                                                        let ch = key.chars().next().unwrap();
                                                        if !ch.is_control() {
                                                            let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                                                            chars.insert(cursor, ch);
                                                            this.state.editing_text = chars.into_iter().collect();
                                                            this.text_cursor_pos = cursor + 1;
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }
                                                    }
                                                }))
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .text_size(px(13.0))
                                                .text_color(Theme::text_primary())
                                                .child(if cur_text.is_empty() {
                                                    div().child(if is_focused { "▌".to_string() } else { "点击直接输入字幕...".to_string() })
                                                } else {
                                                    div()
                                                        .flex()
                                                        .flex_wrap()
                                                        .items_center()
                                                        .children(cur_text.chars().enumerate().map(|(idx, ch)| {
                                                            let show_cursor = is_focused && cursor_pos == idx;
                                                            let ch_str = ch.to_string();
                                                            div()
                                                                .id(("text-char", idx))
                                                                .cursor_text()
                                                                .flex()
                                                                .items_center()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                    window.focus(&this.text_focus);
                                                                    this.is_text_focused = true;
                                                                    this.text_cursor_pos = idx;
                                                                    cx.notify();
                                                                }))
                                                                .child(if show_cursor {
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .child(
                                                                            div()
                                                                                .text_color(Theme::accent_mint())
                                                                                .font_weight(FontWeight::BOLD)
                                                                                .child("▌"),
                                                                        )
                                                                        .child(ch_str)
                                                                } else {
                                                                    div().child(ch_str)
                                                                })
                                                        }))
                                                        .child(if is_focused && cursor_pos == total_chars {
                                                            div()
                                                                .cursor_text()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, window, cx| {
                                                                    window.focus(&this.text_focus);
                                                                    this.is_text_focused = true;
                                                                    this.text_cursor_pos = total_chars;
                                                                    cx.notify();
                                                                }))
                                                                .text_color(Theme::accent_mint())
                                                                .font_weight(FontWeight::BOLD)
                                                                .child("▌")
                                                        } else {
                                                            div()
                                                        })
                                                })
                                        })
                                )
                                // 第三行：时间微调按钮组与快捷标点注入 (紧凑规整，不遮挡)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .flex_wrap()
                                        .gap_2()
                                        // 左侧：时间微调
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .child("微调:"),
                                                )
                                                .child(
                                                    div()
                                                        .id("fine-tune-start-minus")
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(px(4.0))
                                                        .bg(rgb(0x1a1a24))
                                                        .border_1()
                                                        .border_color(rgb(0x282836))
                                                        .cursor_pointer()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(-0.1, 0.0);
                                                            cx.notify();
                                                        }))
                                                        .child("起-0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("fine-tune-start-plus")
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(px(4.0))
                                                        .bg(rgb(0x1a1a24))
                                                        .border_1()
                                                        .border_color(rgb(0x282836))
                                                        .cursor_pointer()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.1, 0.0);
                                                            cx.notify();
                                                        }))
                                                        .child("起+0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("fine-tune-end-minus")
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(px(4.0))
                                                        .bg(rgb(0x1a1a24))
                                                        .border_1()
                                                        .border_color(rgb(0x282836))
                                                        .cursor_pointer()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, -0.1);
                                                            cx.notify();
                                                        }))
                                                        .child("止-0.1s"),
                                                )
                                                .child(
                                                    div()
                                                        .id("fine-tune-end-plus")
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded(px(4.0))
                                                        .bg(rgb(0x1a1a24))
                                                        .border_1()
                                                        .border_color(rgb(0x282836))
                                                        .cursor_pointer()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.adjust_selected_times(0.0, 0.1);
                                                            cx.notify();
                                                        }))
                                                        .child("止+0.1s"),
                                                ),
                                        )
                                        // 右侧：快捷标点注入
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(10.5))
                                                        .text_color(Theme::text_muted())
                                                        .child("标点:"),
                                                )
                                                .child(self.render_punct_btn("，", cx))
                                                .child(self.render_punct_btn("。", cx))
                                                .child(self.render_punct_btn("？", cx))
                                                .child(self.render_punct_btn("！", cx))
                                                .child(self.render_punct_btn("、", cx)),
                                        ),
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 3. 多语言字幕配置与对照大表格 (图二风格)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(200.0))
                            .rounded_xl()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(Theme::border())
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            // 表头 (图二标准规格)
                            .child(
                                div()
                                    .w_full()
                                    .h(px(36.0))
                                    .bg(rgb(0x16161d))
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .px_3()
                                    .gap_3()
                                    // 序号
                                    .child(
                                        div()
                                            .w(px(36.0))
                                            .text_center()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("#"),
                                    )
                                    // 开始时间
                                    .child(
                                        div()
                                            .w(px(92.0))
                                            .text_center()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("开始时间"),
                                    )
                                    // 结束时间
                                    .child(
                                        div()
                                            .w(px(92.0))
                                            .text_center()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("结束时间"),
                                    )
                                    // 字幕内容
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(110.0))
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("字幕内容"),
                                    )
                                    // 翻译字幕
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(110.0))
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("翻译字幕"),
                                    ),
                            )
                            // 表格行内容：uniform_list 虚拟化渲染，千行级字幕清单仅构建可视行，
                            // 长视频下不再整表全量布局（原先 1400+ 行全部渲染导致滚动掉帧）
                            .child({
                                // 选中行变化时自动滚动跟随（时间轴点击 / 上下句跳转 / 播放联动）
                                if self.subtitle_list_followed_sel != sel_idx {
                                    if let Some(idx) = sel_idx {
                                        if let Some(pos) = self.state.segments.iter().position(|s| s.index == idx) {
                                            self.subtitle_list_scroll.scroll_to_item(pos, ScrollStrategy::Top);
                                        }
                                    }
                                    self.subtitle_list_followed_sel = sel_idx;
                                }

                                let row_count = self.state.segments.len();
                                uniform_list(
                                    "inspector-segments-virtual",
                                    row_count,
                                    cx.processor(move |this, visible_range: std::ops::Range<usize>, _window, cx| {
                                        let sel = this.state.selected_segment_index;
                                        let cur_time = this.state.current_time;
                                        visible_range
                                            .filter_map(|i| this.state.segments.get(i).map(|seg| (i, seg)))
                                            .map(|(_i, seg)| {
                                                let seg_idx = seg.index;
                                                let is_selected = sel == Some(seg_idx);
                                                let is_playing_here = cur_time >= seg.start && cur_time <= seg.end;
                                                let start_ts = seconds_to_timestamp(seg.start);
                                                let end_ts = seconds_to_timestamp(seg.end);
                                                let raw_text = seg.text.clone();
                                                let trans_text = seg.translation.as_deref().unwrap_or("—").to_string();

                                                div()
                                                    .id(("table-row-seg", seg_idx))
                                                    .w_full()
                                                    .h(px(40.0))
                                                    .px_3()
                                                    .border_b_1()
                                                    .border_color(rgb(0x1e1e26))
                                                    .cursor_pointer()
                                                    .bg(if is_selected {
                                                        rgba(0x10b9811c)
                                                    } else if is_playing_here {
                                                        rgba(0x6366f116)
                                                    } else {
                                                        rgba(0x00000000)
                                                    })
                                                    .hover(|s| s.bg(if is_selected { rgba(0x10b98128) } else { Theme::bg_hover() }))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.state.select_segment(seg_idx);
                                                        this.trigger_extract_frame(cx);
                                                        cx.notify();
                                                    }))
                                                    .flex()
                                                    .items_center()
                                                    .gap_3()
                                                    // 序号
                                                    .child(
                                                        div()
                                                            .w(px(36.0))
                                                            .text_center()
                                                            .text_size(px(12.5))
                                                            .font_weight(if is_selected { FontWeight::BOLD } else { FontWeight::NORMAL })
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_muted() })
                                                            .child(format!("{}", seg_idx)),
                                                    )
                                                    // 开始时间 (图二高精时间戳)
                                                    .child(
                                                        div()
                                                            .w(px(92.0))
                                                            .text_center()
                                                            .font_family("Consolas")
                                                            .text_size(px(12.0))
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_secondary() })
                                                            .child(start_ts),
                                                    )
                                                    // 结束时间 (图二高精时间戳)
                                                    .child(
                                                        div()
                                                            .w(px(92.0))
                                                            .text_center()
                                                            .font_family("Consolas")
                                                            .text_size(px(12.0))
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_secondary() })
                                                            .child(end_ts),
                                                    )
                                                    // 字幕内容 (原文，清晰中文字体；单行截断保证虚拟列表行高一致)
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w(px(110.0))
                                                            .text_size(px(13.0))
                                                            .font_weight(if is_selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                                            .text_color(if is_selected { Theme::text_primary() } else { rgb(0xe2e8f0) })
                                                            .truncate()
                                                            .child(raw_text),
                                                    )
                                                    // 翻译字幕 (多语言对照)
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w(px(110.0))
                                                            .text_size(px(12.5))
                                                            .text_color(if is_selected {
                                                                rgb(0xd1d5db)
                                                            } else if trans_text == "—" {
                                                                Theme::text_muted()
                                                            } else {
                                                                Theme::text_secondary()
                                                            })
                                                            .truncate()
                                                            .child(trans_text),
                                                    )
                                            })
                                            .collect()
                                    }),
                                )
                                .track_scroll(self.subtitle_list_scroll.clone())
                                .flex_1()
                                .w_full()
                            }),
                    )
            )
            // ── 底部固定：统一导出控制底栏 ──
            .child(self.render_editor_export_dock(cx))
    }

    /// 渲染剪辑工作台右侧底部的统一导出控制底栏
    pub(crate) fn render_editor_export_dock(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.is_export_dropdown_open;
        let cur_fmt = self.editor_export_format;

        div()
            .id("editor-export-dock")
            .w_full()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                if is_open {
                    div()
                        .id("export-format-menu")
                        .rounded_lg()
                        .bg(Theme::bg_card())
                        .border_1()
                        .border_color(Theme::border())
                        .p_1()
                        .max_h(px(200.0))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .children(EditorExportFormat::all().iter().enumerate().map(|(idx, fmt)| {
                            let fmt = *fmt;
                            let is_selected = fmt == cur_fmt;
                            div()
                                .id(("export-fmt-opt", idx))
                                .px_2p5()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(if is_selected {
                                    rgba(0x10b98120)
                                } else {
                                    rgba(0x00000000)
                                })
                                .hover(|s| s.bg(Theme::bg_hover()))
                                .flex()
                                .items_center()
                                .justify_between()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.editor_export_format = fmt;
                                    this.is_export_dropdown_open = false;
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .font_weight(if is_selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                        .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_primary() })
                                        .child(fmt.label()),
                                )
                                .child(
                                    if is_selected {
                                        div()
                                            .text_size(px(10.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child("[当前]")
                                    } else {
                                        div()
                                    }
                                )
                        }))
                } else {
                    div().id("export-format-menu-closed")
                }
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .id("export-format-dropdown-trigger")
                            .flex_1()
                            .h(px(38.0))
                            .px_3p5()
                            .rounded_xl()
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(if is_open { Theme::accent_mint() } else { Theme::border() })
                            .cursor_pointer()
                            .hover(|s| s.border_color(Theme::accent_mint()))
                            .flex()
                            .items_center()
                            .justify_between()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.is_export_dropdown_open = !this.is_export_dropdown_open;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(cur_fmt.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(Theme::text_secondary())
                                    .child(if is_open { "▲" } else { "▼" }),
                            ),
                    )
                    .child(
                        div()
                            .id("editor-do-export-btn")
                            .h(px(38.0))
                            .px_6()
                            .rounded_xl()
                            .bg(Theme::accent_mint())
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .flex()
                            .items_center()
                            .justify_center()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.is_export_dropdown_open = false;
                                this.perform_editor_export(cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0x09090b))
                                    .child("导出"),
                            ),
                    ),
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
                let total_chars = this.state.editing_text.chars().count();
                let cursor = this.text_cursor_pos.min(total_chars);
                let mut chars: Vec<char> = this.state.editing_text.chars().collect();
                let punct_chars: Vec<char> = punct.chars().collect();
                let p_len = punct_chars.len();
                chars.splice(cursor..cursor, punct_chars);
                this.state.editing_text = chars.into_iter().collect();
                this.text_cursor_pos = cursor + p_len;
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
            .h(px(98.0))
            .flex_shrink_0()
            .w_full()
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 时间轴顶栏工具区 (紧凑 28px)
            .child(
                div()
                    .h(px(28.0))
                    .px_3()
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
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("时间轴"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(rgb(0x181820))
                                    .border_1()
                                    .border_color(rgb(0x282832))
                                    .text_size(px(10.5))
                                    .font_family("Consolas")
                                    .text_color(Theme::accent_mint())
                                    .font_weight(FontWeight::BOLD)
                                    .child(seconds_to_hms(cur_time)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(Theme::text_muted())
                                    .child(format!("共 {} 句 · 总长 {}", self.state.segments.len(), format_duration_short(total_dur))),
                            ),
                    ),
            )
            // 1. 统一主字幕轨道 (紧凑苗条单轨布局，高 34px)
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .relative()
                    .flex()
                    .items_center()
                    .py_1()
                    .child(
                        div()
                            .h_full()
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
                                    .child("字幕轨"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(34.0))
                                    .mr_4()
                                    .rounded_lg()
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
                                    // 字幕片段渲染 (紧凑纤细卡片，高度 28px)
                                    .children({
                                        let relevant_segments: Vec<_> = if self.state.segments.len() <= 20 {
                                            self.state.segments.iter().collect()
                                        } else {
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
                                                .top(px(2.0))
                                                .bottom(px(2.0))
                                                .left(relative(start_r))
                                                .w(relative(width_r))
                                                .min_w(px(40.0))
                                                .rounded(px(4.0))
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
                                                        .text_size(px(10.5))
                                                        .text_color(rgb(0xffffff))
                                                        .child(seg.display_text().to_string()),
                                                )
                                        })
                                    }),
                            ),
                    )
                    // 贯穿轨道的交互响应层与播放游标指针
                    .child(
                        div()
                            .id("timeline-playhead-interactive-surface")
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(70.0))
                            .right(px(16.0))
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
                                    .ml(px(-6.0))
                                    .w(px(12.0))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(9.5))
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
            // 2. 探底时间刻度标尺 (Time Ruler 移至底部展示，精简 20px)
            .child(
                div()
                    .id("timeline-time-ruler")
                    .h(px(20.0))
                    .w_full()
                    .bg(rgb(0x131316))
                    .border_t_1()
                    .border_color(Theme::border())
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
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(70.0))
                            .right(px(16.0))
                            .children((0usize..=10).map(|i| {
                                let ratio = i as f64 / 10.0;
                                let t = total_dur * ratio;
                                div()
                                    .id(("ruler-tick-btn", i))
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(ratio as f32))
                                    .ml(px(-16.0))
                                    .w(px(32.0))
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
                    ),
            )
    }

    /// 快捷跳转预设按钮 (iOS Segmented Pill)
    #[allow(dead_code)]
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
                this.halt_preview_playback();
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
        self.halt_preview_playback();
        let nav_w = px(180.0); // 左侧导航侧边栏占用宽度
        let left_pad = px(70.0); // 左侧轨道名称标签占用宽度 ("字幕轨")
        let right_pad = px(16.0); // 右侧留白边距 (mr_4)

        let track_start_x = nav_w + left_pad;
        let track_w = window_width - nav_w - left_pad - right_pad;
        if track_w <= px(10.0) {
            return;
        }
        let relative_x = mouse_x - track_start_x;
        let ratio = (relative_x / track_w).clamp(0.0, 1.0) as f64;
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
    #[allow(dead_code)]
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
