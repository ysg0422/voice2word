//! 类似剪映 / Premiere 风格的多轨视频剪辑与字幕校对工作台
//! 提供：
//! 1. 顶部工作模式 Tab 切换 (智能生成 vs 剪辑校对)
//! 2. 视频画面同步监视器 (支持帧级预览与电影级字幕叠加)
//! 3. 字幕属性检查器 (支持实时修改错字、增删标点、时间微调、拆分与合并)
//! 4. 专业多轨时间轴 (时间刻度标尺、红/青色垂直指针游标、视频轨与字幕胶囊块)

use super::actions::translated_out_count;
use super::primitives;
use super::theme::Theme;
use super::{apply_line_edit, EditorExportFormat, EditorSubtitlePanel, MainWindow, StyleField};
use crate::app::state::{ProcessStatus, WorkspaceTab};
use crate::subtitle::ExportMode;
use crate::utils::time::{format_duration_short, seconds_to_hms, seconds_to_timestamp_short};
use gpui::prelude::*;
use gpui::*;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use std::sync::Arc;

/// 字幕配置 → 预览配色 `(文字色, 底色, 描边色)`。
///
/// 完全由 `SubtitleStyleConfig` 驱动，支持自定义文字颜色、描边粗细与底框不透明度。
fn subtitle_computed_colors(style: &crate::utils::SubtitleStyleConfig) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&style.primary_color);
    let fg = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));

    let (or, og, ob) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&style.outline_color);
    let border = if style.outline_width > 0.0 {
        rgb((or as u32) << 16 | (og as u32) << 8 | (ob as u32))
    } else {
        Theme::transparent()
    };

    let bg = match style.bg_style.as_str() {
        "none" => Theme::transparent(),
        "box" => {
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 255.0) as u32).clamp(0, 255);
            rgba(a)
        }
        "pill" => {
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 230.0) as u32).clamp(0, 255);
            rgba(0x18181b00 | a)
        }
        _ => {
            // "shadow" 默认投影底色
            let a = ((style.bg_opacity.clamp(0.0, 1.0) * 160.0) as u32).clamp(0, 255);
            rgba(a)
        }
    };

    (fg, bg, border)
}

#[allow(dead_code)]
fn subtitle_preset_colors(preset: &str) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    let mut cfg = crate::utils::SubtitleStyleConfig::default();
    cfg.apply_preset(preset);
    subtitle_computed_colors(&cfg)
}

/// 样式预览条的底色修正。
///
/// 预览条里没有真实视频画面，浅色主题下衬底是浅灰；而「电影沉浸」预设本身不带底框、
/// 字形是恒白——白字压浅灰等于看不见。这里在**预览条**这个特殊语境下补一层深色
/// 底衬，只为让预览可读；监视器里压在真实画面上的字幕不受影响，仍走原预设。
fn preview_backdrop(bg: gpui::Rgba) -> gpui::Rgba {
    if Theme::is_light() && bg == Theme::transparent() {
        Theme::bg_overlay()
    } else {
        bg
    }
}

/// HSV(0..360, 0..1, 0..1) 转 RGB(0..255)
pub(crate) fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let h_prime = ((h % 360.0) + 360.0) % 360.0 / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if (0.0..1.0).contains(&h_prime) {
        (c, x, 0.0)
    } else if (1.0..2.0).contains(&h_prime) {
        (x, c, 0.0)
    } else if (2.0..3.0).contains(&h_prime) {
        (0.0, c, x)
    } else if (3.0..4.0).contains(&h_prime) {
        (0.0, x, c)
    } else if (4.0..5.0).contains(&h_prime) {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

/// 字幕样式可选值（字号 / 底边距以 1080p 为基准，与 `SubtitleStyleConfig` 语义一致）
#[allow(dead_code)]
const STYLE_FONT_SIZES: [u32; 5] = [28, 36, 44, 52, 60];
#[allow(dead_code)]
const STYLE_LETTER_SPACINGS: [u32; 5] = [0, 1, 2, 4, 6];
#[allow(dead_code)]
const STYLE_LINE_SPACINGS: [f32; 5] = [1.0, 1.2, 1.4, 1.6, 1.8];
#[allow(dead_code)]
const STYLE_MAX_CHARS: [u32; 5] = [10, 14, 18, 22, 26];
#[allow(dead_code)]
const STYLE_BOTTOM_MARGINS: [u32; 5] = [20, 40, 60, 80, 120];
/// 分段按钮组的宽度上界：4 个短标签等分后每个约 117px，够放下 2~4 个字，
/// 又不会像铺满整行那样被拉伸成 200px 的空条。用 max_w 而非固定宽，
/// 是为了窄面板下仍能随容器收缩，避免「标签 + 按钮组」溢出卡片。
#[allow(dead_code)]
const STYLE_ROW_BTN_MAX_W: f32 = 480.0;

/// 左右二分布局下左侧视频监视器的最小宽度（逻辑 px）
const EDITOR_LEFT_MIN_W: f32 = 300.0;
/// 右侧字幕配置与列表区域的固定面板宽度（逻辑 px，类似 B 站 / YouTube 侧边栏，视频占绝大多数区域）
const EDITOR_RIGHT_PANEL_W: f32 = 400.0;

/// 窗口逻辑宽度低于此值时，「视频监视器 | 字幕配置」由左右二分改为上下堆叠。
/// 导航栏已上移至顶部，可用宽度不再减 180px。
const EDITOR_STACK_BELOW_W: f32 = 820.0;

/// 上下堆叠布局下视频监视器固定占用的高度（监视器自身最少需要 36+180+52=268）。
const EDITOR_STACK_MONITOR_H: f32 = 320.0;

/// 上下并列字幕行高度（双语模式：时间 + 原文 + 译文）
const SUBTITLE_ROW_H_BILINGUAL: f32 = 82.0;
/// 上下并列字幕行高度（单语模式：时间 + 原文）
const SUBTITLE_ROW_H_MONO: f32 = 56.0;

/// 说话人标签的配色：4 个说话人各占一色，超过则回落到第一色循环。
/// 只在标签本身着色（不染整行），避免与「选中行」的高亮底色互相干扰。
fn speaker_color(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::accent_mint(),
        1 => Theme::accent_blue(),
        2 => Theme::accent_orange(),
        _ => Theme::accent_primary(),
    }
}

fn speaker_tint(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::tint_mint_soft(),
        1 => Theme::tint_blue_soft(),
        2 => Theme::tint_red_soft(),
        _ => Theme::tint_primary_soft(),
    }
}

fn speaker_border(speaker: u32) -> gpui::Rgba {
    match speaker % 4 {
        0 => Theme::tint_mint_border(),
        1 => Theme::tint_blue_border(),
        2 => Theme::tint_red_border(),
        _ => Theme::tint_primary_border(),
    }
}

