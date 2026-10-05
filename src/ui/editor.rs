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
use crate::utils::time::{format_duration_short, seconds_to_hms, seconds_to_timestamp_short};
use super::primitives;
use super::theme::Theme;
use super::{apply_line_edit, EditorExportFormat, EditorSubtitlePanel, MainWindow};

/// 预设 → 预览配色 `(文字色, 底色, 描边色)`。
///
/// GPUI 不支持文字描边与投影，所以这里用「底色透明度 + 描边」近似表达各预设的观感；
/// 导出 ASS 时同一预设会写入真正的描边 / 阴影 / 底框参数（见 `subtitle::writer::ass_preset_colors`）。
/// 注意：本文件同时引入了 `image::Rgba`，故返回类型须写全限定名 `gpui::Rgba`。
fn subtitle_preset_colors(preset: &str) -> (gpui::Rgba, gpui::Rgba, gpui::Rgba) {
    match preset {
        // 黄字 + 黑边：用较重的底色近似黑边带来的高对比
        "黄字黑边" => (rgb(0xffd60a), rgba(0x000000d9), rgba(0x000000ff)),
        // 半透明黑框：白字 + 明显的半透明底框
        "半透明黑框" => (
            // 字幕字形画在视频画面 / 媒体底色上，恒用恒白 token，
            // 不能跟主题翻转（浅色主题下 text_white 会变近黑，直接糊成一片）
            Theme::text_subtitle(),
            rgba(0x000000b3),
            Theme::tint_neutral_border(),
        ),
        // 电影沉浸：无底框悬浮白字
        "电影沉浸" => (
            Theme::text_subtitle(),
            Theme::transparent(),
            Theme::transparent(),
        ),
        // 白字黑影（默认）：白字 + 浅底色近似投影
        _ => (Theme::text_subtitle(), rgba(0x00000080), Theme::transparent()),
    }
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

/// 字幕样式可选值（字号 / 底边距以 1080p 为基准，与 `SubtitleStyleConfig` 语义一致）
const STYLE_FONT_SIZES: [u32; 4] = [32, 40, 48, 56];
const STYLE_LETTER_SPACINGS: [u32; 4] = [0, 1, 2, 4];
const STYLE_LINE_SPACINGS: [f32; 4] = [1.0, 1.2, 1.4, 1.6];
const STYLE_MAX_CHARS: [u32; 4] = [12, 16, 20, 24];
const STYLE_BOTTOM_MARGINS: [u32; 4] = [20, 40, 60, 80];
/// 分段按钮组的宽度上界：4 个短标签等分后每个约 117px，够放下 2~4 个字，
/// 又不会像铺满整行那样被拉伸成 200px 的空条。用 max_w 而非固定宽，
/// 是为了窄面板下仍能随容器收缩，避免「标签 + 按钮组」溢出卡片。
const STYLE_ROW_BTN_MAX_W: f32 = 480.0;

/// 左右二分布局下两列各自的最小宽度（逻辑 px）。
///
/// 左侧 300 + 右侧 440 = 740：这是「左右并排仍不互相裁切」的下界。左侧导航栏另有
/// 固定 180，故窗口窄于 `EDITOR_STACK_BELOW_W` 时改为上下堆叠。
/// 右侧取 440 而非原来的 460，是为了配合下表列的收窄（见 `SUBTITLE_TABLE_*`）。
const EDITOR_LEFT_MIN_W: f32 = 300.0;
const EDITOR_RIGHT_MIN_W: f32 = 440.0;

/// 窗口逻辑宽度低于此值时，「视频监视器 | 字幕配置」由左右二分改为上下堆叠。
///
/// 二分布局的最小可用宽度 = 导航栏 180 + 左 300 + 右 440 = 920，这里再留 20px 余量
/// 取 940。窗口比这更窄时两列的 min_w 之和已超过可用宽度，flex 会保持各自 min_w 并把
/// 右列整体顶到窗口右边缘之外，被父级 `.overflow_hidden()` 裁掉——「合并下句」
/// 「收起字幕样式」这类最右元素就是这样消失的。堆叠后两列各占整行宽度，列内再靠折行收口。
const EDITOR_STACK_BELOW_W: f32 = 940.0;

/// 上下堆叠布局下视频监视器固定占用的高度（监视器自身最少需要 36+180+52=268）。
const EDITOR_STACK_MONITOR_H: f32 = 320.0;

/// 上下堆叠布局下「多语言对照表」的固定高度。
///
/// 此时面板外层已可纵向滚动，表格若继续用 `flex_1` 会在自动高度的滚动容器里塌成 0。

/// 「多语言对照表」的列宽。表头与数据行共用同一组常量，避免两处各写一份后悄悄漂移。
///
/// 表内最小横向占用 = 左右内边距 6×2 + 序号 24 + 起止时间 68×2 + 说话人 64
/// + 原文/译文两列各 46 的下限 + 5 处 5px 间距 = 355px（两列文本再窄就只剩省略号）。
/// 面板宽度减去正文与卡片的两层 12px 内边距后若不足 355，表内单元格就会互相挤压；
/// 因此 `EDITOR_RIGHT_MIN_W`(440) 与导航栏 180 共同保证窗口 600px 起表格都有 360px 以上。
///
/// 这几列是本表唯一的「固定开销」，它们每省 1px，原文与译文两列就能各多分到 0.5px
/// （剩余宽度由两列 `flex_1` 均分），所以压缩要压在这里：
/// - 时间列 80→68：显示格式同步压到 `hh:mm:ss.t`（10 字符）。毫秒那一位扫读无用，
///   却是把时间列撑到 80 的唯一原因，白白吃掉两列文本的空间——窄面板下译文最先被截。
/// - 序号 28→24、说话人保持 64（「说话人 N」+ 内边距下限）、内边距 8→6、列间距 6→5。
const SUBTITLE_TABLE_COL_INDEX_W: f32 = 24.0;
const SUBTITLE_TABLE_COL_TIME_W: f32 = 68.0;
const SUBTITLE_TABLE_COL_SPEAKER_W: f32 = 64.0;
const SUBTITLE_TABLE_COL_TEXT_MIN_W: f32 = 46.0;
const SUBTITLE_TABLE_PAD_X: f32 = 6.0;
const SUBTITLE_TABLE_GAP: f32 = 5.0;

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
            .child(
                if is_processing {
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
                }
            )
            .child(
                if stacked {
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
                    div()
                        .id("editor-main-split")
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
                                .min_w(px(EDITOR_LEFT_MIN_W))
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
                                .min_w(px(EDITOR_RIGHT_MIN_W))
                                .overflow_hidden()
                                .child(self.render_subtitle_inspector(false, cx)),
                        )
                },
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
                            .child(
                                if is_playing {
                                    // 瞬时状态胶囊走 primitives::tag_tinted
                                    primitives::tag_tinted(
                                        "播放中",
                                        Theme::tint_mint_soft(),
                                        Theme::tint_mint_border(),
                                        Theme::accent_mint(),
                                    )
                                } else {
                                    div()
                                }
                            ),
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
                                let (fg, bg, border) = subtitle_preset_colors(&style.preset_name);
                                // 宽度与编辑卡里的预览框同源：拖把手时两者在同一帧一起变，
                                // 不存在「预览改了、画面上没跟上」的延迟
                                let box_w = self.subtitle_box_w();
                                div()
                                    .absolute()
                                    .bottom(relative(style.preview_bottom_ratio()))
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .justify_center()
                                    .px_6()
                                    .child(
                                        if let Some(seg) = active_seg {
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
                                                .font_weight(FontWeight::BOLD)
                                                .text_color(fg)
                                                .text_align(TextAlign::Center)
                                                .line_height(px(font_px * style.line_spacing))
                                                .child(seg.display_text().to_string())
                                        } else {
                                            div()
                                        }
                                    )
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
                                    .bg(Theme::bg_input())
                                    .p(px(Theme::CONTROL_INSET))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(Theme::border_mid())
                                    .flex()
                                    .items_center()
                                    .gap(px(Theme::CTRL_GAP_TIGHT))
                                    // 上一句
                                    .child(
                                        primitives::pill_btn("上句")
                                            .id("ctrl-prev-seg")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.jump_prev_segment(cx);
                                            })),
                                    )
                                    // 快退 1 秒
                                    .child(
                                        primitives::pill_btn("-1s")
                                            .id("ctrl-step-back")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.halt_preview_playback();
                                                let target = (this.state.current_time - 1.0).max(0.0);
                                                this.state.seek_to(target);
                                                this.trigger_extract_frame(cx);
                                                cx.notify();
                                            })),
                                    )
                                    // 实时播放 / 暂停 (高亮突出放大按钮)
                                    .child(
                                        primitives::pill_btn_solid(
                                            if is_playing { "暂停" } else { "播放" },
                                            if is_playing { Theme::accent_orange() } else { Theme::accent_mint() },
                                        )
                                            .id("ctrl-play-pause")
                                            .px(px(Theme::SPACE_6))
                                            .text_size(px(Theme::TEXT_BODY_LG))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.toggle_play_preview(cx);
                                            })),
                                    )
                                    // 快进 1 秒
                                    .child(
                                        primitives::pill_btn("+1s")
                                            .id("ctrl-step-fwd")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.halt_preview_playback();
                                                let target = this.state.current_time + 1.0;
                                                this.state.seek_to(target);
                                                this.trigger_extract_frame(cx);
                                                cx.notify();
                                            })),
                                    )
                                    // 下一句
                                    .child(
                                        primitives::pill_btn("下句")
                                            .id("ctrl-next-seg")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.jump_next_segment(cx);
                                            })),
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
            // `preview_frame_path` 只在**确认文件存在**时被写入（见 actions.rs 的帧抽取
            // 回写点），因此这里不再逐帧 `exists()`。空 `Some` 不会出现。
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(img(frame_path.clone()).size_full())
        } else if self.state.selected_file.is_some() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap_1p5()
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(Theme::accent_on_media())
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
        let cur_seg = sel_idx.and_then(|idx| self.state.segments.iter().find(|s| s.index == idx)).cloned();
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
                        primitives::panel_title("字幕配置与多语言列表"),
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
                                // 卡片标题
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
                                                .text_size(px(Theme::TEXT_SMALL))
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
                                                .text_size(px(Theme::TEXT_SMALL))
                                                .text_color(Theme::text_secondary())
                                                .child("视觉预设:"),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_1()
                                                .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                .gap_1p5()
                                                // 预设清单与 `SubtitleStyleConfig::apply_preset` 共用同一份常量，
                                                // 避免两处各写一遍名字后悄悄漂移
                                                .children(crate::utils::SUBTITLE_PRESETS.into_iter().enumerate().map(|(idx, label)| {
                                                    let is_sel = cur_style.preset_name == label;
                                                    div()
                                                        .id(("preset-btn", idx))
                                                        .flex_1()
                                                        .py_1()
                                                        .rounded_md()
                                                        .cursor_pointer()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        // 用布局居中（flex + justify_center），不用 `text_center()`：
                                                        // GPUI 0.2.2 的 `.hover()` 走 `Style::refine`，而 `Style::text`
                                                        // 没有 `#[refineable]`，悬停时整块 `TextStyleRefinement` 会被
                                                        // hover 闭包里那份（只设了 text_color）**整体替换**，没显式设过的
                                                        // `text_align` 于是回落到默认值 Left —— 鼠标一移上去文字就跳到左边。
                                                        // 居中交给 flex 后，悬停只碰底色/前景色，不再牵动对齐。
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                        .border_1()
                                                        .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                        .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                        .hover(|s| if !is_sel { s.bg(Theme::bg_hover()).text_color(Theme::text_primary()) } else { s })
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            // 预设同时套用整组排版参数，而非只改名字
                                                            this.state.apply_subtitle_preset(label);
                                                            cx.notify();
                                                        }))
                                                        .child(label)
                                                }))
                                        )
                                )
                                // 3) 排版参数（字号 / 字间距 / 行间距 / 单行字数 / 底边距）
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_1p5()
                                        // 字号
                                        .child(
                                            div()
                                                .flex_1()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(Theme::FORM_LABEL_W)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字号"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                        .flex()
                                                        .gap_1()
                                                        .children(STYLE_FONT_SIZES.into_iter().map(|val| {
                                                            let is_sel = cur_style.font_size == val;
                                                            div()
                                                                .id(("font-sz", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.font_size = val;
                                                                    this.state.save_subtitle_style();
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
                                                .child(div().w(px(Theme::FORM_LABEL_W)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("字间距"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                        .flex()
                                                        .gap_1()
                                                        .children(STYLE_LETTER_SPACINGS.into_iter().map(|val| {
                                                            let is_sel = cur_style.letter_spacing == val;
                                                            div()
                                                                .id(("letter-sp", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.letter_spacing = val;
                                                                    this.state.save_subtitle_style();
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
                                                .child(div().w(px(Theme::FORM_LABEL_W)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("底边距"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                        .flex()
                                                        .gap_1()
                                                        .children(STYLE_BOTTOM_MARGINS.into_iter().map(|val| {
                                                            let is_sel = cur_style.bottom_margin == val;
                                                            div()
                                                                .id(("bot-mg", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.bottom_margin = val;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                                .child(format!("{}", val))
                                                        }))
                                                )
                                        )
                                        // 行间距
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(Theme::FORM_LABEL_W)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("行间距"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                        .flex()
                                                        .gap_1()
                                                        .children(STYLE_LINE_SPACINGS.into_iter().map(|val| {
                                                            let is_sel = cur_style.line_spacing == val;
                                                            div()
                                                                .id(("line-sp", (val * 10.0) as u32))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.line_spacing = val;
                                                                    this.state.save_subtitle_style();
                                                                    cx.notify();
                                                                }))
                                                                .child(format!("{:.1}", val))
                                                        }))
                                                )
                                        )
                                        // 单行字数
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .child(div().w(px(Theme::FORM_LABEL_W)).flex_shrink_0().text_size(px(Theme::TEXT_SMALL)).text_color(Theme::text_secondary()).child("单行字数"))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .max_w(px(STYLE_ROW_BTN_MAX_W))
                                                        .flex()
                                                        .gap_1()
                                                        .children(STYLE_MAX_CHARS.into_iter().map(|val| {
                                                            let is_sel = cur_style.max_chars_per_line == val;
                                                            div()
                                                                .id(("max-chars", val))
                                                                .flex_1()
                                                                .py_0p5()
                                                                .rounded_md()
                                                                .cursor_pointer()
                                                                .text_size(px(Theme::TEXT_SMALL))
                                                                .text_center()
                                                                .bg(if is_sel { Theme::accent_mint() } else { Theme::bg_inset() })
                                                                .border_1()
                                                                .border_color(if is_sel { Theme::accent_mint() } else { Theme::bg_hover_strong() })
                                                                .text_color(if is_sel { Theme::text_on_accent() } else { Theme::text_secondary() })
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    this.state.config.subtitle_style.max_chars_per_line = val;
                                                                    this.state.save_subtitle_style();
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
                    // 2. 单句快速编辑卡（仅「字幕样式」面板）。
                    //
                    // 这张卡自带「实时预览条」，预览的就是卡里正在编辑的这一句——编辑与
                    // 预览在同一张卡上，改字 / 调字号都是即时的。翻译面板不重复放编辑卡：
                    // 面板标头已能切回样式面板，两处各放一份只会让人分不清哪份生效。
                    .child(
                        if panel == EditorSubtitlePanel::Style && cur_seg.is_some() {
                            let seg = cur_seg.as_ref().unwrap();
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
                                // 第一行：文段编号、时间范围与右侧主操作按钮组 (保存修改、弹窗编辑、删除)
                                //
                                // 这一行是最容易被右边缘裁掉的地方：时间戳约 220px 加上右侧
                                // 五个按钮约 310px，累计超过 530px；窄面板下不加折行，最后的
                                // 「合并下句」「删除」就会被裁到面板外。flex_wrap 让按钮组整体
                                // 掉到第二行，而不是被裁掉。
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
                                                .gap_2()
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
                                                        .text_size(px(Theme::TEXT_BODY))
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
                                                .child(Self::render_mini_btn(
                                                    "btn-split-seg",
                                                    "拆分",
                                                    can_split,
                                                    cx,
                                                    |this, cx| {
                                                        // 光标停在句子中间时按光标断句，否则取正中
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
                                                ))
                                                .child(
                                                    // 危险操作按钮走 btn_danger：红底红边红字，行内尺寸
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
                                        })
                                )
                                // 第二行半：实时预览条——显示的就是上一行正在编辑的那句文本。
                                // 排版参数（字号 / 行间距 / 底边距 / 单行字数）改一下立刻在这里
                                // 看到效果，不必载入视频、也不必去别的面板找。
                                //
                                // 两侧各挂一个「拖拽调宽」把手（`subtitle_preview_box`）：
                                // 字幕框以中线为中心左右对称收放，把手贴在框的两条边上。
                                .child(self.render_subtitle_preview_box(&cur_text, cx))
                                // 第三行：时间微调按钮组与快捷标点注入 (紧凑规整，不遮挡)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .flex_wrap()
                                        .gap_2()
                                        // 左侧：时间微调（起止各 ±0.1s / ±0.5s，粗调与细调并排）
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_muted())
                                                        .child("微调:"),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("起"),
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
                                                ))
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
                                                        .text_color(Theme::text_secondary())
                                                        .child("止"),
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
                                        )
                                        // 右侧：快捷标点注入
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(Theme::TEXT_SMALL))
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
                    // 2.5 字幕多语言翻译面板（仅「字幕翻译」面板可见；后端链路早已就绪）
                    .child(
                        if panel == EditorSubtitlePanel::Translate {
                            self.render_translate_card(cx).into_any_element()
                        } else {
                            div().into_any_element()
                        },
                    )
                    // 3. 多语言字幕配置与对照大表格 (图二风格)
                    .child(
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
                            })
                            // 表头 (图二标准规格)
                            .child(
                                div()
                                    .w_full()
                                    .h(px(Theme::TRACK_ROW_H))
                                    .bg(Theme::bg_input())
                                    .border_b_1()
                                    .border_color(Theme::border())
                                    .flex()
                                    .items_center()
                                    .px(px(SUBTITLE_TABLE_PAD_X))
                                    .gap(px(SUBTITLE_TABLE_GAP))
                                    // 序号
                                    .child(
                                        div()
                                            .w(px(SUBTITLE_TABLE_COL_INDEX_W))
                                            .text_center()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("#"),
                                    )
                                    // 开始时间
                                    .child(
                                        div()
                                            .w(px(SUBTITLE_TABLE_COL_TIME_W))
                                            .text_center()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("开始时间"),
                                    )
                                    // 结束时间
                                    .child(
                                        div()
                                            .w(px(SUBTITLE_TABLE_COL_TIME_W))
                                            .text_center()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("结束时间"),
                                    )
                                    // 说话人（未做分离时整列为 —）
                                    .child(
                                        div()
                                            .w(px(SUBTITLE_TABLE_COL_SPEAKER_W))
                                            .text_center()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("说话人"),
                                    )
                                    // 字幕内容
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(SUBTITLE_TABLE_COL_TEXT_MIN_W))
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_muted())
                                            .child("字幕内容"),
                                    )
                                    // 翻译字幕
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(SUBTITLE_TABLE_COL_TEXT_MIN_W))
                                            .text_size(px(Theme::TEXT_BODY))
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
                                uniform_list(
                                    "inspector-segments-virtual",
                                    row_count,
                                    cx.processor(move |this, visible_range: std::ops::Range<usize>, _window, cx| {
                                        let sel = this.state.selected_segment_index;
                                        let cur_time = this.state.current_time;
                                        visible_range
                                            .map(|i| {
                                                // 行数必须与请求区间严格一致：uniform_list 的
                                                // prepaint 会把返回的行与可见区间逐一对齐，少一行
                                                // 后面的行就整体错位；而它只拿第 0 行量行高，取不到
                                                // 行时量出的高度是 0，整片列表会塌成空白。
                                                // 所以下标取不到片段时渲染一个等高占位行，绝不丢行。
                                                let pos = this.subtitle_filter.get(i).copied().unwrap_or(i);
                                                let Some(seg) = this.state.segments.get(pos) else {
                                                    return div()
                                                        .id(("table-row-placeholder", i))
                                                        .w_full()
                                                        .h(px(Theme::TABLE_ROW_H))
                                                        .border_b_1()
                                                        .border_color(Theme::border_subtle())
                                                        .into_any_element();
                                                };
                                                let seg_idx = seg.index;
                                                let is_selected = sel == Some(seg_idx);
                                                let is_playing_here = cur_time >= seg.start && cur_time <= seg.end;
                                                let start_ts = seconds_to_timestamp_short(seg.start);
                                                let end_ts = seconds_to_timestamp_short(seg.end);
                                                let raw_text = seg.display_text().to_string();
                                                let trans_text = seg.translation.as_deref().unwrap_or("—").to_string();
                                                let speaker = seg.speaker;

                                                div()
                                                    .id(("table-row-seg", seg_idx))
                                                    .w_full()
                                                    .h(px(Theme::TABLE_ROW_H))
                                                    .px(px(SUBTITLE_TABLE_PAD_X))
                                                    .border_b_1()
                                                    .border_color(Theme::border_subtle())
                                                    .cursor_pointer()
                                                    .bg(if is_selected {
                                                        Theme::tint_mint_soft()
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
                                                    .items_center()
                                                    .gap(px(SUBTITLE_TABLE_GAP))
                                                    // 序号
                                                    .child(
                                                        div()
                                                            .w(px(SUBTITLE_TABLE_COL_INDEX_W))
                                                            .text_center()
                                                            .text_size(px(Theme::TEXT_BODY_LG))
                                                            .font_weight(if is_selected { FontWeight::BOLD } else { FontWeight::NORMAL })
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_muted() })
                                                            .child(format!("{}", seg_idx)),
                                                    )
                                                    // 开始时间 (图二高精时间戳)
                                                    .child(
                                                        div()
                                                            .w(px(SUBTITLE_TABLE_COL_TIME_W))
                                                            .text_center()
                                                            .font_family("Consolas")
                                                            .text_size(px(Theme::TEXT_BODY))
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_secondary() })
                                                            .child(start_ts),
                                                    )
                                                    // 结束时间 (图二高精时间戳)
                                                    .child(
                                                        div()
                                                            .w(px(SUBTITLE_TABLE_COL_TIME_W))
                                                            .text_center()
                                                            .font_family("Consolas")
                                                            .text_size(px(Theme::TEXT_BODY))
                                                            .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_secondary() })
                                                            .child(end_ts),
                                                    )
                                                    // 说话人标签（分离关闭或该段无有效人声时为 —）
                                                    .child(
                                                        div()
                                                            .w(px(SUBTITLE_TABLE_COL_SPEAKER_W))
                                                            .flex()
                                                            .justify_center()
                                                            .child(match speaker {
                                                                Some(s) => div()
                                                                    .px_1p5()
                                                                    .py_0p5()
                                                                    .rounded_md()
                                                                    .border_1()
                                                                    .border_color(speaker_border(s))
                                                                    .bg(speaker_tint(s))
                                                                    .text_size(px(Theme::TEXT_SMALL))
                                                                    .font_weight(FontWeight::SEMIBOLD)
                                                                    .text_color(speaker_color(s))
                                                                    .child(format!("说话人 {}", s + 1))
                                                                    .into_any_element(),
                                                                None => div()
                                                                    .text_size(px(Theme::TEXT_BODY))
                                                                    .text_color(Theme::text_muted())
                                                                    .child("—")
                                                                    .into_any_element(),
                                                            }),
                                                    )
                                                    // 字幕内容 (原文，清晰中文字体；单行截断保证虚拟列表行高一致)
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w(px(SUBTITLE_TABLE_COL_TEXT_MIN_W))
                                                            .text_size(px(Theme::TEXT_BODY_LG))
                                                            .font_weight(if is_selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                                            .text_color(if is_selected { Theme::text_primary() } else { Theme::text_primary() })
                                                            .truncate()
                                                            .child(raw_text),
                                                    )
                                                    // 翻译字幕 (多语言对照)
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w(px(SUBTITLE_TABLE_COL_TEXT_MIN_W))
                                                            .text_size(px(Theme::TEXT_BODY_LG))
                                                            .text_color(if is_selected {
                                                                Theme::text_primary()
                                                            } else if trans_text == "—" {
                                                                Theme::text_muted()
                                                            } else {
                                                                Theme::text_secondary()
                                                            })
                                                            .truncate()
                                                            .child(trans_text),
                                                    )
                                                    .into_any_element()
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
        let (fg, bg, border) = subtitle_preset_colors(&style.preset_name);
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
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.end_preview_box_drag();
                cx.notify();
            }))
            // 在行**外**松手也要收尾。GPUI 的 `on_mouse_up` 同样要求指针落在元素内，
            // 若用户把指针拖出行外才松手，上面的 handler 收不到，`preview_drag` 会一直
            // 挂着——预览行从此恒亮「拖拽中」描边，且宽度卡在半途的中间值。
            // `on_mouse_up_out` 走捕获阶段、专在「松手时指针不在元素内」时触发，用它兜底。
            // 没有拖拽会话时 `end_preview_box_drag` 是空操作，因此它在任何别处松手都安全。
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.end_preview_box_drag();
                cx.notify();
            }))
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
                            .font_weight(FontWeight::BOLD)
                            .text_color(fg)
                            .text_align(TextAlign::Center)
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
                        .max_h(px(Theme::DROPDOWN_MAX_H))
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
                                    this.is_export_dropdown_open = false;
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_BODY))
                                        .font_weight(if is_selected { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                        .text_color(if is_selected { Theme::accent_mint() } else { Theme::text_primary() })
                                        .child(fmt.label()),
                                )
                                .child(
                                    if is_selected {
                                        div()
                                            .text_size(px(Theme::TEXT_CAPTION))
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
                            .h(px(Theme::CTRL_H_LG))
                            .px(px(Theme::SPACE_4))
                            .rounded(px(Theme::RADIUS_LG))
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
        let progress = self.state.translate_progress.clamp(0.0, 1.0) as f32;
        let status = self.state.translate_status_msg.clone();
        let done = self.state.translated_count_for(&target);
        let total = self.state.segments.len();
        let can_run = total > 0 && !is_translating;
        // 已带译文、但**不是**当前目标语言的句数。
        // 有了它才能解释「为什么按钮不是『重新翻译』」：用户切换目标语言后
        // 旧译文仍然在，但当前语言一句都没有——不提示的话，界面看起来像
        // 「翻译记录丢了」，用户会以为程序把之前的成果清空了。
        let other_lang = self
            .state
            .segments
            .iter()
            .filter(|s| {
                s.translation.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false)
                    && !s.translation_matches(&target)
            })
            .count();

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
                        if self.state.config.translate.effective_api_key().trim().is_empty() {
                            "未配置 API Key，请在「性能设置」中填写".to_string()
                        } else {
                            format!("模型 {}", model)
                        }
                    } else {
                        "本地 Qwen，无需密钥".to_string()
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
            .gap_2()
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
                    } else if done == total && total > 0 {
                        // 当前语言全部译完：再点只会被引擎的增量逻辑判为「无需翻译」，
                        // 文案说清楚，避免用户以为按钮失灵。
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
            .child(
                if is_translating {
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
                },
            )
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
                        } else if done == total && total > 0 {
                            format!("已全部翻译为{target}")
                        } else if other_lang > 0 && done == 0 {
                            // 关键提示：换语言后旧译文仍在，只是不属于当前目标语言。
                            // 不说明的话，界面看起来像翻译记录被清空了。
                            format!("已有 {other_lang} 句其他语言译文；点上方按钮可译成{target}")
                        } else if done > 0 {
                            format!("{target}译文 {done}/{total} 句，可继续补译")
                        } else {
                            format!("尚未翻译（共 {total} 句）")
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
                        // 计数徽标走 primitives::count_tag，仅按完成量覆盖文字色
                        primitives::count_tag(format!("{}/{} 句已翻译", done, total))
                            .text_color(if done > 0 {
                                Theme::accent_mint()
                            } else {
                                Theme::text_secondary()
                            }),
                    ),
            )
            .child(mode_row)
            .child(lang_row)
            .child(action_row)
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
    fn render_punct_btn(&mut self, punct: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
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
            let hi = (((i + 1) * peaks.len()) / bars).max(lo + 1).min(peaks.len());
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
                                div()
                                    .flex_1()
                                    .flex()
                                    .justify_center()
                                    .items_center()
                                    .child(
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
    pub(crate) fn render_multitrack_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                                    .child(format!("共 {} 句 · 总长 {}", self.state.segments.len(), format_duration_short(total_dur))),
                            ),
                    )
                    // 右侧：快捷键速查（与 shortcuts::SHORTCUT_HINTS 同源，只挑高频几项，避免占满工具条）
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .children(
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
    pub(crate) fn seek_by_mouse_x(&mut self, mouse_x: Pixels, window_width: Pixels, is_drag: bool, cx: &mut Context<Self>) {
        if self.state.total_duration <= 0.0 {
            return;
        }
        self.halt_preview_playback();
        // 三个偏移量必须与布局里的实际值同源，否则点击位置与播放头会整体错位。
        let nav_w = px(Theme::NAV_W);
        let left_pad = px(Theme::TRACK_LABEL_W);
        let right_pad = px(Theme::TRACK_RIGHT_PAD);

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