impl MainWindow {
    /// 渲染类似剪映 / Premiere 风格的剪辑工作区布局 (左中右之 中：视频与时间轴，右：字幕属性)
    pub(crate) fn render_editor_layout(
        &mut self,
        viewport_w: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        // 布局随窗口宽度二选一，见 `EDITOR_STACK_BELOW_W` 的说明
        let stacked = viewport_w < EDITOR_STACK_BELOW_W;

        div()
            .id("editor-workspace-layout")
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            .child(if is_processing {
                div()
                    .w_full()
                    .h(px(Theme::BANNER_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::tint_mint_soft())
                    .border_b_1()
                    .border_color(Theme::tint_mint_border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(primitives::stat_dot_sm(Theme::accent_mint()))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::accent_mint())
                                    .child("语音转写进行中，完成后自动同步到剪辑工作台"),
                            ),
                    )
                    .child(
                        // 标头小胶囊走 primitives::btn_sm_outline
                        primitives::btn_sm_outline("查看进度")
                            .id("switch-to-generator-banner-btn")
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .hover(|s| s.bg(Theme::bg_hover()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.active_tab = WorkspaceTab::Generate;
                                cx.notify();
                            })),
                    )
            } else {
                div()
            })
            .child(if stacked {
                // 窄窗口降级：视频监视器在上、字幕配置在下，两列各占整行宽度
                div()
                    .id("editor-main-stacked")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        div()
                            .w_full()
                            .flex_none()
                            .h(px(EDITOR_STACK_MONITOR_H))
                            .border_b_1()
                            .border_color(Theme::border())
                            .overflow_hidden()
                            .child(self.render_video_monitor(cx)),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_subtitle_inspector(true, cx)),
                    )
            } else {
                let right_w = self
                    .editor_split_w
                    .unwrap_or(EDITOR_RIGHT_PANEL_W)
                    .clamp(360.0, (viewport_w * 0.5).max(360.0));
                let is_dragging = self.editor_split_drag.is_some();

                div()
                    .id("editor-main-split")
                    .flex()
                    .flex_row()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if event.pressed_button != Some(MouseButton::Left) {
                            return;
                        }
                        if let Some((start_w, start_x)) = this.editor_split_drag {
                            let max_w = (viewport_w * 0.5).max(360.0);
                            let dx = f32::from(event.position.x) - start_x;
                            let new_w = (start_w - dx).clamp(360.0, max_w);
                            this.editor_split_w = Some(new_w);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.editor_split_drag = None;
                            cx.notify();
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.editor_split_drag = None;
                            cx.notify();
                        }),
                    )
                    // 左侧：视频监视器 (占主要空间，类似 B 站 / YouTube 播放页)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .h_full()
                            .min_w(px(EDITOR_LEFT_MIN_W))
                            .overflow_hidden()
                            .child(self.render_video_monitor(cx)),
                    )
                    // 中间：可自由拖动的分隔条（宽度不超过窗口的 50%）
                    .child(
                        div()
                            .id("editor-split-divider")
                            .w(px(5.0))
                            .h_full()
                            .bg(if is_dragging {
                                Theme::accent_mint()
                            } else {
                                Theme::border()
                            })
                            .cursor_col_resize()
                            .hover(|s| s.bg(Theme::accent_mint()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    this.editor_split_drag =
                                        Some((right_w, f32::from(event.position.x)));
                                    cx.notify();
                                }),
                            ),
                    )
                    // 右侧：字幕配置与列表区域 (紧凑侧边面板，可拖动调节宽度，上限 50%)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(right_w))
                            .h_full()
                            .overflow_hidden()
                            .child(self.render_subtitle_inspector(false, cx)),
                    )
            })
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
            .min_h(px(Theme::MONITOR_MIN_H))
            .bg(Theme::bg_media())
            .flex()
            .flex_col()
            .overflow_hidden()
            // 监视器标头
            .child(
                div()
                    .h(px(Theme::HEADER_H_SM))
                    .px(px(Theme::PAGE_PAD))
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
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("视频预览"),
                            )
                            .child(if is_playing {
                                // 瞬时状态胶囊走 primitives::tag_tinted
                                primitives::tag_tinted(
                                    "播放中",
                                    Theme::tint_mint_soft(),
                                    Theme::tint_mint_border(),
                                    Theme::accent_mint(),
                                )
                            } else {
                                div()
                            }),
                    )
                    .child(
                        // 标头小胶囊走 primitives::btn_sm_outline
                        primitives::btn_sm_outline("独立窗口")
                            .id("monitor-ffplay-btn")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.play_video(cx);
                            })),
                    ),
            )
            // 监视器视口屏幕：视频画面按真实宽高比等比适配居中（分辨率探测前回退 16:9）
            .child(
                div()
                    .id("monitor-viewport-screen")
                    .flex_1()
                    .w_full()
                    .min_h(px(Theme::VIEWPORT_MIN_H))
                    .relative()
                    .bg(Theme::bg_media_deep())
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
                            // 字幕覆盖层：完全由 config.subtitle_style 驱动，
                            // 主界面改样式即时反映在这里（预览按 PREVIEW_SCALE 缩放显示）
                            .child({
                                let style = self.state.config.subtitle_style.clone();
                                let font_px = style.preview_font_px();
                                let (fg, bg, border) = subtitle_computed_colors(&style);
                                // 宽度与编辑卡里的预览框同源：拖把手时两者在同一帧一起变，
                                // 不存在「预览改了、画面上没跟上」的延迟
                                let box_w = self.subtitle_box_w();
                                let export_mode = self.editor_export_mode;
                                div()
                                    .absolute()
                                    .bottom(relative(style.preview_bottom_ratio()))
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .justify_center()
                                    .px_6()
                                    .child(if let Some(seg) = active_seg {
                                        div()
                                            // 固定宽度（不是 max_w）：拖动把手时画面上的
                                            // 字幕框要跟着同宽变化，而不是只有超长句才受影响。
                                            // 短句在框内居中，长句按此宽度折行——与编辑卡的
                                            // 预览框是同一个宽度值，两者同步增减。
                                            .w(px(box_w))
                                            .min_w_0()
                                            .px_3()
                                            .py_1()
                                            .rounded_md()
                                            .bg(bg)
                                            .border_1()
                                            .border_color(border)
                                            .text_size(px(font_px))
                                            .font_weight(if style.is_bold { FontWeight::BOLD } else { FontWeight::NORMAL })
                                            .text_color(fg)
                                            .text_align(match style.alignment.as_str() {
                                                "left" => TextAlign::Left,
                                                "right" => TextAlign::Right,
                                                _ => TextAlign::Center,
                                            })
                                            .line_height(px(font_px * style.line_spacing))
                                            // 预览与导出同源：按当前导出内容模式渲染，
                                            // 译好之后在监视器里就能直接看到译文/双语。
                                            .child(seg.export_text(export_mode))
                                    } else {
                                        div()
                                    })
                            })
                    }),
            )
            // 监视器底部播放控制器与时间码显示
            .child(
                div()
                    .h(px(Theme::CONTROL_BAR_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    // 左侧占位 (保证正中间对齐)
                    .child(div().flex_1().flex().items_center().justify_start())
                    // 中间：iOS 紧凑媒体控制条 (居中 + 放大主要播放按钮)
                    .child(
                        div().flex().items_center().justify_center().child(
                            div()
                                .bg(Theme::bg_input())
                                .p(px(Theme::CONTROL_INSET))
                                .rounded_full()
                                .border_1()
                                .border_color(Theme::border_mid())
                                .flex()
                                .items_center()
                                .gap(px(Theme::CTRL_GAP_TIGHT))
                                // 上一句
                                .child(primitives::pill_btn("上句").id("ctrl-prev-seg").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.jump_prev_segment(cx);
                                    }),
                                ))
                                // 快退 1 秒
                                .child(primitives::pill_btn("-1s").id("ctrl-step-back").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.halt_preview_playback();
                                        let target = (this.state.current_time - 1.0).max(0.0);
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }),
                                ))
                                // 实时播放 / 暂停 (高亮突出放大按钮)
                                .child(
                                    primitives::pill_btn_solid(
                                        if is_playing { "暂停" } else { "播放" },
                                        if is_playing {
                                            Theme::accent_orange()
                                        } else {
                                            Theme::accent_mint()
                                        },
                                    )
                                    .id("ctrl-play-pause")
                                    .px(px(Theme::SPACE_6))
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.toggle_play_preview(cx);
                                        },
                                    )),
                                )
                                // 快进 1 秒
                                .child(primitives::pill_btn("+1s").id("ctrl-step-fwd").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.halt_preview_playback();
                                        let target = this.state.current_time + 1.0;
                                        this.state.seek_to(target);
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }),
                                ))
                                // 下一句
                                .child(primitives::pill_btn("下句").id("ctrl-next-seg").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.jump_next_segment(cx);
                                    }),
                                )),
                        ),
                    )
                    // 右侧时间码 (iOS 胶囊卡片，右对齐)
                    .child(
                        div().flex_1().flex().items_center().justify_end().child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded_full()
                                .bg(Theme::bg_input())
                                .border_1()
                                .border_color(Theme::border_mid())
                                .flex()
                                .items_center()
                                .gap_1()
                                .font_family("Consolas")
                                .text_size(px(Theme::TEXT_SMALL))
                                .child(
                                    div()
                                        .text_color(Theme::accent_mint())
                                        .font_weight(FontWeight::BOLD)
                                        .child(seconds_to_hms(cur_time)),
                                )
                                .child(div().text_color(Theme::text_muted()).child("/"))
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
                    if *version == live_version && *width == frame_w && *height == frame_h =>
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
                            let new_img = Arc::new(RenderImage::new(SmallVec::from_elem(
                                Frame::new(buffer),
                                1,
                            )));
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
            // `preview_frame_path` 只在**确认文件存在**时被写入（见 actions.rs 的帧抽取
            // 回写点），因此这里不再逐帧 `exists()`。空 `Some` 不会出现。
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(img(frame_path.clone()).size_full())
        } else if self.state.selected_file.is_some() {
            div().flex().flex_col().items_center().gap_1p5().child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(Theme::accent_on_media())
                    .child("正在同步视频画面..."),
            )
        } else {
            div().flex().flex_col().items_center().gap_1p5().child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    // 监视器视口在任何主题下都是深底，空态提示也在深底上，
                    // 必须用媒体区 token；用 text_muted 会在浅色主题下变成深字压深底
                    .text_color(Theme::text_on_media())
                    .child("拖动时间轴或点击播放，画面将在此实时呈现"),
            )
        }
    }

    /// 渲染字幕属性检查器与错别字编辑区 (Inspector)
    /// 渲染字幕属性检查器与多语言配置表格 (Inspector & Table)
    pub(crate) fn render_subtitle_inspector(
        &mut self,
        stacked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 搜索命中的行下标按需重算（键不变时零成本复用），虚拟列表据此渲染
        self.refresh_subtitle_filter();
        let sel_idx = self.state.selected_segment_index;
        let cur_seg = sel_idx
            .and_then(|idx| self.state.segments.iter().find(|s| s.index == idx))
            .cloned();
        let cur_text = self.state.editing_text.clone();
        let panel = self.subtitle_panel;

        div()
            .id("editor-subtitle-inspector")
            .flex_1()
            // 宽度约束交给外层列容器（见 `EDITOR_RIGHT_MIN_W`）；这里显式允许收缩，
            // 因为列内每一行都已按「先收缩、再折行」处理，不需要再撑住一个固定下限。
            .min_w_0()
            .h_full()
            .bg(Theme::bg_sidebar())
            .flex()
            .flex_col()
            .overflow_hidden()
            // ── 顶部标头栏 ──
            .child(
                div()
                    .h(px(Theme::HEADER_H))
                    .px(px(Theme::PAGE_PAD))
                    .bg(Theme::bg_sidebar())
                    .border_b_1()
                    .border_color(Theme::border())
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        primitives::panel_title("字幕面板"),
                    )
                    .child(
                        // 两张面板互斥切换：顶部一对分段选项代替原先的「展开 / 收起」开关。
                        // 面板高度有限，样式卡与翻译卡纵向堆叠时必须滚动才能看到下面那张，
                        // 拆成互斥视图后一次只占一份版面。
                        primitives::segmented_cluster()
                            // 样式面板：全局字幕样式与排版
                            .child(
                                primitives::segmented(
                                    "字幕样式",
                                    panel == EditorSubtitlePanel::Style,
                                    false,
                                )
                                .id("btn-panel-subtitle-style")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Style;
                                    cx.notify();
                                })),
                            )
                            // 翻译面板：多语言翻译与进度
                            .child(
                                primitives::segmented(
                                    "字幕翻译",
                                    panel == EditorSubtitlePanel::Translate,
                                    false,
                                )
                                .id("btn-panel-subtitle-translate")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Translate;
                                    cx.notify();
                                })),
                            )
                            // 统计与导出面板：整篇的时长 / 字数 / 语速 / 过长句，
                            // 以及导出内容 / 文件名模板 / 格式选择。这些数字此前无处可看
                            // （只能自己数），而 CPS 与「过长句」正是字幕交付会被打回的硬指标；
                            // 导出又正是这份字幕最后的交付动作，两者合并同屏最贴合。
                            .child(
                                primitives::segmented(
                                    "统计与导出",
                                    panel == EditorSubtitlePanel::Stats,
                                    false,
                                )
                                .id("btn-panel-subtitle-stats")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.subtitle_panel = EditorSubtitlePanel::Stats;
                                    cx.notify();
                                })),
                            ),
                    ),
            )
            // ── 属性面板主体 ──
            .child(
                div()
                    .id("editor-subtitle-inspector-body")
                    .flex_1()
                    .min_h_0()
                    // 样式卡 + 单句编辑卡 + 翻译卡 + 对照表的自然高度≈830px，而本面板
                    // 在 869px 高的窗口里只剩 ~590px。此前左右二分模式用的是
                    // `overflow_hidden`，超出部分既不滚动也不被压缩，结果「字幕翻译」卡
                    // 被挤成 0 高、正文整段消失（只剩下方对照表留在裁切区外）。这里两种
                    // 布局一律放开纵向滚动，并把各卡片设成 `flex_none()`（不参与收缩），
                    // 卡片保持自然高度、由用户滚动查看。
                    .overflow_y_scroll()
                    .p(px(Theme::CARD_PAD_SM))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::CARD_GAP))
                    // 1. 全局字幕样式与排版配置卡片（仅「字幕样式」面板可见）
                    .child(
                        if panel == EditorSubtitlePanel::Style {
                            let cur_style = self.state.config.subtitle_style.clone();

                            primitives::card_sm()
                                .flex_none()
                                .gap(px(Theme::SPACE_2))
                                // 卡片标题与恢复默认
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(Theme::TEXT_BODY))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(Theme::text_muted())
                                                .child("全局字幕样式与排版"),
                                        )
                                        .child(
                                            div()
                                                .id("btn-reset-subtitle-style")
                                                .px_2()
                                                .py_0p5()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.state.config.subtitle_style = crate::utils::SubtitleStyleConfig::default();
                                                    this.state.save_subtitle_style();
                                                    cx.notify();
                                                }))
                                                .child("恢复默认"),
                                        ),
                                )
                                // 1) 文字颜色池（纯色圆点 + 最后一个调色板色轮）
                                .child({
                                    let is_custom_color = !crate::utils::SUBTITLE_COLOR_PALETTE
                                        .iter()
                                        .any(|(_, hex)| cur_style.primary_color.eq_ignore_ascii_case(hex));
                                    let is_picker_open = self.custom_color_picker_open;

                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(
                                                    div()
                                                        .w(px(58.0))
                                                        .flex_shrink_0()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("文字颜色"),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .flex_wrap()
                                                        .gap_2()
                                                        // 7 个预设纯色小圆点
                                                        .children(crate::utils::SUBTITLE_COLOR_PALETTE.into_iter().enumerate().map(|(idx, (_name, hex))| {
                                                            let is_sel = cur_style.primary_color.eq_ignore_ascii_case(hex);
                                                            let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(hex);
                                                            let swatch_color = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
                                                            div()
                                                                .id(("color-opt", idx))
                                                                .w(px(22.0))
                                                                .h(px(22.0))
                                                                .rounded_full()
                                                                .cursor_pointer()
                                                                .bg(swatch_color)
                                                                .border_2()
                                                                .border_color(if is_sel {
                                                                    Theme::accent_mint()
                                                                } else {
                                                                    rgba(0x80808044)
                                                                })
                                                                .hover(|s| if !is_sel { s.border_color(Theme::text_secondary()) } else { s })
                                                                .on_click(cx.listener({
                                                                    let hex_str = hex.to_string();
                                                                    move |this, _, _, cx| {
                                                                        this.state.config.subtitle_style.primary_color = hex_str.clone();
                                                                        this.state.save_subtitle_style();
                                                                        cx.notify();
                                                                    }
                                                                }))
                                                        }))
                                                        // 最后一个：调色板色轮圆点（可滑动自定义调色）
                                                        .child({
                                                            let is_wheel_active = is_custom_color || is_picker_open;
                                                            let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&cur_style.primary_color);
                                                            let cur_swatch = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
                                                            div()
                                                                .id("color-wheel-toggle-btn")
                                                                .w(px(22.0))
                                                                .h(px(22.0))
                                                                .rounded_full()
                                                                .cursor_pointer()
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_custom_color { cur_swatch } else { Theme::bg_inset() })
                                                                .border_2()
                                                                .border_color(if is_wheel_active {
                                                                    Theme::accent_mint()
                                                                } else {
                                                                    rgba(0x80808044)
                                                                })
                                                                .hover(|s| s.border_color(Theme::accent_mint()))
                                                                .child(
                                                                    if is_custom_color {
                                                                        div()
                                                                    } else {
                                                                        div()
                                                                            .text_size(px(10.0))
                                                                            .child("🎨")
                                                                    }
                                                                )
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.custom_color_picker_open = !this.custom_color_picker_open;
                                                                    cx.notify();
                                                                }))
                                                        })
                                                )
                                        )
                                        // 若调色板展开：显示环形色轮与自由滑动调色卡片
                                        .child(if is_picker_open {
                                            self.render_color_wheel_picker(&cur_style.primary_color, cx)
                                        } else {
                                            div().into_any_element()
                                        })
                                })
                                // 2) 字重与水平对齐
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("字重对齐"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                // 粗细开关
                                                .child(
                                                    div()
                                                        .w(px(96.0))
                                                        .flex()
                                                        .gap_1()
                                                        .child({
                                                            let is_bold = cur_style.is_bold;
                                                            div()
                                                                .id("btn-bold-on")
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .font_weight(FontWeight::BOLD)
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_bold { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_bold { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_bold { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child("加粗")
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.state.config.subtitle_style.is_bold = true;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        })
                                                        .child({
                                                            let is_bold = cur_style.is_bold;
                                                            div()
                                                                .id("btn-bold-off")
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if !is_bold { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if !is_bold { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if !is_bold { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child("常规")
                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                    this.state.config.subtitle_style.is_bold = false;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        })
                                                )
                                                // 对齐方式 (居左/居中/居右)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .flex()
                                                        .gap_1()
                                                        .children([("靠左", "left"), ("居中", "center"), ("靠右", "right")].into_iter().enumerate().map(|(idx, (label, align))| {
                                                            let is_sel = cur_style.alignment == align;
                                                            div()
                                                                .id(("align-btn", idx))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .flex()
                                                                .items_center()
                                                                .justify_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .child(label)
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.alignment = align.to_string();
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                        }))
                                                )
                                        )
                                )
                                // 3) 文字描边
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("文字描边"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .gap_1()
                                                .children(crate::utils::SUBTITLE_OUTLINE_OPTIONS.into_iter().enumerate().map(|(idx, (label, width))| {
                                                    let is_sel = (cur_style.outline_width - width).abs() < 0.1;
                                                    div()
                                                        .id(("outline-w", idx))
                                                        .flex_1()
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                        .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                        .child(label)
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.state.config.subtitle_style.outline_width = width;
                                                            this.state.save_subtitle_style();
                                                            cx.notify();
                                                        }))
                                                }))
                                        )
                                )
                                // 4) 背景底框
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .w(px(58.0))
                                                .flex_shrink_0()
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("背景底框"),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .gap_1()
                                                .children([("投影", "shadow"), ("黑框", "box"), ("胶囊", "pill"), ("纯净", "none")].into_iter().enumerate().map(|(idx, (label, mode))| {
                                                    let is_sel = cur_style.bg_style == mode;
                                                    div()
                                                        .id(("bg-mode", idx))
                                                        .flex_1()
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                        .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                        .child(label)
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.state.config.subtitle_style.bg_style = mode.to_string();
                                                            this.state.save_subtitle_style();
                                                            cx.notify();
                                                        }))
                                                }))
                                        )
                                )
                                // 3) 排版参数（字号 / 字间距 / 行间距 / 单行字数 / 底边距）
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1p5()
                                        // 字体大小
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字体大小"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::FontSize,
                                                    "-2",
                                                    "+2",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.font_size;
                                                        this.state.config.subtitle_style.font_size = cur.saturating_sub(2).max(10);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.font_size;
                                                        this.state.config.subtitle_style.font_size = (cur + 2).min(120);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.font_size),
                                                    cx,
                                                ))
                                        )
                                        // 字间距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字间距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::LetterSpacing,
                                                    "-1",
                                                    "+1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.letter_spacing;
                                                        this.state.config.subtitle_style.letter_spacing = cur.saturating_sub(1);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.letter_spacing;
                                                        this.state.config.subtitle_style.letter_spacing = (cur + 1).min(50);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.letter_spacing),
                                                    cx,
                                                ))
                                        )
                                        // 底边距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("底边距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::BottomMargin,
                                                    "-5",
                                                    "+5",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.bottom_margin;
                                                        this.state.config.subtitle_style.bottom_margin = cur.saturating_sub(5);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.bottom_margin;
                                                        this.state.config.subtitle_style.bottom_margin = (cur + 5).min(500);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}px", cur_style.bottom_margin),
                                                    cx,
                                                ))
                                        )
                                        // 行间距
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("行间距"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::LineSpacing,
                                                    "-0.1",
                                                    "+0.1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.line_spacing;
                                                        let next = ((cur - 0.1) * 10.0).round() / 10.0;
                                                        this.state.config.subtitle_style.line_spacing = next.max(0.5);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.line_spacing;
                                                        let next = ((cur + 0.1) * 10.0).round() / 10.0;
                                                        this.state.config.subtitle_style.line_spacing = next.min(5.0);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{:.1}x", cur_style.line_spacing),
                                                    cx,
                                                ))
                                        )
                                        // 单行字数
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(58.0)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("单行字数"))
                                                .child(self.render_style_stepper_input(
                                                    StyleField::MaxChars,
                                                    "-1",
                                                    "+1",
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.max_chars_per_line;
                                                        this.state.config.subtitle_style.max_chars_per_line = cur.saturating_sub(1).max(4);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    |this, cx| {
                                                        let cur = this.state.config.subtitle_style.max_chars_per_line;
                                                        this.state.config.subtitle_style.max_chars_per_line = (cur + 1).min(100);
                                                        this.state.save_subtitle_style();
                                                        cx.notify();
                                                    },
                                                    format!("{}字", cur_style.max_chars_per_line),
                                                    cx,
                                                ))
                                        )
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 2. 单句快速编辑卡（仅「字幕样式」面板）。
                    //
                    // 这张卡自带「实时预览条」，预览的就是卡里正在编辑的这一句——编辑与
                    // 预览在同一张卡上，改字 / 调字号都是即时的。翻译面板不重复放编辑卡：
                    // 面板标头已能切回样式面板，两处各放一份只会让人分不清哪份生效。
                    .child(
                        if let Some(seg) = cur_seg.as_ref().filter(|_| panel == EditorSubtitlePanel::Style) {
                            let seg_idx = seg.index;
                            let start_ts = seconds_to_timestamp_short(seg.start);
                            let end_ts = seconds_to_timestamp_short(seg.end);
                            let dur = seg.duration();
                            // 单字无法拆分；最后一句没有下一句可合并，按钮置灰而不是点了没反应
                            let can_split = seg.display_text().chars().count() > 1;
                            let can_merge = seg_idx < self.state.segments.len();

                            primitives::card_sm()
                                .flex_none()
                                .gap(px(Theme::SPACE_2))
                                // 第一行：文段编号与主要操作 (保存修改、删除)
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .justify_between()
                                        .gap_2()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_0p5()
                                                        .rounded_md()
                                                        .bg(Theme::tint_mint_badge())
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child(format!("#{:03}", seg_idx)),
                                                )
                                                .child(
                                                    div()
                                                        .font_family("Consolas")
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("{} - {} ({:.1}s)", start_ts, end_ts, dur)),
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
                                                        .id("btn-save-text-top")
                                                        .px_3()
                                                        .py_1()
                                                        .rounded_md()
                                                        .bg(Theme::accent_mint())
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(Theme::text_on_accent())
                                                        .hover(|s| s.opacity(0.9))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.save_selected_text();
                                                            cx.notify();
                                                        }))
                                                        .child("保存修改"),
                                                )
                                                .child(
                                                    primitives::btn_danger("删除", primitives::BtnSize::Sm)
                                                        .id("btn-del-seg")
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.state.delete_selected_segment();
                                                            this.trigger_extract_frame(cx);
                                                            cx.notify();
                                                        })),
                                                ),
                                        ),
                                )
                                // 第一行半：次级操作工具条 (弹窗编辑、改译文、拆分、合并下句)
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            div()
                                                .id("btn-open-prompt-edit")
                                                .px_2p5()
                                                .py_1()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.prompt_edit_text(cx);
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("弹窗编辑"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("btn-open-translation-edit")
                                                .px_2p5()
                                                .py_1()
                                                .rounded_md()
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .cursor_pointer()
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    if let Some(idx) = this.state.selected_segment_index {
                                                        let trans = this.state.segments.iter().find(|s| s.index == idx)
                                                            .and_then(|s| s.translation.clone())
                                                            .unwrap_or_default();
                                                        this.start_inline_edit(idx, true, &trans, window, cx);
                                                    }
                                                }))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("改译文"),
                                                ),
                                        )
                                        .child(
                                            if seg.has_translation() {
                                                div()
                                                    .id("btn-clear-translation")
                                                    .px_2p5()
                                                    .py_1()
                                                    .rounded_md()
                                                    .bg(Theme::bg_inset())
                                                    .border_1()
                                                    .border_color(Theme::border_mid())
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(Theme::bg_hover()))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.state.set_selected_translation("");
                                                        cx.notify();
                                                    }))
                                                    .child(
                                                        div()
                                                            .text_size(px(Theme::TEXT_SMALL))
                                                            .text_color(Theme::text_secondary())
                                                            .child("清除译文"),
                                                    )
                                                    .into_any_element()
                                            } else {
                                                div().into_any_element()
                                            },
                                        )
                                        .child(Self::render_mini_btn(
                                            "btn-split-seg",
                                            "拆分",
                                            can_split,
                                            cx,
                                            |this, cx| {
                                                let len = this.state.editing_text.chars().count();
                                                let at = if this.text_cursor_pos > 0
                                                    && this.text_cursor_pos < len
                                                {
                                                    Some(this.text_cursor_pos)
                                                } else {
                                                    None
                                                };
                                                this.state.split_selected_segment(at);
                                                this.trigger_extract_frame(cx);
                                            },
                                        ))
                                        .child(Self::render_mini_btn(
                                            "btn-merge-seg",
                                            "合并下句",
                                            can_merge,
                                            cx,
                                            |this, cx| {
                                                this.state.merge_selected_with_next();
                                                this.trigger_extract_frame(cx);
                                            },
                                        )),
                                )
                                // 第二行：原文输入框（带清晰标题）
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::text_muted())
                                                        .child("原文编辑"),
                                                ),
                                        )
                                        .child({
                                            let is_focused = self.is_text_focused;
                                            let total_chars = cur_text.chars().count();
                                            let cursor_pos = self.text_cursor_pos.min(total_chars);

                                            div()
                                                .id("subtitle-text-editor-box")
                                                .w_full()
                                                .track_focus(&self.text_focus)
                                                .min_h(px(Theme::TABLE_HEADER_H))
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1))
                                                .rounded(px(Theme::RADIUS_LG))
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
                                                .text_size(px(Theme::TEXT_BODY_LG))
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
                                        }),
                                )
                                // 第二行半：译文内容展示（若已翻译）
                                .children(seg.translation.as_ref().filter(|t| !t.trim().is_empty()).map(|trans_str| {
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::accent_mint())
                                                        .child("译文内容"),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .id("subtitle-trans-editor-box")
                                                .w_full()
                                                .min_h(px(Theme::TABLE_HEADER_H))
                                                .px(px(Theme::SPACE_3))
                                                .py(px(Theme::SPACE_1))
                                                .rounded(px(Theme::RADIUS_LG))
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .hover(|s| s.border_color(Theme::accent_mint()))
                                                .text_size(px(Theme::TEXT_BODY_LG))
                                                .text_color(Theme::accent_mint())
                                                .on_click(cx.listener({
                                                    let trans_clone = trans_str.clone();
                                                    let idx = seg.index;
                                                    move |this, _, window, cx| {
                                                        this.start_inline_edit(idx, true, &trans_clone, window, cx);
                                                    }
                                                }))
                                                .child(trans_str.clone()),
                                        )
                                }))
                                // 第二行四分之三：实时排版效果预览（带明确标题与双语联动）
                                .child(
                                    div()
                                        .w_full()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .pt_1()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(Theme::text_muted())
                                                        .child("画面效果预览"),
                                                ),
                                        )
                                        .child({
                                            let preview_str = if let Some(t) = &seg.translation {
                                                if !t.trim().is_empty() && t.trim() != cur_text.trim() {
                                                    format!("{}\n{}", t.trim(), cur_text.trim())
                                                } else {
                                                    cur_text.clone()
                                                }
                                            } else {
                                                cur_text.clone()
                                            };
                                            self.render_subtitle_preview_box(&preview_str, cx)
                                        }),
                                )
                                // 第三行：时间微调与快捷标点注入 (规整两行，干净不拥挤)
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap(px(Theme::SPACE_2))
                                        // 第一行：时间微调（起止各 ±0.1s / ±0.5s）
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_color(Theme::text_muted())
                                                                .child("起点:"),
                                                        )
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-minus-500",
                                                            "-0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(-0.5, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-minus-100",
                                                            "-0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(-0.1, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-plus-100",
                                                            "+0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.1, 0.0),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-start-plus-500",
                                                            "+0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.5, 0.0),
                                                        )),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1()
                                                        .child(
                                                            div()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_color(Theme::text_muted())
                                                                .child("终点:"),
                                                        )
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-minus-500",
                                                            "-0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, -0.5),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-minus-100",
                                                            "-0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, -0.1),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-plus-100",
                                                            "+0.1",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, 0.1),
                                                        ))
                                                        .child(Self::render_mini_btn(
                                                            "fine-tune-end-plus-500",
                                                            "+0.5",
                                                            true,
                                                            cx,
                                                            |this, _| this.state.adjust_selected_times(0.0, 0.5),
                                                        )),
                                                ),
                                        )
                                        // 第二行：快捷标点注入
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("标点:"),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_1p5()
                                                        .child(self.render_punct_btn("，", cx))
                                                        .child(self.render_punct_btn("。", cx))
                                                        .child(self.render_punct_btn("？", cx))
                                                        .child(self.render_punct_btn("！", cx))
                                                        .child(self.render_punct_btn("、", cx)),
                                                ),
                                        ),
                                )
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    )
                    // 2.4 整轨时间轴调整（仅「字幕样式」面板可见）
                    //
                    // 与样式卡并列、而不是塞进样式卡内部：它是**整轨级**操作（平移 /
                    // 缩放 / 铺满），与「观感」是两件事；放在同一页是因为两者都属于
                    // 「我要调整这一整份字幕」的心智模型。
                    .child(if panel == EditorSubtitlePanel::Style {
                        self.render_retime_card(cx).into_any_element()
                    } else {
                        div().into_any_element()
                    })
                    // 2.5 字幕多语言翻译面板（仅「字幕翻译」面板可见；后端链路早已就绪）
                    .child(
                        if panel == EditorSubtitlePanel::Translate {
                            self.render_translate_card(cx).into_any_element()
                        } else if panel == EditorSubtitlePanel::Stats {
                            // 统计与导出同屏：两张卡都是「对整份字幕做事」的收尾动作，
                            // 一起竖排在同一个面板里。
                            div()
                                .w_full()
                                .flex_none()
                                .flex()
                                .flex_col()
                                .gap(px(Theme::CARD_GAP))
                                .child(self.render_subtitle_stats_card(cx))
                                .child(self.render_export_card(cx))
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        },
                    )
                    // 3. 多语言字幕配置与对照大表格 (仅在「字幕翻译」面板可见；「字幕样式」与「统计与导出」不展示此卡)
                    .child(if panel == EditorSubtitlePanel::Translate {
                        primitives::card_sm()
                            .flex_1()
                            .min_h(px(Theme::TABLE_MIN_H))
                            .gap(px(0.0))
                            // 上下堆叠时面板整体已可纵向滚动，这里不能再吃 flex_1
                            // （自动高度的滚动容器里会塌成 0 高），改为固定高度。
                            .when(stacked, |d| d.flex_none().h(px(Theme::TABLE_STACK_H)))
                            .overflow_hidden()
                            // 搜索栏：按关键字过滤清单（原文与译文都参与匹配），
                            // 命中条数实时回显，避免用户以为「搜了没结果」是卡住了
                            .child({
                                let total = self.state.segments.len();
                                let visible = self.subtitle_filter.len();
                                let searching = !self.subtitle_search.trim().is_empty();
                                div()
                                    .w_full()
                                    .h(px(Theme::TABLE_HEADER_H))
                                    .px(px(Theme::SPACE_3))
                                    .bg(Theme::bg_sidebar())
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_muted())
                                            .child("搜索"),
                                    )
                                    .child(self.render_search_input(cx))
                                    .child(if searching {
                                        div()
                                            .id("subtitle-search-clear")
                                            .flex_shrink_0()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_md()
                                            .bg(Theme::bg_raised())
                                            .border_1()
                                            .border_color(Theme::border_mid())
                                            .cursor_pointer()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(Theme::text_secondary())
                                            .hover(|s| {
                                                s.bg(Theme::bg_hover())
                                                    .text_color(Theme::text_primary())
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.subtitle_search.clear();
                                                this.subtitle_search_cursor = 0;
                                                cx.notify();
                                            }))
                                            .child("清除")
                                            .into_any_element()
                                    } else {
                                        div().into_any_element()
                                    })
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .font_family("Consolas")
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .text_color(if searching && visible == 0 {
                                                Theme::accent_red()
                                            } else {
                                                Theme::text_muted()
                                            })
                                            .child(if searching {
                                                format!("命中 {}/{}", visible, total)
                                            } else {
                                                format!("共 {} 句", total)
                                            }),
                                    )
                                    .child(
                                        // 查找替换开关：与「搜索」分开成独立面板，因为
                                        // 搜索只过滤显示、替换会真改字幕，混在一起极易误操作。
                                        primitives::mini_btn("查找替换", true)
                                            .id("replace-panel-toggle")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.replace_panel_open = !this.replace_panel_open;
                                                if !this.replace_panel_open {
                                                    this.replace_status = None;
                                                }
                                                cx.notify();
                                            })),
                                    )
                            })
                            .children(self.render_replace_bar(cx))
                            // 列表信息子标头
                            .child({
                                let has_any_trans = self.state.segments.iter().any(|s| s.has_translation());
                                let has_distinct_trans = self.state.segments.iter().any(|s| {
                                    if let Some(t) = &s.translation {
                                        !t.trim().is_empty() && t.trim() != s.display_text().trim()
                                    } else {
                                        false
                                    }
                                });
                                div()
                                    .w_full()
                                    .h(px(26.0))
                                    .px(px(Theme::SPACE_3))
                                    .bg(Theme::bg_sidebar())
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_muted())
                                            .child(if has_distinct_trans {
                                                "字幕清单 (上下并列显示)"
                                            } else {
                                                "字幕清单"
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_SMALL))
                                            .text_color(if has_distinct_trans {
                                                Theme::accent_mint()
                                            } else {
                                                Theme::text_muted()
                                            })
                                            .child(if has_distinct_trans {
                                                "双语显示"
                                            } else if has_any_trans {
                                                "单语显示 (译文同原文)"
                                            } else {
                                                "单语显示"
                                            }),
                                    )
                            })
                            // 表格行内容：uniform_list 虚拟化渲染，上下并列布局
                            .child({
                                // 选中行变化时自动滚动跟随（时间轴点击 / 上下句跳转 / 播放联动）
                                if self.subtitle_list_followed_sel != sel_idx {
                                    if let Some(idx) = sel_idx {
                                        // 滚动位置按「过滤后列表里的行号」计算，否则搜索态下会滚错行
                                        let pos = self.subtitle_filter.iter().position(|&p| {
                                            self.state.segments.get(p).map(|s| s.index) == Some(idx)
                                        });
                                        if let Some(pos) = pos {
                                            self.subtitle_list_scroll
                                                .scroll_to_item(pos, ScrollStrategy::Top);
                                        }
                                    }
                                    self.subtitle_list_followed_sel = sel_idx;
                                }

                                let row_count = self.subtitle_filter.len();
                                let has_distinct_trans = self.state.segments.iter().any(|s| {
                                    if let Some(t) = &s.translation {
                                        !t.trim().is_empty() && t.trim() != s.display_text().trim()
                                    } else {
                                        false
                                    }
                                });
                                let row_h = if has_distinct_trans {
                                    SUBTITLE_ROW_H_BILINGUAL
                                } else {
                                    SUBTITLE_ROW_H_MONO
                                };

                                // 术语表合规：疑似未按术语表译出的句子下标集合。每帧只算一次
                                let glossary_bad: std::collections::HashSet<usize> = self
                                    .cached_glossary_violations()
                                    .into_iter()
                                    .collect();
                                // 低置信句集合：同样每帧只算一次
                                let low_conf: std::collections::HashSet<usize> = self
                                    .cached_low_confidence()
                                    .into_iter()
                                    .collect();
                                uniform_list(
                                    "inspector-segments-virtual",
                                    row_count,
                                    cx.processor(move |this, visible_range: std::ops::Range<usize>, _window, cx| {
                                        let sel = this.state.selected_segment_index;
                                        let cur_time = this.state.current_time;
                                        let glossary_bad = glossary_bad.clone();
                                        let low_conf = low_conf.clone();
                                        let inline_target = this.inline_edit_target;
                                        let inline_buf = this.inline_edit_buffer.clone();
                                        let inline_focus = this.inline_edit_focus.clone();
                                        visible_range
                                            .map(|i| {
                                                let pos = this.subtitle_filter.get(i).copied().unwrap_or(i);
                                                let Some(seg) = this.state.segments.get(pos) else {
                                                    return div()
                                                        .id(("table-row-placeholder", i))
                                                        .w_full()
                                                        .h(px(row_h))
                                                        .border_b_1()
                                                        .border_color(Theme::border_subtle())
                                                        .into_any_element();
                                                };
                                                let seg_idx = seg.index;
                                                let is_selected = sel == Some(seg_idx);
                                                let is_playing_here = cur_time >= seg.start && cur_time <= seg.end;
                                                let start_ts = seconds_to_timestamp_short(seg.start);
                                                let end_ts = seconds_to_timestamp_short(seg.end);
                                                let dur = seg.duration();
                                                let raw_text = seg.display_text().to_string();
                                                let has_trans = seg.has_translation();
                                                let trans_text = seg.translation.clone().unwrap_or_default();
                                                let speaker = seg.speaker;
                                                let glossary_flagged = glossary_bad.contains(&seg_idx);
                                                let low_conf_flagged = low_conf.contains(&seg_idx);

                                                div()
                                                    .id(("table-row-seg", seg_idx))
                                                    .w_full()
                                                    .h(px(row_h))
                                                    .px(px(Theme::SPACE_3))
                                                    .py(px(4.0))
                                                    .border_b_1()
                                                    .border_color(Theme::border_subtle())
                                                    .cursor_pointer()
                                                    .relative()
                                                    .bg(if is_selected {
                                                        Theme::tint_mint_soft()
                                                    } else if glossary_flagged {
                                                        Theme::tint_warn_soft()
                                                    } else if is_playing_here {
                                                        Theme::tint_primary_soft()
                                                    } else {
                                                        Theme::transparent()
                                                    })
                                                    .hover(|s| s.bg(if is_selected { Theme::tint_mint_badge() } else { Theme::bg_hover() }))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        this.state.select_segment(seg_idx);
                                                        this.trigger_extract_frame(cx);
                                                        cx.notify();
                                                    }))
                                                    .flex()
                                                    .flex_col()
                                                    .justify_start()
                                                    .gap(px(3.0))
                                                    // 选中指示线（左缘 3px 竖条）
                                                    .child(if is_selected {
                                                        div()
                                                            .absolute()
                                                            .left(px(0.0))
                                                            .top(px(2.0))
                                                            .bottom(px(2.0))
                                                            .w(px(3.0))
                                                            .rounded_r_full()
                                                            .bg(Theme::accent_mint())
                                                            .into_any_element()
                                                    } else {
                                                        div().into_any_element()
                                                    })
                                                    // 第一行：时间与元数据（序号、起止时间、时长、说话人、标记）
                                                    .child(
                                                        div()
                                                            .w_full()
                                                            .flex()
                                                            .items_center()
                                                            .justify_between()
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .items_center()
                                                                    .gap_1p5()
                                                                    .children(low_conf_flagged.then(|| {
                                                                        primitives::stat_dot_sm(Theme::accent_red())
                                                                    }))
                                                                    .child(
                                                                        div()
                                                                            .px_1()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .bg(if is_selected {
                                                                                Theme::tint_mint_badge()
                                                                            } else {
                                                                                Theme::bg_raised()
                                                                            })
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .font_weight(FontWeight::BOLD)
                                                                            .text_color(if is_selected {
                                                                                Theme::accent_mint()
                                                                            } else {
                                                                                Theme::text_muted()
                                                                            })
                                                                            .child(format!("#{:03}", seg_idx)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .font_family("Consolas")
                                                                            .text_size(px(Theme::TEXT_SMALL))
                                                                            .text_color(if is_selected {
                                                                                Theme::accent_mint()
                                                                            } else {
                                                                                Theme::text_secondary()
                                                                            })
                                                                            .child(format!("{} - {}", start_ts, end_ts)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .text_color(Theme::text_muted())
                                                                            .child(format!("({:.1}s)", dur)),
                                                                    ),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .items_center()
                                                                    .gap_1()
                                                                    .children(speaker.map(|s| {
                                                                        div()
                                                                            .px_1p5()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .border_1()
                                                                            .border_color(speaker_border(s))
                                                                            .bg(speaker_tint(s))
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .font_weight(FontWeight::SEMIBOLD)
                                                                            .text_color(speaker_color(s))
                                                                            .child(format!("说话人 {}", s + 1))
                                                                    }))
                                                                    .children(glossary_flagged.then(|| {
                                                                        div()
                                                                            .px_1()
                                                                            .py_0p5()
                                                                            .rounded_sm()
                                                                            .bg(Theme::tint_warn_soft())
                                                                            .text_size(px(Theme::TEXT_CAPTION))
                                                                            .text_color(Theme::accent_orange())
                                                                            .child("术语")
                                                                    })),
                                                            ),
                                                    )
                                                    // 第二行：字幕内容 (原文，点击直接就地编辑)
                                                    .child({
                                                        let is_editing_raw = inline_target == Some((seg_idx, false));
                                                        if is_editing_raw {
                                                            div()
                                                                .id(("inline-edit-raw", seg_idx))
                                                                .w_full()
                                                                .h(px(24.0))
                                                                .track_focus(&inline_focus)
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded_sm()
                                                                .bg(Theme::bg_sidebar())
                                                                .border_1()
                                                                .border_color(Theme::accent_mint())
                                                                .cursor_text()
                                                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                                                    window.focus(&this.inline_edit_focus);
                                                                    cx.notify();
                                                                }))
                                                                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                                                    this.handle_inline_edit_keydown(event, cx);
                                                                }))
                                                                .flex()
                                                                .items_center()
                                                                .justify_between()
                                                                .child(
                                                                    div()
                                                                        .flex_1()
                                                                        .text_size(px(Theme::TEXT_BODY))
                                                                        .text_color(Theme::accent_mint())
                                                                        .truncate()
                                                                        .child(format!("{}▌", inline_buf)),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .gap_1()
                                                                        .flex_shrink_0()
                                                                        .child(
                                                                            div()
                                                                                .id(("inline-save-raw", seg_idx))
                                                                                .px_1()
                                                                                .py_0p5()
                                                                                .rounded_sm()
                                                                                .bg(Theme::accent_mint())
                                                                                .text_size(px(10.0))
                                                                                .text_color(Theme::text_on_accent())
                                                                                .cursor_pointer()
                                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                                    this.commit_inline_edit(cx);
                                                                                }))
                                                                                .child("保存"),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .id(("inline-cancel-raw", seg_idx))
                                                                                .px_1()
                                                                                .py_0p5()
                                                                                .rounded_sm()
                                                                                .bg(Theme::bg_hover())
                                                                                .text_size(px(10.0))
                                                                                .text_color(Theme::text_muted())
                                                                                .cursor_pointer()
                                                                                .on_click(cx.listener(|this, _, _, cx| {
                                                                                    this.cancel_inline_edit(cx);
                                                                                }))
                                                                                .child("取消"),
                                                                        ),
                                                                )
                                                        } else {
                                                            div()
                                                                .id(("table-row-raw", seg_idx))
                                                                .w_full()
                                                                .text_size(px(Theme::TEXT_BODY))
                                                                .font_weight(if is_selected {
                                                                    FontWeight::SEMIBOLD
                                                                } else {
                                                                    FontWeight::NORMAL
                                                                })
                                                                .text_color(Theme::text_primary())
                                                                .truncate()
                                                                .hover(|s| s.text_color(Theme::accent_mint()))
                                                                .cursor_pointer()
                                                                .on_click(cx.listener({
                                                                    let raw_for_click = raw_text.clone();
                                                                    move |this, _, window, cx| {
                                                                        this.state.select_segment(seg_idx);
                                                                        this.trigger_extract_frame(cx);
                                                                        let now = std::time::Instant::now();
                                                                        let is_double = match this.last_subtitle_click {
                                                                            Some((last_idx, false, last_t))
                                                                                if last_idx == seg_idx && now.duration_since(last_t).as_millis() < 350 =>
                                                                            {
                                                                                true
                                                                            }
                                                                            _ => false,
                                                                        };
                                                                        if is_double {
                                                                            this.last_subtitle_click = None;
                                                                            this.start_inline_edit(seg_idx, false, &raw_for_click, window, cx);
                                                                        } else {
                                                                            this.last_subtitle_click = Some((seg_idx, false, now));
                                                                            cx.notify();
                                                                        }
                                                                    }
                                                                }))
                                                                .child(raw_text.clone())
                                                        }
                                                    })
                                                    // 第三行：翻译字幕 (点击直接就地编辑；无翻译且非编辑态时不显示)
                                                    .children({
                                                        let is_editing_trans = inline_target == Some((seg_idx, true));
                                                        let show_trans = is_editing_trans || (has_trans && !trans_text.trim().is_empty() && trans_text.trim() != raw_text.trim());
                                                        if show_trans {
                                                            Some(if is_editing_trans {
                                                                div()
                                                                    .id(("inline-edit-trans", seg_idx))
                                                                    .w_full()
                                                                    .h(px(24.0))
                                                                    .track_focus(&inline_focus)
                                                                    .px_1p5()
                                                                    .py_0p5()
                                                                    .rounded_sm()
                                                                    .bg(Theme::bg_sidebar())
                                                                    .border_1()
                                                                    .border_color(Theme::accent_mint())
                                                                    .cursor_text()
                                                                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                                                                        window.focus(&this.inline_edit_focus);
                                                                        cx.notify();
                                                                    }))
                                                                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                                                                        this.handle_inline_edit_keydown(event, cx);
                                                                    }))
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_between()
                                                                    .child(
                                                                        div()
                                                                            .flex_1()
                                                                            .text_size(px(Theme::TEXT_SMALL))
                                                                            .text_color(Theme::accent_mint())
                                                                            .truncate()
                                                                            .child(format!("{}▌", inline_buf)),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .flex()
                                                                            .items_center()
                                                                            .gap_1()
                                                                            .flex_shrink_0()
                                                                            .child(
                                                                                div()
                                                                                    .id(("inline-save-trans", seg_idx))
                                                                                    .px_1()
                                                                                    .py_0p5()
                                                                                    .rounded_sm()
                                                                                    .bg(Theme::accent_mint())
                                                                                    .text_size(px(10.0))
                                                                                    .text_color(Theme::text_on_accent())
                                                                                    .cursor_pointer()
                                                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                                                        this.commit_inline_edit(cx);
                                                                                    }))
                                                                                    .child("保存"),
                                                                            )
                                                                            .child(
                                                                                div()
                                                                                    .id(("inline-cancel-trans", seg_idx))
                                                                                    .px_1()
                                                                                    .py_0p5()
                                                                                    .rounded_sm()
                                                                                    .bg(Theme::bg_hover())
                                                                                    .text_size(px(10.0))
                                                                                    .text_color(Theme::text_muted())
                                                                                    .cursor_pointer()
                                                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                                                        this.cancel_inline_edit(cx);
                                                                                    }))
                                                                                    .child("取消"),
                                                                            ),
                                                                    )
                                                            } else {
                                                                div()
                                                                    .id(("table-row-trans", seg_idx))
                                                                    .w_full()
                                                                    .text_size(px(Theme::TEXT_SMALL))
                                                                    .text_color(if is_selected {
                                                                        Theme::accent_mint()
                                                                    } else {
                                                                        Theme::text_secondary()
                                                                    })
                                                                    .truncate()
                                                                    .hover(|s| s.text_color(Theme::accent_mint()))
                                                                    .cursor_pointer()
                                                                    .on_click(cx.listener({
                                                                        let trans_for_click = trans_text.clone();
                                                                        move |this, _, window, cx| {
                                                                            this.state.select_segment(seg_idx);
                                                                            this.trigger_extract_frame(cx);
                                                                            let now = std::time::Instant::now();
                                                                            let is_double = match this.last_subtitle_click {
                                                                                Some((last_idx, true, last_t))
                                                                                    if last_idx == seg_idx && now.duration_since(last_t).as_millis() < 350 =>
                                                                                {
                                                                                    true
                                                                                }
                                                                                _ => false,
                                                                            };
                                                                            if is_double {
                                                                                this.last_subtitle_click = None;
                                                                                this.start_inline_edit(seg_idx, true, &trans_for_click, window, cx);
                                                                            } else {
                                                                                this.last_subtitle_click = Some((seg_idx, true, now));
                                                                                cx.notify();
                                                                            }
                                                                        }
                                                                    }))
                                                                    .child(trans_text)
                                                            })
                                                        } else {
                                                            None
                                                        }
                                                    })
                                                    .into_any_element()
                                            })
                                            .collect()
                                    }),
                                )
                                .track_scroll(self.subtitle_list_scroll.clone())
                                .flex_1()
                                .w_full()
                                // 内嵌清单自己吃掉滚轮：清单还有余量时不让事件继续冒泡到
                                // 外层可滚动面板，避免「清单与外层面板一起滚」（见
                                // `should_consume_scroll` 的说明）。到边界时放行，外层接管。
                                .on_scroll_wheel(cx.listener(
                                    |this, event: &ScrollWheelEvent, _window, cx| {
                                        let delta_y = match event.delta {
                                            ScrollDelta::Lines(p) => p.y,
                                            ScrollDelta::Pixels(p) => f32::from(p.y),
                                        };
                                        let state = this.subtitle_list_scroll.0.borrow();
                                        let offset_y =
                                            f32::from(state.base_handle.offset().y);
                                        let max_h = f32::from(state.base_handle.max_offset().height);
                                        drop(state);
                                        if primitives::should_consume_scroll(delta_y, offset_y, max_h) {
                                            cx.stop_propagation();
                                        }
                                    },
                                ))
                            })
                            .into_any_element()
                    } else {
                        div().into_any_element()
                    })
            )
    }

    /// 字幕预览框：显示当前编辑句的排版效果，两侧把手可拖拽调宽。
    ///
    /// 拖动任一手的位移按**两倍**作用到宽度上——把手贴在字幕框的两条边上，
    /// 往右拖 d 像素，右边多 d、左边也多 d，于是框以中线为中心左右对称变宽 / 变窄。
    /// 手动拖过之后宽度以手动值为准（`preview_box_w`），否则按「单行最大字数 ×
    /// 预览字号」自动推算。
    fn render_subtitle_preview_box(
        &mut self,
        cur_text: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 样式取自全局配置（面板里改哪个参数都立即写回这里）
        let style = self.state.config.subtitle_style.clone();
        let font_px = style.preview_font_px();
        let (fg, bg, border) = subtitle_computed_colors(&style);
        let bg = preview_backdrop(bg);
        // 空行会被 GPUI 折成 0 高，给个占位符保证预览条始终有形
        let preview_text = if cur_text.trim().is_empty() {
            "（本句暂无文字）".to_string()
        } else {
            cur_text.to_string()
        };
        let box_w = self.subtitle_box_w();
        let is_dragging = self.preview_drag.is_some();

        div()
            .id("subtitle-preview-row")
            .w_full()
            .h(px(Theme::STYLE_PREVIEW_H))
            .rounded(px(Theme::RADIUS_LG))
            .bg(Theme::bg_media())
            .border_1()
            .border_color(if is_dragging {
                Theme::accent_mint()
            } else {
                Theme::border()
            })
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            // 位移监听挂在整个预览行上而不是把手上：把手只有几像素宽，拖动时指针
            // 稍一移动就出了它的命中区；整行满宽，横向拖多远都不会掉出去。
            //
            // 必须再校验 `event.pressed_button`：GPUI 的 `on_mouse_move` 只看指针**当前**
            // 是否落在元素内，并不关心按钮是不是在这个元素上按下的。少了这层校验，指针
            // 在别处按下、再移进本行时会被误当成「正在拖预览框」——`drag_preview_box`
            // 见 `preview_drag` 是 `Some` 就一直改宽度。更糟的是若按钮是在把手之外
            // 松开的，本行的 `on_mouse_up` 收不到（松手时指针不在行内），`preview_drag`
            // 永远不清空，此后指针每次划过预览行都会继续改宽度。
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                this.drag_preview_box(event.position.x, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_preview_box_drag();
                    cx.notify();
                }),
            )
            // 在行**外**松手也要收尾。GPUI 的 `on_mouse_up` 同样要求指针落在元素内，
            // 若用户把指针拖出行外才松手，上面的 handler 收不到，`preview_drag` 会一直
            // 挂着——预览行从此恒亮「拖拽中」描边，且宽度卡在半途的中间值。
            // `on_mouse_up_out` 走捕获阶段、专在「松手时指针不在元素内」时触发，用它兜底。
            // 没有拖拽会话时 `end_preview_box_drag` 是空操作，因此它在任何别处松手都安全。
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_preview_box_drag();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(self.preview_resize_handle(true, cx))
                    .child(
                        div()
                            .w(px(box_w))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(bg)
                            .border_1()
                            .border_color(border)
                            .text_size(px(font_px))
                            .font_weight(if style.is_bold { FontWeight::BOLD } else { FontWeight::NORMAL })
                            .text_color(fg)
                            .text_align(match style.alignment.as_str() {
                                "left" => TextAlign::Left,
                                "right" => TextAlign::Right,
                                _ => TextAlign::Center,
                            })
                            .line_height(px(font_px * style.line_spacing))
                            .child(preview_text),
                    )
                    .child(self.preview_resize_handle(false, cx)),
            )
    }

    /// 预览框的拖拽把手，贴在字幕框左右两侧；`left` 决定是左只还是右只。
    fn preview_resize_handle(&self, left: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.preview_drag.is_some();
        div()
            .id(if left {
                "preview-handle-left"
            } else {
                "preview-handle-right"
            })
            .w(px(Theme::RESIZE_HANDLE_W))
            .h(px(Theme::STYLE_PREVIEW_H - Theme::SPACE_3 * 2.0))
            .rounded(px(Theme::RADIUS_SM))
            .bg(if active {
                Theme::accent_mint()
            } else {
                Theme::border_strong()
            })
            .opacity(if active { 1.0 } else { 0.5 })
            .cursor_col_resize()
            .hover(|s| s.opacity(1.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.begin_preview_box_drag(event.position.x, left);
                    cx.notify();
                }),
            )
    }

    /// 字幕框的统一宽度（px）。
    ///
    /// 监视器覆盖层与编辑卡预览条**共用这一个来源**，所以拖把手时两者在同一帧一起
    /// 变化，不存在「预览改了、画面没跟上」的延迟。手动拖过就用 `preview_box_w`，
    /// 否则按「单行最大字数 × 预览字号」自动推算。
    fn subtitle_box_w(&self) -> f32 {
        let s = &self.state.config.subtitle_style;
        self.preview_box_w
            .unwrap_or_else(|| s.preview_font_px() * s.max_chars_per_line as f32)
            .clamp(Theme::PREVIEW_BOX_MIN_W, Theme::PREVIEW_BOX_MAX_W)
    }

    /// 开始拖拽预览框：记下基准宽度、按下时的鼠标 x、以及抓的是哪一侧把手。
    fn begin_preview_box_drag(&mut self, mouse_x: Pixels, left: bool) {
        self.preview_drag = Some((self.subtitle_box_w(), f32::from(mouse_x), left));
    }

    /// 拖拽中：位移按两倍作用到宽度（两侧对称），结果记进手动宽度。
    ///
    /// 左右把手的「往外」方向相反，所以按手柄所在侧取符号：右手柄往右拉（dx>0）
    /// 加宽，左手柄往左拉（dx<0）同样加宽。少了这一步符号，左手柄往外拉会算成
    /// 负增量，看着就是「往外拉反而变窄」。
    fn drag_preview_box(&mut self, mouse_x: Pixels, cx: &mut Context<Self>) {
        let Some((start_w, start_x, left)) = self.preview_drag else {
            return;
        };
        let sign = if left { -1.0 } else { 1.0 };
        let new_w = (start_w + sign * 2.0 * (f32::from(mouse_x) - start_x))
            .clamp(Theme::PREVIEW_BOX_MIN_W, Theme::PREVIEW_BOX_MAX_W);
        self.preview_box_w = Some(new_w);
        cx.notify();
    }

    /// 松手：结束拖拽会话，并把宽度落盘（下次启动仍用这个宽度）。
    fn end_preview_box_drag(&mut self) {
        if self.preview_drag.take().is_some() {
            self.state.config.subtitle_style.preview_box_w = self.preview_box_w;
            self.state.save_subtitle_style();
        }
    }

    /// 整轨时间轴调整卡：平移 / 缩放 / 铺满目标时长。
    ///
    /// # 为什么需要它
    ///
    /// 字幕的时间轴几乎从不对上最终成片：片子剪掉了 12 秒就要整轨平移；开头加了一段
    /// 片头就要「某点之后平移」；25 分钟的成片复用 22 分钟的版本就要整体缩放；TXT
    /// 导入合成的时间轴更是完全假的、必须铺满真实时长。此前这些只能一行行拖。
    ///
    /// # 设计要点
    ///
    /// - **先算后应用**：规划在 `subtitle::timing`（纯函数、16 条单测），这里只负责
    ///   取参数、显示预览摘要、点「应用」才写回。用户可以反复试参数而不用担心
    ///   把时间轴改坏——没点应用就没有任何副作用。
    /// - **应用前记撤销快照**：整轨操作一改就是几百句，必须有路可退。
    /// - **重叠自纠**：变换后若产生重叠（缩放比例与原始空隙不一致时常见），应用时
    ///   自动过一遍 `resolve_overlaps`，保证结果仍能原样通过 `optimize_segments`
    ///   ——否则用户下次载入工程会被「排序 + 截断重叠」再改一遍，看起来像怎么改都丢。
    fn render_retime_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::subtitle::timing::{describe, plan_retime, suggest_fit, TimeShift};
        let segments = &self.state.segments;
        // 三个参数：平移量、缩放比、铺满目标。都用整数档位（渲染器只吃 u32），
        // 显示时换算回秒 / 百分比。
        let shift_ms = self.retime_shift_ms;
        let scale_pct = self.retime_scale_pct;
        let target_secs = self.retime_target_secs;
        let op = self.retime_op;

        // 预览：按当前参数算一遍，把「会改几句 / 是否重叠」如实显示出来。
        let preview = match op {
            1 => Some(plan_retime(
                segments,
                TimeShift::ShiftAll {
                    delta: shift_ms as f64 / 1000.0,
                },
            )),
            2 => Some(plan_retime(
                segments,
                TimeShift::Scale {
                    source_span: 1.0,
                    target_span: scale_pct as f64 / 100.0,
                },
            )),
            3 => Some(plan_retime(
                segments,
                TimeShift::FitToDuration {
                    target_secs: target_secs as f64,
                },
            )),
            _ => None,
        };

        let preview_text = match (&preview, op) {
            (Some(plan), _) => {
                let shift_desc = match op {
                    1 => describe(TimeShift::ShiftAll {
                        delta: shift_ms as f64 / 1000.0,
                    }),
                    2 => format!("缩放 {:+.0}%", scale_pct as f64 - 100.0),
                    _ => format!("铺满 {target_secs} 秒"),
                };
                let mut t = format!("{shift_desc} · 将改 {} 句", plan.changed);
                if plan.overlaps {
                    // 重叠不是错误——应用时会自动压掉——但必须提前说，否则用户看到
                    // 「应用后句数没变但时间变了」会以为是 bug。
                    t.push_str(" · 检测到重叠，应用时自动压平");
                }
                if plan.degenerate {
                    t.push_str(" · 含过短片段，应用时会自纠到最短时长");
                }
                t
            }
            (None, _) => "选择一种调整方式".to_string(),
        };

        // 目标时长的默认建议：整轨已有时长与视频总时长差得远时，界面直接提示
        // （TXT 导入后必然命中）。
        let fit_hint = suggest_fit(segments, self.state.total_duration);

        let op_row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                primitives::segmented("整轨平移", op == 1, false)
                    .id("retime-op-shift")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 1;
                        cx.notify();
                    })),
            )
            .child(
                primitives::segmented("整体缩放", op == 2, false)
                    .id("retime-op-scale")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 2;
                        cx.notify();
                    })),
            )
            .child(
                primitives::segmented("铺满总时长", op == 3, false)
                    .id("retime-op-fit")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.retime_op = 3;
                        cx.notify();
                    })),
            );

        // 参数行：按当前方式只显示相关的那一项，避免三个滑条堆在一起让人不知道
        // 哪个在生效。
        let param_row = match op {
            1 => self
                .render_slider(
                    "retime-slider-shift",
                    "平移量",
                    // 档位 = (毫秒/1000 + 5) 取整，范围 0..60；负值也落在档位内。
                    ((shift_ms / 1000) + 5).clamp(0, 60) as u32,
                    0,
                    60,
                    format!("{:+.1} s", shift_ms as f64 / 1000.0),
                    "正数往后、负数往前；起点会被钳在 0 且保持时长不变".to_string(),
                    cx,
                    move |this, v, cx| {
                        // 滑条档位 0..60 秒 → -5.0..+55.0 秒，覆盖常见的修剪幅度。
                        this.retime_shift_ms = (v as i64 - 5) * 1000;
                        cx.notify();
                    },
                )
                .into_any_element(),
            2 => self
                .render_slider(
                    "retime-slider-scale",
                    "缩放比例",
                    scale_pct,
                    50,
                    150,
                    format!("{:.0}%", scale_pct),
                    "成片比字幕版本更长就把比例调大（时间轴等比拉伸）".to_string(),
                    cx,
                    move |this, v, cx| {
                        this.retime_scale_pct = v;
                        cx.notify();
                    },
                )
                .into_any_element(),
            3 => self
                .render_slider(
                    "retime-slider-target",
                    "目标总时长",
                    target_secs,
                    10,
                    3600,
                    crate::utils::time::format_duration_short(target_secs as f64),
                    fit_hint
                        .map(|_| {
                            format!(
                                "当前整轨 {}，与视频时长不符——建议铺满到 {}",
                                crate::utils::time::format_duration_short(
                                    self.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                                ),
                                crate::utils::time::format_duration_short(
                                    self.state.total_duration
                                )
                            )
                        })
                        .unwrap_or_else(|| "按各句时长比例重新铺满整轨".to_string()),
                    cx,
                    move |this, v, cx| {
                        this.retime_target_secs = v;
                        cx.notify();
                    },
                )
                .into_any_element(),
            _ => div().into_any_element(),
        };

        let can_apply = op != 0 && !segments.is_empty();
        let apply_label = match op {
            1 => "应用平移",
            2 => "应用缩放",
            3 => "铺满时间轴",
            _ => "应用",
        };
        let apply_row = div()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(preview_text),
            )
            .child(
                primitives::mini_btn(apply_label, can_apply)
                    .id("retime-apply")
                    .when(can_apply, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            this.apply_retime(cx);
                        }))
                    }),
            );

        primitives::card_sm()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(primitives::section_title("整轨时间轴调整")),
            )
            .child(op_row)
            .child(param_row)
            .child(apply_row)
    }

    /// 把当前参数对应的整轨变换真正写回字幕，并自纠重叠。
    ///
    /// 与 `render_retime_card` 的预览用**同一份** `plan_retime` 调用参数，避免
    /// 「预览说改 190 句、应用却改了 189 句」这种自相矛盾。
    fn apply_retime(&mut self, cx: &mut Context<Self>) {
        use crate::subtitle::timing::{plan_retime, resolve_overlaps, TimeShift};
        if self.state.segments.is_empty() {
            return;
        }
        let shift = match self.retime_op {
            1 => TimeShift::ShiftAll {
                delta: self.retime_shift_ms as f64 / 1000.0,
            },
            2 => TimeShift::Scale {
                source_span: 1.0,
                target_span: self.retime_scale_pct as f64 / 100.0,
            },
            3 => TimeShift::FitToDuration {
                target_secs: self.retime_target_secs as f64,
            },
            _ => return,
        };
        let plan = plan_retime(&self.state.segments, shift);
        // 应用前记快照：整轨操作一改几百句，点错了必须有路可退。
        self.state.snapshot_for_undo();
        let resolved = resolve_overlaps(&plan.times);
        for (seg, (start, end)) in self.state.segments.iter_mut().zip(resolved) {
            seg.start = start;
            seg.end = end;
        }
        let changed = plan.changed;
        self.state.bump_segments_revision();
        // 时间轴变了：波形/监视器的定位与质检缓存都要跟着重算。
        self.subtitle_filter_key = None;
        self.notice = Some(format!(
            "已调整整轨时间轴（{changed} 句变动{}）",
            if plan.overlaps {
                "，重叠已压平"
            } else {
                ""
            }
        ));
        cx.notify();
    }

    /// 字幕统计卡：整篇的时长 / 字数 / 语速 / 过长句。
    ///
    /// # 为什么这些数字值得单独一栏
    ///
    /// 「阅读速度（CPS）」与「单行过长」是字幕交付的两条硬指标——被平台或客户打回
    /// 的常见原因就是「这句字幕一闪而过看不清」。此前用户只能一行行目测，现在给出
    /// 整篇的极值与超标计数，并直接标出**是哪一句**（可点，跳过去改）。
    ///
    /// # 为什么按字符集分档
    ///
    /// CPS 对汉字与对英文字母不是一个量纲：一个汉字的信息量远大于一个字母。用同一个
    /// 阈值会把中文全判成「过快」或把英文全判成「舒适」，两头都不准。分档逻辑在
    /// `subtitle::stats::ReadingSpeed::classify`（纯函数、有单测），这里只负责展示。
    fn render_subtitle_stats_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::subtitle::stats::{compute, ReadingSpeed};
        let style = self.state.config.subtitle_style.clone();
        // 媒体总时长：优先用工程里记录的（`total_duration`），没有则退回探测值。
        // 两者都为 0 时 `spoken_ratio` 会返回 0，界面照常显示而不出 NaN。
        let media_secs = if self.state.total_duration > 0.0 {
            self.state.total_duration
        } else {
            self.state.transcribe_duration
        };
        // `max_chars_per_line` 是 u32（样式配置），stats 收 usize——显式转换，
        // 不用 `as`（u32→usize 在 32 位平台也是无损的，但 `try_into` 更直白）。
        let max_chars = usize::try_from(style.max_chars_per_line).unwrap_or(usize::MAX);
        let stats = compute(&self.state.segments, media_secs, max_chars);
        let max_cps_idx = stats.max_cps_index;
        let longest_idx = stats.longest_index;
        let too_fast = stats.too_fast_count;
        let long_lines = stats.long_line_count;

        // 统计行：标签 + 数值。`bad` 为真时用红色——只给需要用户动手的数字上色，
        // 全绿或全红都等于没有信号。
        let row = |label: &'static str, value: String, bad: bool| -> AnyElement {
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(Theme::SPACE_4))
                .py(px(Theme::SPACE_1))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_secondary())
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if bad {
                            Theme::accent_red()
                        } else {
                            Theme::text_primary()
                        })
                        .child(value),
                )
                .into_any_element()
        };

        let mut card = primitives::card_rows()
            .child(primitives::setting_row(
                "字幕统计",
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(format!("共 {} 句", stats.segment_count))
                    .into_any_element(),
            ))
            .child(primitives::divider());

        if stats.segment_count == 0 {
            return card.child(
                div()
                    .py(px(Theme::SPACE_2))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("还没有字幕，先在「智能转写」生成"),
            );
        }

        card = card
            .child(row(
                "原文总字数",
                format!("{} 字", stats.total_chars),
                false,
            ))
            .child(row(
                "已翻译",
                format!("{} / {} 句", stats.translated_count, stats.segment_count),
                false,
            ))
            .child(row(
                "有语音时长",
                format!(
                    "{}（占全片 {:.0}%）",
                    crate::utils::time::format_duration_short(stats.spoken_secs),
                    stats.spoken_ratio() * 100.0
                ),
                false,
            ))
            .child(row(
                "平均语速",
                format!("{:.1} 字/秒", stats.average_cps()),
                false,
            ))
            .child(row(
                "最快一句",
                format!("{:.1} 字/秒", stats.max_cps),
                stats
                    .max_cps_index
                    .and_then(|i| self.state.segments.iter().find(|s| s.index == i))
                    .and_then(|s| ReadingSpeed::classify(s.display_text(), s.duration()))
                    == Some(ReadingSpeed::TooFast),
            ));

        // 两个「有问题」的数字做成可点的胶囊：点一下跳到那一句，用户才知道去改哪。
        // 零项时保留计数但置灰——用户要看得出「这一项确实检查过、结果是 0」。
        // 跳转目标取极值句（语速过快 → 最快那句；单行过长 → 最长那句）：stats 只给
        // 极值，要精确到「第一句过长的」得再遍历一次，而极值句通常就是要改的那句。
        let mut issues = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(Theme::SPACE_2));

        if too_fast == 0 {
            issues = issues.child(primitives::tag_tinted(
                "语速过快 0 句",
                Theme::tint_neutral(),
                Theme::tint_neutral_border(),
                Theme::text_muted(),
            ));
        } else {
            let el = primitives::tag_tinted(
                format!("语速过快 {too_fast} 句"),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            )
            .id("stats-issue-too-fast");
            issues = issues.child(match max_cps_idx {
                Some(idx) => el
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_segment_for_stats(idx, cx);
                    }))
                    .into_any_element(),
                None => el.into_any_element(),
            });
        }

        if long_lines == 0 {
            issues = issues.child(primitives::tag_tinted(
                "单行过长 0 句",
                Theme::tint_neutral(),
                Theme::tint_neutral_border(),
                Theme::text_muted(),
            ));
        } else {
            let el = primitives::tag_tinted(
                format!("单行过长 {long_lines} 句"),
                Theme::tint_red_soft(),
                Theme::tint_red_border(),
                Theme::accent_red(),
            )
            .id("stats-issue-long-line");
            issues = issues.child(match longest_idx {
                Some(idx) => el
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to_segment_for_stats(idx, cx);
                    }))
                    .into_any_element(),
                None => el.into_any_element(),
            });
        }

        if let Some(idx) = longest_idx {
            issues = issues.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .child(format!("最长句为第 {idx} 句（{} 字）", stats.max_chars)),
            );
        }

        card.child(issues).child(
            div()
                .pt(px(Theme::SPACE_2))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(Theme::text_muted())
                .child(format!(
                    "语速上限按字符集分档：中日韩约 9 字/秒、拉丁约 17 字/秒；单行超过 {} 字判为过长（按可折两行算）",
                    style.max_chars_per_line.saturating_mul(2)
                )),
        )
    }

    /// 统计卡里的「跳到这一句」：选中目标句、强制字幕清单重新定位、刷新监视器帧。
    ///
    /// 与质检卡的 `jump_to_quality_issue` 同法但**不切页**——统计卡就在剪辑台里，
    /// 用户点它是想看一眼那一句，不是想离开当前页面。
    fn jump_to_segment_for_stats(&mut self, index: usize, cx: &mut Context<Self>) {
        self.state.select_segment(index);
        // 置 `None` 强制清单重新滚动到选中行：只在列表内部改选中不会自动跟随。
        self.subtitle_list_followed_sel = None;
        // 监视器要显示这一句对应的画面，否则用户还得自己在时间轴上找。
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 导出卡片：导出内容 / 文件名模板 / 编辑日志 / 格式选择 + 导出按钮。
    ///
    /// 它原本是**常驻在右侧面板最底部**的一条底栏（`render_editor_export_dock`），
    /// 无论切到哪个面板都占着约 150px 高。代价是那 150px 从右侧面板的可用高度里
    /// 切走，字幕清单 / 统计 / 翻译卡都因此少显示几行；而导出本身是**低频动作**，
    /// 不值得常驻。
    ///
    /// 现在收进「统计与导出」面板，成为该面板的第三张卡：三个面板各自占满整个
    /// 右侧高度，可用空间更宽裕、能多显示内容；导出就在同一页，要用时顺手可及。
    ///
    /// 下拉菜单放在触发行**之后**（向下展开）：本卡在可滚动面板里，向下展开比
    /// 向上顶出更符合阅读方向，也不会一开菜单就把上面的内容挤动。
    fn render_export_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.is_export_dropdown_open;
        let cur_fmt = self.editor_export_format;
        let cur_mode = self.editor_export_mode;

        primitives::card_sm()
            .flex_none()
            .gap(px(Theme::SPACE_2))
            // 卡头：标题 + 本次会话改动数（原先挂在编辑日志那一排尾部）
            .child(primitives::setting_row(
                "导出与留档",
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child(format!("本次会话 {} 条改动", self.state.edit_log.len()))
                    .into_any_element(),
            ))
            .child(primitives::divider())
            // 导出内容：原文 / 仅译文 / 双语。对字幕与剪辑工程文件统一生效。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("导出内容"),
                    )
                    .child({
                        let modes = [
                            (ExportMode::RawOnly, "仅原文"),
                            (ExportMode::TranslationOnly, "仅译文"),
                            (ExportMode::Bilingual, "双语对照"),
                        ];
                        div()
                            .flex()
                            .gap_1()
                            .children(modes.into_iter().enumerate().map(|(idx, (mode, label))| {
                                let selected = mode == cur_mode;
                                primitives::chip_clickable(label, selected, false)
                                    .id(("export-mode-opt", idx))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.editor_export_mode = mode;
                                        // 落盘：下次启动仍用同一模式，避免「选项没生效」的错觉
                                        this.state.set_export_mode(mode);
                                        cx.notify();
                                    }))
                            }))
                    }),
            )
            // 文件名模板：`{name}` / `{ext}` / `{date}`。同一部片子常要交付多份
            // （给剪辑的、给外语同事的、按日期归档的），此前只能导出后手工改名。
            // 默认 `{name}.{ext}` = 历史行为，不动这个输入框的用户完全无感。
            .child({
                let tpl = self.state.config.ui.export_name_template.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("文件名模板"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .child(self.render_export_name_template_input(cx)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child(if tpl.trim() == "{name}.{ext}" {
                                "占位符：{name} {ext} {date}"
                            } else {
                                "例：{name}.{date}.{ext}"
                            }),
                    )
            })
            // 编辑日志：与「导出字幕」同一排，因为它导出的是**这次编辑过程的记录**，
            // 属于「交付/留档」这一类动作。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .flex_shrink_0()
                            .child("编辑日志"),
                    )
                    .child(
                        primitives::chip_clickable("CSV", false, false)
                            .id("export-log-csv")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_edit_log(crate::subtitle::qc::QcFormat::Csv, cx);
                            })),
                    )
                    .child(
                        primitives::chip_clickable("Markdown", false, false)
                            .id("export-log-md")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_edit_log(crate::subtitle::qc::QcFormat::Markdown, cx);
                            })),
                    ),
            )
            // 格式选择 + 导出按钮
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        div()
                            .id("export-format-dropdown-trigger")
                            .flex_1()
                            .h(px(Theme::CTRL_H_LG))
                            .px(px(Theme::SPACE_4))
                            .rounded(px(Theme::RADIUS_LG))
                            .bg(Theme::bg_card())
                            .border_1()
                            .border_color(if is_open {
                                Theme::accent_mint()
                            } else {
                                Theme::border()
                            })
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
                                    .text_size(px(Theme::TEXT_BODY_LG))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(cur_fmt.label()),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_secondary())
                                    .child(if is_open { "▲" } else { "▼" }),
                            ),
                    )
                    .child({
                        // 无字幕时置灰：导出链路（`perform_editor_export` 等）在没有
                        // segments 时都是静默 return，按钮却照常可点，用户会以为程序坏了。
                        // 同页的 mini_btn 早已有禁用态规范，导出按钮此前漏了这一层。
                        let can_export = !self.state.segments.is_empty();
                        primitives::btn_state(
                            "导出",
                            primitives::BtnSize::Lg,
                            primitives::BtnVariant::Primary,
                            can_export,
                        )
                        .id("editor-do-export-btn")
                        .px(px(Theme::SPACE_6))
                        .when(can_export, |d| {
                            d.on_click(cx.listener(|this, _, _, cx| {
                                this.is_export_dropdown_open = false;
                                this.perform_editor_export(cx);
                            }))
                        })
                    }),
            )
            // 格式下拉菜单（打开时）：放在触发行之后向下展开。
            .child(if is_open {
                div()
                    .id("export-format-menu")
                    .rounded_lg()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .p_1()
                    .max_h(px(Theme::DROPDOWN_MAX_H))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .children(
                        EditorExportFormat::all()
                            .iter()
                            .enumerate()
                            .map(|(idx, fmt)| {
                                let fmt = *fmt;
                                let is_selected = fmt == cur_fmt;
                                div()
                                    .id(("export-fmt-opt", idx))
                                    .px_2p5()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .bg(if is_selected {
                                        Theme::tint_mint_badge()
                                    } else {
                                        Theme::transparent()
                                    })
                                    .hover(|s| s.bg(Theme::bg_hover()))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.editor_export_format = fmt;
                                        // 落盘：导出格式是长期偏好，不存的话重启就
                                        // 跳回默认，用户会以为选择没生效。
                                        this.state.config.ui.export_format =
                                            fmt.as_str().to_string();
                                        let _ = this.state.config.save_to_file("config.toml");
                                        this.is_export_dropdown_open = false;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(if is_selected {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .text_color(if is_selected {
                                                Theme::accent_mint()
                                            } else {
                                                Theme::text_primary()
                                            })
                                            .child(fmt.label()),
                                    )
                                    .child(if is_selected {
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child("[当前]")
                                    } else {
                                        div()
                                    })
                            }),
                    )
            } else {
                div().id("export-format-menu-closed")
            })
    }

    /// 查找替换面板（展开时才有内容；收起时返回空元素）。
    ///
    /// 真正的替换逻辑在 `subtitle::edit::replace_all`（纯函数、13 条单测）；这里只
    /// 负责输入、选项、执行与结果反馈。**不在这里拼任何字符串替换逻辑**——那正是
    /// 此前每个调用点各写一遍、大小写与译文保护各不相同的来源。
    ///
    /// 为什么替换前要记一次撤销快照：替换动辄改几十句，用户点错了必须有路可退。
    /// 走 `state.push_undo_snapshot()`（与所有编辑入口同一套），不另造机制。
    fn render_replace_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::subtitle::edit::{replace_all, ReplaceOptions};
        if !self.replace_panel_open {
            return None;
        }
        let find = self.replace_find.clone();
        let with = self.replace_with.clone();
        let case_sensitive = self.replace_case_sensitive;
        let include_translation = self.replace_include_translation;
        let status = self.replace_status.clone();
        let can_apply = !find.is_empty() && !self.state.segments.is_empty();

        // 两个自绘输入框：与 `render_api_input` 同一套最小可用编辑集合
        // （可打印字符 + 退格/方向/Home/End + Ctrl+A/C/V，见 `apply_line_edit`）。
        // 抽成闭包是因为两个框除了「绑哪个缓冲、哪个焦点」以外完全一样。
        let input = |id: &'static str,
                     field: crate::ui::ReplaceField,
                     value: &str,
                     placeholder: &str,
                     cx: &mut Context<Self>|
         -> AnyElement {
            use crate::ui::ReplaceField;
            let focus = match field {
                ReplaceField::Find => self.replace_find_focus.clone(),
                ReplaceField::With => self.replace_with_focus.clone(),
            };
            let is_focused = self.focused_replace_field == Some(field);
            let char_count = value.chars().count();
            let cursor = self.replace_cursor.min(char_count);
            let before: String = value.chars().take(cursor).collect();
            let after: String = value.chars().skip(cursor).collect();
            primitives::text_input(is_focused, 120.0)
                .id(id)
                .track_focus(&focus)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        window.focus(&focus);
                        this.focused_replace_field = Some(field);
                        this.replace_cursor = match field {
                            ReplaceField::Find => this.replace_find.chars().count(),
                            ReplaceField::With => this.replace_with.chars().count(),
                        };
                        cx.notify();
                    }),
                )
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.replace_panel_open = false;
                        this.replace_status = None;
                        cx.notify();
                        return;
                    }
                    let buffer = match field {
                        ReplaceField::Find => &mut this.replace_find,
                        ReplaceField::With => &mut this.replace_with,
                    };
                    let cursor = &mut this.replace_cursor;
                    if apply_line_edit(buffer, cursor, event, cx) {
                        cx.notify();
                    }
                }))
                .child(if is_focused {
                    div()
                        .flex()
                        .items_center()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_primary())
                        .child(before)
                        .child(div().text_color(Theme::accent_blue()).child("▌"))
                        .child(after)
                        .into_any_element()
                } else if char_count == 0 {
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_muted())
                        .truncate()
                        .child(placeholder.to_string())
                        .into_any_element()
                } else {
                    div()
                        .text_size(px(Theme::TEXT_SMALL))
                        .text_color(Theme::text_primary())
                        .truncate()
                        .child(value.to_string())
                        .into_any_element()
                })
                .into_any_element()
        };

        let mut row = div()
            .w_full()
            .px(px(Theme::SPACE_3))
            .py(px(Theme::SPACE_2))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("查找"),
            )
            .child(div().flex_1().min_w(px(120.0)).child(input(
                "replace-find-input",
                crate::ui::ReplaceField::Find,
                &find,
                "要查找的文字",
                cx,
            )))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("替换为"),
            )
            .child(div().flex_1().min_w(px(120.0)).child(input(
                "replace-with-input",
                crate::ui::ReplaceField::With,
                &with,
                "替换成（可留空＝删除）",
                cx,
            )));

        // 两个勾选：大小写、译文。用 mini_btn 的选中态表达，不引入新控件族。
        row = row
            .child(
                primitives::mini_btn(
                    if case_sensitive {
                        "✓ 区分大小写"
                    } else {
                        "区分大小写"
                    },
                    true,
                )
                .id("replace-toggle-case")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_case_sensitive = !this.replace_case_sensitive;
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn(
                    if include_translation {
                        "✓ 含译文"
                    } else {
                        "含译文"
                    },
                    true,
                )
                .id("replace-toggle-translation")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.replace_include_translation = !this.replace_include_translation;
                    cx.notify();
                })),
            )
            .child(
                primitives::mini_btn("全部替换", can_apply)
                    .id("replace-apply")
                    .when(can_apply, |d| {
                        d.on_click(cx.listener(move |this, _, _, cx| {
                            // 先记快照：替换可能一次改几十句，点错必须有路可退。
                            this.state.snapshot_for_undo();
                            let options = ReplaceOptions {
                                case_sensitive: this.replace_case_sensitive,
                                include_translation: this.replace_include_translation,
                            };
                            let report = replace_all(
                                &mut this.state.segments,
                                &this.replace_find,
                                &this.replace_with,
                                &options,
                            );
                            this.replace_status = Some(if report.is_empty() {
                                (false, "没有找到匹配的内容".to_string())
                            } else {
                                let mut msg = format!(
                                    "已替换 {} 处，涉及 {} 句",
                                    report.total_hits(),
                                    report.changed.len()
                                );
                                if report.translation_hits > 0 {
                                    msg.push_str(&format!(
                                        "（其中译文 {} 处）",
                                        report.translation_hits
                                    ));
                                }
                                (true, msg)
                            });
                            // 内容变了：字幕清单的过滤结果与质检缓存都要跟着重算。
                            this.state.bump_segments_revision();
                            this.subtitle_filter_key = None;
                            cx.notify();
                        }))
                    }),
            )
            .child(
                primitives::mini_btn("关闭", true)
                    .id("replace-close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.replace_panel_open = false;
                        this.replace_status = None;
                        cx.notify();
                    })),
            );

        if let Some((ok, msg)) = status {
            row = row.child(
                div()
                    .flex_basis(relative(1.0))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if ok {
                        Theme::accent_mint()
                    } else {
                        Theme::accent_orange()
                    })
                    .child(msg),
            );
        }
        Some(row.into_any_element())
    }

    /// 导出文件名模板输入框（走与 API 输入框同一套最小编辑集合）。
    ///
    /// 校验**故意宽松**：模板是自由文本，任何「看起来不对」的值都仍能导出（最差
    /// 只是名字不合心意），为此弹红字只会打扰。真正的兜底在
    /// `writer::export_file_name`：空模板回落默认、无占位符时补扩展名。
    fn render_export_name_template_input(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.export_template_focus.clone();
        let is_focused = self.export_template_focused;
        let value = self.state.config.ui.export_name_template.clone();
        let char_count = value.chars().count();
        let cursor = self.export_template_cursor.min(char_count);
        let before: String = value.chars().take(cursor).collect();
        let after: String = value.chars().skip(cursor).collect();
        primitives::text_input(is_focused, 120.0)
            .id("export-name-template-input")
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.export_template_cursor =
                        this.state.config.ui.export_name_template.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let buffer = &mut this.state.config.ui.export_name_template;
                let cursor = &mut this.export_template_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    let _ = this.state.config.save_to_file("config.toml");
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_blue()).child("▌"))
                    .child(after)
                    .into_any_element()
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child("{name}.{ext}")
                    .into_any_element()
            } else {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(value)
                    .into_any_element()
            })
            .into_any_element()
    }

    /// 字幕清单的搜索输入框。
    ///
    /// Esc 一键清空（比按住退格删十个字快得多），其余按键复用 [`apply_line_edit`]
    /// 的最小可用编辑集合。搜索结果是渲染时按需重算的，这里只需改缓冲并重绘。
    fn render_search_input(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let is_focused = self.subtitle_search_focused;
        let focus = self.subtitle_search_focus.clone();
        let raw = self.subtitle_search.clone();
        let char_count = raw.chars().count();
        let cursor = self.subtitle_search_cursor.min(char_count);
        let before: String = raw.chars().take(cursor).collect();
        let after: String = raw.chars().skip(cursor).collect();

        div()
            .id("subtitle-search-input")
            .flex_1()
            .min_w(px(Theme::SEARCH_MIN_W))
            .h(px(Theme::CTRL_H_XS))
            .px(px(Theme::SPACE_2))
            .rounded(px(Theme::RADIUS_MD))
            .bg(Theme::bg_input())
            .border_1()
            .border_color(if is_focused {
                Theme::accent_mint()
            } else {
                Theme::border_mid()
            })
            .cursor_text()
            .flex()
            .items_center()
            .overflow_hidden()
            .track_focus(&focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&focus);
                    this.subtitle_search_focused = true;
                    this.subtitle_search_cursor = this.subtitle_search.chars().count();
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.subtitle_search.clear();
                    this.subtitle_search_cursor = 0;
                    cx.notify();
                    return;
                }
                let buffer = &mut this.subtitle_search;
                let cursor = &mut this.subtitle_search_cursor;
                if apply_line_edit(buffer, cursor, event, cx) {
                    cx.notify();
                }
            }))
            .child(if is_focused {
                div()
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .child(before)
                    .child(div().text_color(Theme::accent_mint()).child("▌"))
                    .child(after)
            } else if char_count == 0 {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_muted())
                    .child("输入关键字过滤原文或译文，Esc 清空")
            } else {
                div()
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_primary())
                    .truncate()
                    .child(raw)
            })
            .into_any_element()
    }

    /// 单选胶囊（翻译引擎档位 / 目标语言共用）。
    fn render_choice_pill(
        id: (&'static str, usize),
        label: &str,
        is_sel: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        primitives::chip_clickable(label.to_string(), is_sel, false)
            .id(id)
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
    }

    /// 字幕多语言翻译面板：引擎档位、目标语言、进度与一键翻译。
    ///
    /// 翻译链路（本地 Qwen / 在线 OpenAI 兼容 API）与 `AppState` 里的进度字段
    /// 此前都已就绪，缺的只是这个入口——没有它，「翻译字幕」列永远是「—」。
    fn render_translate_card(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::app::state::TRANSLATE_TARGET_LANGS;
        use crate::engines::TranslateMode;

        let mode = self.state.translate_mode;
        let target = self.state.translate_target_lang.clone();
        let is_translating = self.state.is_translating;
        let progress = self.state.translate_progress.clamp(0.0, 1.0);
        let status = self.state.translate_status_msg.clone();
        // 「已完成」只认**实际译出**：`has_translation()` 要求译文非空（忽略空串/纯空白），
        // 再叠加目标语言匹配。只要还有译文为空/纯空白（哪怕引擎给它打了
        // `translation_lang`），这里就不会凑满 total，界面绝不会显示「已全部翻译」。
        let done = translated_out_count(&self.state.segments, &target);
        // 分母只算**源文非空**的句子（与引擎的可译口径、与收尾文案一致）：
        // 空源句（静音段/纯空白行）永远不会有译文，计进分母就会永远差几句。
        let expected = super::actions::expected_translation_count(&self.state.segments);
        let total = self.state.segments.len();
        // 可点的前提是**有可翻译的文本**：整片都是空源句时点下去只会
        // 得到「没有可翻译的文本」，不如直接置灰。
        let can_run = expected > 0 && !is_translating;
        // 已带译文、但**不是**当前目标语言的句数。
        // 有了它才能解释「为什么按钮不是『重新翻译』」：用户切换目标语言后
        // 旧译文仍然在，但当前语言一句都没有——不提示的话，界面看起来像
        // 「翻译记录丢了」，用户会以为程序把之前的成果清空了。
        let other_lang = self
            .state
            .segments
            .iter()
            .filter(|s| s.has_translation() && !s.translation_matches(&target))
            .count();
        let has_any_trans_overall = self.state.segments.iter().any(|s| s.has_translation());
        let has_distinct_trans = self.state.segments.iter().any(|s| {
            if let Some(t) = &s.translation {
                !t.trim().is_empty() && t.trim() != s.display_text().trim()
            } else {
                false
            }
        });

        // 引擎档位：本地 Qwen 免费离线，在线 API 更快更好但需要密钥
        let mode_row = div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(Theme::FORM_LABEL_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .child("翻译引擎"),
            )
            .child(Self::render_choice_pill(
                ("translate-mode", 0),
                TranslateMode::OfflineQwen.label(),
                mode == TranslateMode::OfflineQwen,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OfflineQwen);
                    cx.notify();
                },
            ))
            .child(Self::render_choice_pill(
                ("translate-mode", 1),
                TranslateMode::OnlineApi.label(),
                mode == TranslateMode::OnlineApi,
                cx,
                |this, cx| {
                    this.state
                        .set_translate_mode(crate::engines::TranslateMode::OnlineApi);
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(Theme::text_muted())
                    .truncate()
                    .child(if mode == TranslateMode::OnlineApi {
                        let model = self.state.config.translate.api_model.clone();
                        if self
                            .state
                            .config
                            .translate
                            .effective_api_key()
                            .trim()
                            .is_empty()
                        {
                            "未配置 API Key，请在「性能设置」中填写".to_string()
                        } else {
                            format!("模型 {}", model)
                        }
                    } else {
                        // 离线链路需要两件东西：Qwen 模型与 llama.cpp 推理程序。
                        // 缺哪个就点名哪个，避免用户点「开始翻译」后才收到错误；
                        // 两者均缺时先提模型（体积大、下载慢，先知道更有心理预期）。
                        // 模型路径可能是用户自备的（性能设置里可浏览 / 粘贴），
                        // 因此文案不能只说「未下载」——自备路径填错时同样会命中这里，
                        // 说「未就位」并给出两条出路（去模型页下载 / 去性能设置核对路径）
                        // 才覆盖得住。
                        let miss_model = !self.state.model_is_present("qwen-llm");
                        let miss_cli = !self.state.model_is_present("llama-cpp");
                        if miss_model {
                            "本地模型未就位，请在「性能设置」核对路径或到「模型与组件」下载"
                                .to_string()
                        } else if miss_cli {
                            "未下载 llama.cpp 推理程序，请在「性能设置 → 模型与组件」下载"
                                .to_string()
                        } else {
                            "本地 Qwen，无需密钥".to_string()
                        }
                    }),
            );

        // 目标语言：8 个常用语种，窄面板下自动折行
        let lang_row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_1()
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(Theme::FORM_LABEL_W))
                    .text_size(px(Theme::TEXT_SMALL))
                    .text_color(Theme::text_secondary())
                    .child("目标语言"),
            )
            .children(TRANSLATE_TARGET_LANGS.iter().enumerate().map(|(i, lang)| {
                let is_sel = target == *lang;
                let lang_owned = lang.to_string();
                Self::render_choice_pill(
                    ("translate-lang", i),
                    lang,
                    is_sel,
                    cx,
                    move |this, cx| {
                        // 走 setter 而非直接赋值：目标语言是长期偏好，必须落盘，
                        // 否则重启后跳回默认语言，用户会以为选择没生效。
                        this.state.set_translate_target_lang(&lang_owned);
                        cx.notify();
                    },
                )
            }));

        let action_row = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_2()
            // 机翻难免有出入：选中某句后可直接订正译文（走系统输入框，支持中文输入法）。
            // 放在翻译卡里，是因为用户就是在这一面板发现某句译得不对。
            .child(if self.state.selected_segment_index.is_some() {
                primitives::btn_clickable(
                    "改译文",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-edit-selected")
                .on_click(cx.listener(|this, _, window, cx| {
                    if let Some(idx) = this.state.selected_segment_index {
                        let trans = this.state.segments.iter().find(|s| s.index == idx)
                            .and_then(|s| s.translation.clone())
                            .unwrap_or_default();
                        this.start_inline_edit(idx, true, &trans, window, cx);
                    }
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                div()
                    .id("btn-translate-subtitles")
                    .flex_shrink_0()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(if can_run {
                        Theme::accent_mint()
                    } else {
                        Theme::bg_disabled()
                    })
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(if can_run {
                        Theme::text_on_accent()
                    } else {
                        Theme::text_disabled()
                    })
                    .when(can_run, |s| {
                        s.cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.trigger_llm_translation(cx);
                            }))
                    })
                    .child(if is_translating {
                        "翻译中…".to_string()
                    } else if expected > 0 && done == expected {
                        // 当前语言应译的句子全部译出（`done` 只数**实际译出**的句子，
                        // 空/纯空白译文永远凑不满）：再点只会被引擎的增量
                        // 逻辑判为「无需翻译」，文案说清楚，避免用户以为按钮失灵。
                        "已全部翻译".to_string()
                    } else if done > 0 {
                        // 部分完成（含取消后继续、或补译新句）——增量翻译，
                        // 不会把已完成的部分重译一遍。
                        "继续翻译".to_string()
                    } else if other_lang > 0 {
                        // 有别的语言的译文、当前语言一句都没有：这是「换语言」场景，
                        // 按钮要说清是「译成新语言」而不是「重新翻译」。
                        format!("译为{}", target)
                    } else {
                        "开始翻译".to_string()
                    }),
            )
            // 翻译中才出现的「取消」：离线 Qwen 模型路径不对、在线 API 长时间无响应时，
            // 此前只能强杀进程——按钮变成灰的「翻译中…」且不可点，是条纯死路。
            // 取消只需置位一个 AtomicBool，引擎在**每个批次之间**检查，最多损失当前批。
            .child(if is_translating {
                primitives::btn_clickable(
                    "取消",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_llm_translation(cx);
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            // 存在译文且未在翻译中时，提供一键清空译文入口，方便恢复纯净单语状态
            .child(if has_any_trans_overall && !is_translating {
                primitives::btn_clickable(
                    "清空译文",
                    primitives::BtnSize::Sm,
                    primitives::BtnVariant::Secondary,
                )
                .id("btn-translate-clear-all")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.clear_all_translations(cx);
                }))
                .into_any_element()
            } else {
                div().into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(if status.contains("失败") {
                        Theme::accent_red()
                    } else {
                        Theme::text_muted()
                    })
                    .truncate()
                    .child(if status.is_empty() {
                        if total == 0 {
                            "先在「智能转写」生成字幕，再回到这里翻译".to_string()
                        } else if expected > 0 && done == expected {
                            // 同上：`done` 只认实际译出，空/纯空白译文不会被
                            // 当成「已全部翻译为{target}」。
                            if !has_distinct_trans && has_any_trans_overall {
                                "已处理完毕（译文同原文，画面与列表已自动单行显示；若无需译文可点「清空译文」）".to_string()
                            } else {
                                format!("已全部翻译为{target}")
                            }
                        } else if expected == 0 {
                            // 有字幕但没有任何非空源文：没有可翻译的内容
                            "所有字幕行都为空，没有可翻译的文本".to_string()
                        } else if other_lang > 0 && done == 0 {
                            // 关键提示：换语言后旧译文仍在，只是不属于当前目标语言。
                            // 不说明的话，界面看起来像翻译记录被清空了。
                            format!("已有 {other_lang} 句其他语言译文；点上方按钮可译成{target}")
                        } else if done > 0 {
                            format!("{target}译文 {done}/{expected} 句，可继续补译")
                        } else {
                            format!("尚未翻译（共 {expected} 句）")
                        }
                    } else {
                        status
                    }),
            );

        primitives::card_sm()
            .flex_none()
            .gap(px(Theme::SPACE_2))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("字幕翻译"),
                    )
                    .child(
                        // 计数徽标走 primitives::count_tag，仅按完成量覆盖文字色。
                        // 分母用**应有译文句数**（排除空源句），否则与收尾文案不一致。
                        primitives::count_tag(format!("{}/{} 句已翻译", done, expected)).text_color(
                            if done > 0 {
                                Theme::accent_mint()
                            } else {
                                Theme::text_secondary()
                            },
                        ),
                    ),
            )
            .child(mode_row)
            .child(lang_row)
            .child(action_row)
            // 术语表状态 + 快速入口：翻译就在这一面板发生，把「固定译法」的入口放在这里
            // 最顺手。点击直接跳到设置页的术语表编辑（打开系统编辑器改）。
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_SMALL))
                            .text_color(Theme::text_secondary())
                            .child("术语表"),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child({
                                let n = self.state.glossary_entries().len();
                                if n == 0 {
                                    "未设置".to_string()
                                } else {
                                    // 显示**实际生效**条数：`glossary_prompt` 只注入前
                                    // `MAX_GLOSSARY_ENTRIES` 条，超过的部分静默丢弃，
                                    // 不提示的话用户会以为 120 条全在生效。
                                    let limit =
                                        self.state.config.translate.effective_glossary_limit();
                                    let effective =
                                        super::views::performance::effective_glossary_count(n, limit);
                                    let cap_note = if effective < n {
                                        format!(
                                            "，其中 {effective} 条生效（受上限 {limit} 条限制）"
                                        )
                                    } else {
                                        String::new()
                                    };
                                    let bad = self.cached_glossary_violations().len();
                                    if bad > 0 {
                                        format!(
                                            "已启用 {n} 条{cap_note} · {bad} 句疑似未按术语译（见琥珀色行）"
                                        )
                                    } else {
                                        format!("已启用 {n} 条{cap_note}（专名/术语按固定译法）")
                                    }
                                }
                            }),
                    )
                    .child(
                        primitives::chip_clickable("编辑术语表", false, false)
                            .id("editor-glossary-edit")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_glossary_editor(cx);
                            })),
                    )
                    .child(
                        primitives::chip_clickable("应用", false, false)
                            .id("editor-glossary-apply")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.apply_glossary_from_file(cx);
                            })),
                    ),
            )
            // 翻译进行中才出现进度条，空闲时不占版面
            .child(if is_translating {
                div()
                    .w_full()
                    .h(px(Theme::PROGRESS_H))
                    .rounded_full()
                    .bg(Theme::bg_inset())
                    .border_1()
                    .border_color(Theme::border())
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(relative(progress))
                            .bg(Theme::accent_mint()),
                    )
            } else {
                div()
            })
    }

    /// 属性面板里的紧凑次级按钮（拆分 / 合并 / 时间微调共用同一套外观）。
    ///
    /// `enabled = false` 时置灰且不响应点击，用于「最后一句无法合并」这类
    /// 点了也没反应的操作，避免用户误以为程序坏了。
    fn render_mini_btn(
        id: &'static str,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let base = primitives::mini_btn(label, enabled).id(id);
        if enabled {
            base.on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
        } else {
            base
        }
    }

    /// 标点快捷注入按钮 (iOS Pill Chip)
    fn render_punct_btn(
        &mut self,
        punct: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        primitives::chip_clickable(punct, false, false)
            .id(punct)
            .text_color(Theme::text_primary())
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
}

/// 把任意长度的峰值序列重采样成 `bars` 根等宽柱体，每根取所在窗口的**最大值**。
///
/// 取最大值而非平均值：包络上的「毛刺」正是人眼用来定位语音起止的特征，
/// 平均会把它们抹平成一条没有信息量的平滑曲线。
fn resample_peaks(peaks: &[f32], bars: usize) -> Vec<f32> {
    if peaks.is_empty() || bars == 0 {
        return Vec::new();
    }
    if peaks.len() <= bars {
        // 桶数比柱数还少（极短视频）时按比例拉伸复用，避免出现空柱
        return (0..bars).map(|i| peaks[i * peaks.len() / bars]).collect();
    }
    (0..bars)
        .map(|i| {
            let lo = i * peaks.len() / bars;
            let hi = (((i + 1) * peaks.len()) / bars)
                .max(lo + 1)
                .min(peaks.len());
            peaks[lo..hi].iter().copied().fold(0.0f32, f32::max)
        })
        .collect()
}

impl MainWindow {
    /// 渲染时间轴波形轨 (F-014)。
    ///
    /// 包络在后台由 FFmpeg 解码成 8kHz 单声道后按时间桶取峰值（见
    /// `engines::waveform`），这里只做「重采样到固定柱数 + 上色」两件事。
    /// 固定柱数而非直接铺满桶数，是为了让每根柱在任意窗口宽度下都对应一段
    /// 稳定的时间切片，同时把元素数量压在两百个量级，播放时逐帧重绘不卡。
    fn render_waveform_lane(
        &mut self,
        progress_ratio: f32,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // 波形柱数量：约等于 1000px 时间轴上的 5px 一根，再密也看不出差别
        const BARS: usize = 200;
        // 满量程柱高（px）。轨道内高 26px，留出上下呼吸空间
        const MAX_BAR_H: f32 = 22.0;

        let peaks: Vec<f32> = match self.state.waveform.as_ref() {
            Some(w) if !w.peaks.is_empty() => resample_peaks(&w.peaks, BARS),
            _ => Vec::new(),
        };
        let extracting = self.state.waveform_busy_for.is_some();
        let has_peaks = !peaks.is_empty();

        div()
            .id("timeline-waveform-lane-row")
            .h(px(Theme::TRACK_ROW_H))
            .w_full()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(Theme::border_subtle())
            // 轨道名，与「字幕轨」标签同宽同缩进，保证两条轨的时间刻度对齐
            .child(
                div()
                    .w(px(Theme::TRACK_LABEL_W))
                    .pl(px(Theme::SPACE_3))
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(Theme::accent_blue())
                    .child("波形轨"),
            )
            .child(
                div()
                    .id("timeline-waveform-lane")
                    .flex_1()
                    .h(px(Theme::CTRL_H_XS))
                    .mr(px(Theme::TRACK_RIGHT_PAD))
                    .rounded(px(Theme::RADIUS_LG))
                    .bg(Theme::bg_panel())
                    .border_1()
                    .border_color(Theme::border())
                    .relative()
                    .overflow_hidden()
                    .cursor_pointer()
                    // 波形轨同样支持点击/拖拽定位，避免用户只能在字幕轨上找播放点
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                        }
                    }))
                    .child(if has_peaks {
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .px_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::WAVE_BAR_GAP))
                            .children(peaks.into_iter().enumerate().map(|(i, peak)| {
                                // 已播过的一半用薄荷色，未播的一半用中性灰，形成进度感
                                let played = (i as f32 + 0.5) / BARS as f32 <= progress_ratio;
                                div().flex_1().flex().justify_center().items_center().child(
                                    div()
                                        .w_full()
                                        // 静音段也留 1px 细线，保持「整轨连续」的观感
                                        .h(px((peak * MAX_BAR_H).max(Theme::WAVE_BAR_MIN_H)))
                                        .rounded_full()
                                        .bg(if played {
                                            Theme::accent_mint()
                                        } else {
                                            Theme::bg_dot_idle()
                                        }),
                                )
                            }))
                            .into_any_element()
                    } else {
                        // 提取中 / 提取失败 / 无音轨：给出可辨识的占位文案，而不是一条空白槽
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Theme::text_muted())
                            .child(if extracting {
                                "波形提取中…"
                            } else {
                                "无波形数据"
                            })
                            .into_any_element()
                    })
                    // 播放游标，与字幕轨的指针保持同一条垂直线
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(relative(progress_ratio))
                            .w(px(Theme::PLAYHEAD_W))
                            .bg(Theme::accent_mint()),
                    ),
            )
    }

    /// 渲染专业多轨剪辑时间轴 (Multi-track Timeline - 剪映/Premiere风格)
    pub(crate) fn render_multitrack_timeline(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let total_dur = self.state.total_duration.max(1.0);
        let cur_time = self.state.current_time;
        let progress_ratio = (cur_time / total_dur).clamp(0.0, 1.0) as f32;
        let sel_idx = self.state.selected_segment_index;

        div()
            .id("editor-multitrack-timeline")
            .h(px(Theme::TIMELINE_H))
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
                    .h(px(Theme::CTRL_H_SM))
                    .px(px(Theme::SPACE_3))
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
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text_primary())
                                    .child("时间轴"),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(Theme::bg_inset())
                                    .border_1()
                                    .border_color(Theme::border_mid())
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_family("Consolas")
                                    .text_color(Theme::accent_mint())
                                    .font_weight(FontWeight::BOLD)
                                    .child(seconds_to_hms(cur_time)),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .text_color(Theme::text_muted())
                                    .child(format!(
                                        "共 {} 句 · 总长 {}",
                                        self.state.segments.len(),
                                        format_duration_short(total_dur)
                                    )),
                            ),
                    )
                    // 右侧：快捷键速查（与 shortcuts::SHORTCUT_HINTS 同源，只挑高频几项，避免占满工具条）
                    .child(
                        div().flex().items_center().gap_3().children(
                            crate::ui::shortcuts::SHORTCUT_HINTS
                                .iter()
                                .filter(|(key, _)| {
                                    // 撤销是这次新加的高频操作，不进这个白名单的话
                                    // 提示只写在常量里、用户永远看不到
                                    matches!(
                                        *key,
                                        "Ctrl+Space" | "Alt+↑↓" | "Ctrl+F" | "Ctrl+E" | "Ctrl+Z"
                                    )
                                })
                                .map(|(key, label)| {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .child(
                                            div()
                                                .px_1p5()
                                                .py_0p5()
                                                .rounded(px(Theme::RADIUS_SM))
                                                .bg(Theme::bg_inset())
                                                .border_1()
                                                .border_color(Theme::border_mid())
                                                .text_size(px(Theme::TEXT_CAPTION))
                                                .font_family("Consolas")
                                                .text_color(Theme::text_secondary())
                                                .child(*key),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(Theme::TEXT_CAPTION))
                                                .text_color(Theme::text_muted())
                                                .child(*label),
                                        )
                                }),
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
                                    .w(px(Theme::TRACK_LABEL_W))
                                    .pl(px(Theme::SPACE_3))
                                    .text_size(px(Theme::TEXT_SMALL))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::accent_mint())
                                    .child("字幕轨"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(Theme::CTRL_H_MD))
                                    .mr(px(Theme::TRACK_RIGHT_PAD))
                                    .rounded_lg()
                                    .bg(Theme::bg_panel())
                                    .border_1()
                                    .border_color(Theme::border())
                                    .relative()
                                    .overflow_hidden()
                                    // 进度指示底色
                                    .child(
                                        div()
                                            .h_full()
                                            .w(relative(progress_ratio))
                                            .bg(Theme::tint_mint_soft()),
                                    )
                                    // 字幕片段渲染 (紧凑纤细卡片，高度 28px)
                                    .children({
                                        let relevant_segments: Vec<_> =
                                            if self.state.segments.len() <= 20 {
                                                self.state.segments.iter().collect()
                                            } else {
                                                self.state
                                                    .segments
                                                    .iter()
                                                    .filter(|seg| {
                                                        sel_idx == Some(seg.index)
                                                            || (cur_time >= seg.start
                                                                && cur_time <= seg.end)
                                                            || (cur_time >= seg.start - 15.0
                                                                && cur_time <= seg.end + 15.0)
                                                    })
                                                    .collect()
                                            };

                                        relevant_segments.into_iter().map(|seg| {
                                            let seg_idx = seg.index;
                                            let start_r =
                                                (seg.start / total_dur).clamp(0.0, 1.0) as f32;
                                            let width_r = (((seg.end - seg.start) / total_dur)
                                                .clamp(0.015, 1.0)
                                                as f32)
                                                .max(0.02);
                                            let is_selected = sel_idx == Some(seg_idx);
                                            let is_active =
                                                cur_time >= seg.start && cur_time <= seg.end;
                                            div()
                                                .id(("timeline-clip", seg_idx))
                                                .absolute()
                                                .top(px(Theme::TRACK_CLIP_INSET))
                                                .bottom(px(Theme::TRACK_CLIP_INSET))
                                                .left(relative(start_r))
                                                .w(relative(width_r))
                                                .min_w(px(Theme::SEG_CLIP_MIN_W))
                                                .rounded(px(Theme::RADIUS_SM))
                                                .bg(if is_selected {
                                                    Theme::accent_primary()
                                                } else if is_active {
                                                    Theme::accent_mint_deep()
                                                } else {
                                                    Theme::bg_dot_idle()
                                                })
                                                .border_1()
                                                .border_color(if is_selected {
                                                    Theme::text_on_saturated()
                                                } else if is_active {
                                                    Theme::accent_mint()
                                                } else {
                                                    Theme::tint_neutral_border()
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
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        // 未选中的片段底色是**翻转**的中性槽，文字须跟着翻；
                                                        // 选中/激活态是深色饱和块，改用恒白文字
                                                        .text_color(if is_selected || is_active {
                                                            Theme::text_on_saturated()
                                                        } else {
                                                            Theme::text_primary()
                                                        })
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
                            .left(px(Theme::TRACK_LABEL_W))
                            .right(px(Theme::TRACK_RIGHT_PAD))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                    let win_w = window.viewport_size().width;
                                    this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &MouseMoveEvent, window, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        let win_w = window.viewport_size().width;
                                        this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                                    }
                                },
                            ))
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                                    let win_w = window.viewport_size().width;
                                    this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                                }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(progress_ratio))
                                    .ml(px(-Theme::PLAYHEAD_GRAB_W / 2.0))
                                    .w(px(Theme::PLAYHEAD_GRAB_W))
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .text_color(Theme::accent_mint())
                                            .child("▼"),
                                    )
                                    .child(
                                        div()
                                            .w(px(Theme::PLAYHEAD_W))
                                            .flex_1()
                                            .bg(Theme::accent_mint()),
                                    ),
                            ),
                    ),
            )
            // 2. 音频波形轨 (F-014)：与字幕轨共用同一条时间刻度，方便对着语音峰值卡点
            .child(self.render_waveform_lane(progress_ratio, cx))
            // 3. 探底时间刻度标尺 (Time Ruler 移至底部展示，精简 20px)
            .child(
                div()
                    .id("timeline-time-ruler")
                    .h(px(Theme::BADGE_H))
                    .w_full()
                    .bg(Theme::bg_sidebar())
                    .border_t_1()
                    .border_color(Theme::border())
                    .relative()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, true, cx);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, window, cx| {
                            let win_w = window.viewport_size().width;
                            this.seek_by_mouse_x(event.position.x, win_w, false, cx);
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(Theme::TRACK_LABEL_W))
                            .right(px(Theme::TRACK_RIGHT_PAD))
                            .children((0usize..=10).map(|i| {
                                let ratio = i as f64 / 10.0;
                                let t = total_dur * ratio;
                                div()
                                    .id(("ruler-tick-btn", i))
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left(relative(ratio as f32))
                                    .ml(px(-Theme::RULER_TICK_HIT_W / 2.0))
                                    .w(px(Theme::RULER_TICK_HIT_W))
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
                                            .w(px(Theme::RULER_TICK_W))
                                            .h(px(Theme::RULER_TICK_H))
                                            .bg(Theme::text_muted()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
                                            .font_family("Consolas")
                                            .text_color(Theme::text_muted())
                                            .child(format_duration_short(t)),
                                    )
                            })),
                    ),
            )
    }

    /// 统一的时间轴鼠标点击与拖拽精确定位逻辑
    pub(crate) fn seek_by_mouse_x(
        &mut self,
        mouse_x: Pixels,
        window_width: Pixels,
        is_drag: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.total_duration <= 0.0 {
            return;
        }
        self.halt_preview_playback();
        // 两个偏移量必须与布局里的实际值同源，否则点击位置与播放头会整体错位。
        //
        // 注意：导航已从左侧 180px 竖栏改为顶部标签条，时间轴现在从窗口左缘
        // （x=0）起铺满整宽，不再有 `NAV_W` 的左偏移——多减它会让点击位置整体左移 180px。
        let left_pad = px(Theme::TRACK_LABEL_W);
        let right_pad = px(Theme::TRACK_RIGHT_PAD);

        let track_start_x = left_pad;
        let track_w = window_width - left_pad - right_pad;
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

    /// 把工作区选中项拨回「弹窗编辑打开时的那一句」，成功返回 `true`。
    ///
    /// 系统 InputBox **不是本窗口的模态框**：弹窗开着的时候用户仍能去视频库换工程、
    /// 删掉当前记录（`load_task_with` / `clear_current_workspace` 会整体换掉 `segments`），
    /// 连播时播放头推进也会自动改选中项。收尾时若闷头写「当前选中项」，用户敲进对话框的
    /// 文字就会落到别的句子上、甚至别的工程里，并随即落库——属于静默改错文档。
    ///
    /// 因此这里按「弹窗打开时的那份工程 + 那一句下标」定位：工程换了、或那句已不存在，
    /// 返回 `false` 由调用方丢弃结果；同一工程内则把选中项拨回原句，与用户当初点的是同一句。
    fn reanchor_prompt_target(
        &mut self,
        doc: &Option<std::path::PathBuf>,
        target: Option<usize>,
    ) -> bool {
        let Some(target) = target else {
            return false;
        };
        if self.state.selected_file.as_ref() != doc.as_ref() {
            return false;
        }
        if !self.state.segments.iter().any(|s| s.index == target) {
            return false;
        }
        // 走 `select_segment` 让选中项 / 编辑缓冲 / 播放头三者同步，避免后续写回时错位
        self.state.select_segment(target);
        true
    }

    /// 弹出原生 Windows 输入对话框进行字幕文本修改（完美支持搜狗/微软等中文输入法）
    pub(crate) fn prompt_edit_text(&mut self, cx: &mut Context<Self>) {
        // 记下这次弹窗属于哪份文档的哪一句，收尾时据它定位（见 `reanchor_prompt_target`）
        let doc = self.state.selected_file.clone();
        let target = self.state.selected_segment_index;
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
                    // 定位回弹窗打开时的那一句；该句已不属于当前工程则丢弃这次结果
                    if !this.reanchor_prompt_target(&doc, target) {
                        return;
                    }
                    this.state.editing_text = new_text;
                    this.state.save_selected_text();
                    cx.notify();
                });
            }
        }).detach();
    }

    /// 开始就地原地编辑字幕（原文或译文）。
    pub(crate) fn start_inline_edit(
        &mut self,
        seg_idx: usize,
        is_trans: bool,
        initial_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inline_edit_target.is_some() {
            self.commit_inline_edit(cx);
        }
        self.inline_edit_target = Some((seg_idx, is_trans));
        self.inline_edit_buffer = initial_text.to_string();
        self.inline_edit_cursor = initial_text.chars().count();
        window.focus(&self.inline_edit_focus);
        cx.notify();
    }

    /// 提交保存当前的就地原地编辑。
    pub(crate) fn commit_inline_edit(&mut self, cx: &mut Context<Self>) {
        let Some((seg_idx, is_trans)) = self.inline_edit_target.take() else {
            return;
        };
        let new_text = self.inline_edit_buffer.trim().to_string();
        if is_trans {
            if let Some(seg) = self.state.segments.iter_mut().find(|s| s.index == seg_idx) {
                if new_text.is_empty() {
                    seg.translation = None;
                    seg.translation_lang = None;
                } else {
                    seg.translation = Some(new_text);
                    seg.translation_lang = Some(self.state.translate_target_lang.clone());
                }
            }
        } else {
            if !new_text.is_empty() {
                if let Some(seg) = self.state.segments.iter_mut().find(|s| s.index == seg_idx) {
                    seg.text = new_text.clone();
                    if !seg.polished.is_empty() {
                        seg.polished = new_text.clone();
                    }
                }
                if self.state.selected_segment_index == Some(seg_idx) {
                    self.state.editing_text = new_text;
                }
            }
        }
        self.inline_edit_buffer.clear();
        self.inline_edit_cursor = 0;
        self.state.segments_dirty = true;
        self.state.flush_segments_if_dirty();
        cx.notify();
    }

    /// 取消当前的就地原地编辑。
    pub(crate) fn cancel_inline_edit(&mut self, cx: &mut Context<Self>) {
        self.inline_edit_target = None;
        self.inline_edit_buffer.clear();
        self.inline_edit_cursor = 0;
        cx.notify();
    }

    /// 处理就地原地编辑时的按键输入
    pub(crate) fn handle_inline_edit_keydown(
        &mut self,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke.key;
        let total_chars = self.inline_edit_buffer.chars().count();
        let cursor = self.inline_edit_cursor.min(total_chars);

        if event.keystroke.modifiers.control {
            if key == "v" {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        let clean = text.trim().replace(['\r', '\n'], "");
                        let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                        let insert_chars: Vec<char> = clean.chars().collect();
                        let ins_len = insert_chars.len();
                        chars.splice(cursor..cursor, insert_chars);
                        self.inline_edit_buffer = chars.into_iter().collect();
                        self.inline_edit_cursor = cursor + ins_len;
                        cx.notify();
                    }
                }
            } else if key == "c" {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                    self.inline_edit_buffer.clone(),
                ));
            } else if key == "a" {
                self.inline_edit_cursor = total_chars;
                cx.notify();
            }
            return;
        }

        if key == "enter" {
            self.commit_inline_edit(cx);
        } else if key == "escape" {
            self.cancel_inline_edit(cx);
        } else if key == "backspace" {
            if cursor > 0 && total_chars > 0 {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.remove(cursor - 1);
                self.inline_edit_buffer = chars.into_iter().collect();
                self.inline_edit_cursor = cursor - 1;
                cx.notify();
            }
        } else if key == "delete" {
            if cursor < total_chars {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.remove(cursor);
                self.inline_edit_buffer = chars.into_iter().collect();
                cx.notify();
            }
        } else if key == "left" {
            if cursor > 0 {
                self.inline_edit_cursor = cursor - 1;
                cx.notify();
            }
        } else if key == "right" {
            if cursor < total_chars {
                self.inline_edit_cursor = cursor + 1;
                cx.notify();
            }
        } else if key == "home" {
            self.inline_edit_cursor = 0;
            cx.notify();
        } else if key == "end" {
            self.inline_edit_cursor = total_chars;
            cx.notify();
        } else if key.chars().count() == 1 {
            let ch = key.chars().next().unwrap();
            if !ch.is_control() {
                let mut chars: Vec<char> = self.inline_edit_buffer.chars().collect();
                chars.insert(cursor, ch);
                self.inline_edit_buffer = chars.into_iter().collect();
                self.inline_edit_cursor = cursor + 1;
                cx.notify();
            }
        }
    }

    /// 弹出原生 Windows 输入对话框直接订正**译文**（旧版备用入口，已被就地编辑替代）。
    #[allow(dead_code)]
    pub(crate) fn prompt_edit_translation(&mut self, cx: &mut Context<Self>) {
        // 与 `prompt_edit_text` 同样的会话守卫：InputBox 弹出期间工作区可能被换掉、
        // 选中项也可能被播放联动改掉，收尾时必须定位回原来那一句
        // （见 `reanchor_prompt_target`）。
        let doc = self.state.selected_file.clone();
        let target = self.state.selected_segment_index;
        // 预填「当前选中片段的译文」；没有译文则从原文起步，方便用户直接在机翻基础上改。
        let current = self
            .state
            .segments
            .iter()
            .find(|s| Some(s.index) == target)
            .and_then(|s| s.translation.clone())
            .unwrap_or_default();
        let prompt_title = "Voice2Word - 修改译文";
        // 系统 InputBox 在「取消」与「确定但内容为空」两种情况下都返回空串，无法区分。
        // 因此这里把空串当作**空操作**（与 `prompt_edit_text` 一致），避免用户点「取消」
        // 却把已译好的句子清空。要删除译文请用旁边的「清除译文」按钮。
        let prompt_msg = "请输入修改后的译文：（点「取消」不会改动原文；清空请用「清除译文」）";

        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                use std::process::Command;
                let safe_msg = prompt_msg.replace('\'', "''");
                let safe_title = prompt_title.replace('\'', "''");
                let safe_default = current.replace('\'', "''");
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
                        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                    }
                    _ => None,
                }
            }).await;

            if let Some(new_text) = res {
                let _ = this.update(cx, |this, cx| {
                    // 定位回弹窗打开时的那一句；该句已不属于当前工程则丢弃这次结果
                    if !this.reanchor_prompt_target(&doc, target) {
                        return;
                    }
                    if !new_text.trim().is_empty() {
                        this.state.set_selected_translation(&new_text);
                        cx.notify();
                    }
                });
            }
        }).detach();
    }

    /// 渲染色彩圆盘调色板（24色相环绕成圈，鼠标滑动调色）
    pub(crate) fn render_color_wheel_picker(
        &self,
        current_hex: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (r, g, b) = crate::utils::SubtitleStyleConfig::hex_to_rgb(current_hex);
        let cur_rgb = rgb((r as u32) << 16 | (g as u32) << 8 | (b as u32));
        let hex_display = current_hex.to_uppercase();

        div()
            .id("custom-color-wheel-panel")
            .w_full()
            .p_2()
            .rounded_md()
            .bg(Theme::bg_inset())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            // 左侧：24色相圆环色轮，鼠标滑过即可顺滑调色
            .child(
                div()
                    .w(px(76.0))
                    .h(px(76.0))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    // 24 个色块环绕成一个圈
                    .children((0..24usize).map(|i| {
                        let angle_deg = i as f32 * (360.0 / 24.0);
                        let (pr, pg, pb) = hsv_to_rgb(angle_deg, 1.0, self.color_picker_brightness);
                        let hex_str = format!("#{:02X}{:02X}{:02X}", pr, pg, pb);
                        let dot_color = rgb((pr as u32) << 16 | (pg as u32) << 8 | (pb as u32));
                        let rad = angle_deg.to_radians();
                        let cx_pos = 38.0 + 26.0 * rad.cos() - 4.5;
                        let cy_pos = 38.0 + 26.0 * rad.sin() - 4.5;

                        div()
                            .id(("wheel-dot", i))
                            .absolute()
                            .left(px(cx_pos))
                            .top(px(cy_pos))
                            .w(px(9.0))
                            .h(px(9.0))
                            .rounded_full()
                            .cursor_pointer()
                            .bg(dot_color)
                            .hover(|s| s.border_1().border_color(Theme::text_primary()))
                            .on_mouse_move(cx.listener({
                                let h = hex_str.clone();
                                move |this, event: &MouseMoveEvent, _, cx| {
                                    if event.pressed_button == Some(MouseButton::Left) {
                                        this.state.config.subtitle_style.primary_color = h.clone();
                                        this.state.save_subtitle_style();
                                        cx.notify();
                                    }
                                }
                            }))
                            .on_click(cx.listener({
                                let h = hex_str.clone();
                                move |this, _, _, cx| {
                                    this.state.config.subtitle_style.primary_color = h.clone();
                                    this.state.save_subtitle_style();
                                    cx.notify();
                                }
                            }))
                    }))
                    // 圆环正中心：当前拾取的大色块
                    .child(
                        div()
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded_full()
                            .bg(cur_rgb)
                            .border_2()
                            .border_color(Theme::border())
                    )
            )
            // 右侧：当前十六进制数值与快速纯度调节
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .font_family("Consolas")
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_mint())
                                    .child(hex_display)
                            )
                            .child(
                                div()
                                    .id("wheel-close-btn")
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(Theme::bg_hover())
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.text_color(Theme::text_primary()))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.custom_color_picker_open = false;
                                        cx.notify();
                                    }))
                                    .child("收起 ✕")
                            )
                    )
                    // 明暗度 / 纯度微调
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children([
                                ("纯黑", "#000000"),
                                ("深暗", "#1E293B"),
                                ("明亮", "#F8FAFC"),
                                ("金色", "#F59E0B"),
                                ("青翠", "#10B981"),
                            ].into_iter().enumerate().map(|(idx, (name, hex))| {
                                div()
                                    .id(("preset-light", idx))
                                    .flex_1()
                                    .py_0p5()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(Theme::bg_hover())
                                    .text_color(Theme::text_secondary())
                                    .hover(|s| s.text_color(Theme::text_primary()).bg(Theme::bg_hover_strong()))
                                    .on_click(cx.listener({
                                        let h = hex.to_string();
                                        move |this, _, _, cx| {
                                            this.state.config.subtitle_style.primary_color = h.clone();
                                            this.state.save_subtitle_style();
                                            cx.notify();
                                        }
                                    }))
                                    .child(name)
                            }))
                    )
            )
            .into_any_element()
    }

    /// 渲染 Word 风格的数值步进微调输入组件（带加减按钮，中间数值框支持单击/双击就地编辑数字）
    pub(crate) fn render_style_stepper_input<FDown, FUp>(
        &self,
        field: StyleField,
        step_down_label: &str,
        step_up_label: &str,
        step_down_fn: FDown,
        step_up_fn: FUp,
        display_val: String,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        FDown: Fn(&mut MainWindow, &mut Context<MainWindow>) + 'static + Clone,
        FUp: Fn(&mut MainWindow, &mut Context<MainWindow>) + 'static + Clone,
    {
        let is_editing = self.focused_style_field == Some(field);
        let id_prefix = match field {
            StyleField::FontSize => "font-size",
            StyleField::LetterSpacing => "letter-sp",
            StyleField::BottomMargin => "bot-mg",
            StyleField::LineSpacing => "line-sp",
            StyleField::MaxChars => "max-chars",
        };

        let dec_id = format!("{}-dec", id_prefix);
        let inc_id = format!("{}-inc", id_prefix);
        let val_id = format!("{}-val-box", id_prefix);

        let down_fn = step_down_fn.clone();
        let up_fn = step_up_fn.clone();

        let focus_handle = self.style_field_focus.clone();

        div()
            .flex()
            .items_center()
            .gap_1()
            .bg(Theme::bg_inset())
            .border_1()
            .border_color(if is_editing { Theme::accent_mint() } else { Theme::bg_hover_strong() })
            .rounded_md()
            .p_0p5()
            // 减号步进按钮
            .child(
                div()
                    .id(SharedString::from(dec_id))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .rounded_sm()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::accent_mint()))
                    .child(step_down_label.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.focused_style_field.is_some() {
                            this.commit_style_field_edit(cx);
                        }
                        down_fn(this, cx);
                    }))
            )
            // 中间输入框 / 显示框（支持单击与双击直接输入数字）
            .child(if is_editing {
                let cursor = self.line_edit_cursor.min(self.style_field_input.chars().count());
                let before: String = self.style_field_input.chars().take(cursor).collect();
                let after: String = self.style_field_input.chars().skip(cursor).collect();

                div()
                    .id(SharedString::from(val_id))
                    .track_focus(&focus_handle)
                    .w(px(76.0))
                    .px_1p5()
                    .py_0p5()
                    .bg(Theme::bg_app())
                    .border_1()
                    .border_color(Theme::accent_mint())
                    .rounded_sm()
                    .cursor_text()
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        let key = &event.keystroke.key;
                        if key == "enter" {
                            this.commit_style_field_edit(cx);
                        } else if key == "escape" {
                            this.cancel_style_field_edit(cx);
                        } else if apply_line_edit(&mut this.style_field_input, &mut this.line_edit_cursor, event, cx) {
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .text_size(px(Theme::TEXT_SMALL))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(before)
                            .child(div().text_color(Theme::accent_mint()).child("▌"))
                            .child(after)
                    )
            } else {
                div()
                    .id(SharedString::from(val_id))
                    .w(px(76.0))
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_text()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::accent_mint())
                    .hover(|s| s.bg(Theme::bg_hover()).border_1().border_color(Theme::accent_mint()))
                    .child(display_val)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_style_field_edit(field, window, cx);
                    }))
            })
            // 加号步进按钮
            .child(
                div()
                    .id(SharedString::from(inc_id))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .rounded_sm()
                    .text_size(px(Theme::TEXT_SMALL))
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::accent_mint()))
                    .child(step_up_label.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.focused_style_field.is_some() {
                            this.commit_style_field_edit(cx);
                        }
                        up_fn(this, cx);
                    }))
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::translated_out_count;
    use crate::subtitle::Segment;
    use crate::ui::primitives::should_consume_scroll;

    /// 回归（P2）：「已全部翻译」的判定必须用**实际译出**口径。
    /// 引擎可能给空串/纯空白译文打上 `translation_lang`（或直接复制原文），
    /// 只按语言标签计数会在整片空白时仍显示「已全部翻译」。
    #[test]
    fn only_non_empty_translations_count_as_done() {
        let mut blank = Segment::new(1, 0.0, 1.0, "你好");
        blank.translation = Some("   ".to_string());
        blank.translation_lang = Some("English".to_string()); // 脏标签：有标签无译文
        let mut ok = Segment::new(2, 1.0, 2.0, "世界");
        ok.translation = Some("World".to_string());
        ok.translation_lang = Some("English".to_string());
        let mut other = Segment::new(3, 2.0, 3.0, "再见");
        other.translation = Some("Au revoir".to_string());
        other.translation_lang = Some("Français".to_string());

        let segs = vec![blank, ok, other];
        // 只有 ok 是「当前目标语言 + 非空译文」；blank 的脏标签绝不能算完成。
        assert_eq!(translated_out_count(&segs, "English"), 1);
        assert_eq!(translated_out_count(&segs, "Français"), 1);
        assert_eq!(translated_out_count(&segs, "日本語"), 0);
    }

    /// 内嵌清单滚轮判据：有余量就吃掉（阻止冒泡），到边界才放行给外层。
    ///
    /// 回归现象：在字幕清单里滚轮，整个右侧面板也跟着滚。根因是 GPUI 的滚动
    /// 监听不 `stop_propagation`，内嵌清单与外层可滚动面板同时吃同一个事件。
    #[test]
    fn nested_list_consumes_scroll_until_it_hits_the_edge() {
        // 可滚动区间 100px：offset 从 0（顶部）到 -100（底部）
        let max_h = 100.0;

        // 在中段：两个方向都有余量，清单自己吃掉
        assert!(should_consume_scroll(-3.0, -50.0, max_h), "中段向下应吃掉");
        assert!(should_consume_scroll(3.0, -50.0, max_h), "中段向上应吃掉");

        // 顶部再往上滚：清单已没余量，放行给外层
        assert!(
            !should_consume_scroll(3.0, 0.0, max_h),
            "顶部继续上滚应放行"
        );
        // 顶部向下滚：有余量，吃掉
        assert!(should_consume_scroll(-3.0, 0.0, max_h), "顶部向下应吃掉");

        // 底部再往下滚：放行
        assert!(
            !should_consume_scroll(-3.0, -max_h, max_h),
            "底部继续下滚应放行"
        );
        // 底部向上滚：有余量，吃掉
        assert!(should_consume_scroll(3.0, -max_h, max_h), "底部向上应吃掉");

        // 内容没超框（max_h≈0）：一律放行，交给外层
        assert!(!should_consume_scroll(-3.0, 0.0, 0.0), "不可滚时应放行");
        assert!(!should_consume_scroll(3.0, 0.0, 0.0), "不可滚时应放行");

        // 横向滚轮（dy == 0）不拦
        assert!(!should_consume_scroll(0.0, -50.0, max_h), "横向滚轮应放行");
    }
}
