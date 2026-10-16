//! 独立播放窗口：**应用内自绘**画面与字幕。
//!
//! # 为什么不是外挂 ffplay
//!
//! 早先用的是 `ffplay.exe -vf subtitles=...`：会额外弹一个控制台窗口，字幕还要靠
//! 滤镜重新渲染一遍，与画面上的样式未必一致。这里复用剪辑台那条内嵌链路——
//! 同一个 `VideoPlayerEngine`（ffmpeg 直出 BGRA 帧）与同一份 `SubtitleStyleConfig`
//! （字号 / 颜色 / 底框 / 折行阈值），所以窗口里看到的就是导出会得到的。
//!
//! # 与编辑页监视器的差别
//!
//! 监视器是「校对用」的：选框、手柄、拖拽、逐句定位。这个窗口只负责**看**：
//! 画面 + 字幕 + 播放控制，不做任何编辑，因此自成一个视图、不碰 `AppState`。

use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::*;
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use super::super::primitives;
use crate::engines::VideoPlayerEngine;
use crate::subtitle::Segment;
use crate::utils::SubtitleStyleConfig;

/// 播放节拍：约 25fps。与剪辑台的内嵌播放同频（`toggle_play_preview` 也是 40ms）。
const TICK_MS: u64 = 40;

pub struct PlaybackWindow {
    /// 独占一份播放引擎：不能与主窗口共用——共用会让「这里暂停」把剪辑台的预览
    /// 一起停掉，而两边播的很可能是同一支片子的不同时间点。
    player: Arc<VideoPlayerEngine>,
    media: PathBuf,
    segments: Vec<Segment>,
    style: SubtitleStyleConfig,
    /// 已构建的纹理：(版本, 宽, 高, 图)。换帧时把上一张交还 GPUI 回收——
    /// 精灵图集只按 image_id 回收，不还就等于每帧留一张常驻显存，几分钟就见底。
    cached: Option<(u64, u32, u32, Arc<RenderImage>)>,
    playing: bool,
    /// 节拍所属的播放代次：暂停/重播后旧循环看到代次不匹配就自行退出。
    tick: u64,
    current_time: f64,
    total_duration: f64,
}

impl PlaybackWindow {
    pub fn new(
        media: PathBuf,
        segments: Vec<Segment>,
        style: SubtitleStyleConfig,
        ffmpeg: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let total_duration = segments.last().map(|s| s.end).unwrap_or(0.0);
        let mut this = Self {
            player: Arc::new(VideoPlayerEngine::new(ffmpeg)),
            media,
            segments,
            style,
            cached: None,
            playing: false,
            tick: 0,
            current_time: 0.0,
            total_duration,
        };
        // 开窗即播：用户点「播放」的意图很明确，不该再让他找一次播放键。
        this.start(cx);
        this
    }

    /// 起播（或从暂停处继续）。
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.player.play(&self.media, self.current_time);
        self.playing = true;
        let gen = self.player.generation();
        self.tick = gen;
        let player = self.player.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(TICK_MS))
                    .await;
                let keep = this
                    .update(cx, |this, cx| {
                        // 代次变了（又点了一次播放）或已暂停：本循环退出，交给新的那个。
                        if this.tick != gen || !this.playing {
                            return false;
                        }
                        this.current_time = player.current_play_time();
                        if this.total_duration > 0.0 && this.current_time >= this.total_duration {
                            this.current_time = this.total_duration;
                            this.playing = false;
                            player.pause_clock();
                            cx.notify();
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    /// 暂停：先记下墙钟时间再停钟，否则暂停后字幕会跟着指针继续跑。
    fn pause(&mut self, cx: &mut Context<Self>) {
        self.current_time = self.player.current_play_time();
        self.playing = false;
        self.player.pause_clock();
        cx.notify();
    }

    /// 当前时间落在哪一句字幕上（与编辑页同一判据：左闭右开）。
    fn active_segment(&self) -> Option<&Segment> {
        let t = self.current_time;
        self.segments.iter().find(|s| t >= s.start && t < s.end)
    }

    /// 取这一帧要显示的纹理：有新帧就重建，否则复用上一张。
    ///
    /// 与编辑页 `render_monitor_picture` 同法，包括「换帧时把旧图交还 GPUI」那一步——
    /// 少了它，播放几分钟就会因为显存里堆满历史帧而掉帧。
    fn take_image(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        let Some((version, frame)) = self.player.take_unconsumed_frame() else {
            return self.cached.as_ref().map(|(_, _, _, img)| img.clone());
        };
        let (w, h) = (frame.width, frame.height);
        // 帧已被 take 成独占 Arc，try_unwrap 必然成功；clone 只是异常路径的兜底。
        let data = match Arc::try_unwrap(frame) {
            Ok(f) => f.data,
            Err(arc) => arc.data.clone(),
        };
        let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, data)?;
        let new_img = Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)));
        if let Some((_, _, _, old)) = self.cached.take() {
            cx.drop_image(old, Some(window));
        }
        self.cached = Some((version, w, h, new_img.clone()));
        Some(new_img)
    }
}

impl Render for PlaybackWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let image = self.take_image(window, cx);
        let subtitle = self
            .active_segment()
            .map(|s| s.display_text().to_string())
            .unwrap_or_default();
        let playing = self.playing;
        // 字幕按**用户自己的样式**渲染，而不是写死的白字 22px：字号沿用配置里的
        // font_size（1080p 基准）按窗口高度等比换算——与监视器同一个口径，所以小窗口
        // 里字幕不会大得离谱；颜色取 primary_color。
        let video_h = (f32::from(window.bounds().size.height) - 56.0).max(1.0);
        let (cr, cg, cb) = crate::utils::SubtitleStyleConfig::hex_to_rgb(&self.style.primary_color);
        let font_px = (self.style.font_size as f32 * video_h / 1080.0).clamp(12.0, 96.0);
        let text_color = rgb((cr as u32) << 16 | (cg as u32) << 8 | (cb as u32));

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x000000))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(
                        // 参数名不能叫 img：会把 gpui 的 img() 构造函数遮蔽成局部变量
                        image.map(|picture| {
                            img(ImageSource::Render(picture)).max_w_full().max_h_full()
                        }),
                    )
                    // 字幕叠在画面底部：字号按窗口宽度等比缩放，与监视器同一套换算口径
                    // （1080p 基准）。写死 px 会让小窗口上的字幕大得离谱。
                    .child(
                        div()
                            .absolute()
                            .bottom(px(20.0))
                            .left_0()
                            .right_0()
                            .flex()
                            .justify_center()
                            .px(px(16.0))
                            .child(
                                div()
                                    .max_w_full()
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgba(0x000000cc))
                                    .text_color(text_color)
                                    .text_size(px(font_px))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(subtitle),
                            ),
                    ),
            )
            .child(
                div()
                    .h(px(56.0))
                    .flex_shrink_0()
                    .px(px(16.0))
                    .bg(rgb(0x18181b))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        primitives::pill_btn(if playing { "暂停" } else { "播放" })
                            .id("playback-toggle")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.playing {
                                    this.pause(cx);
                                } else {
                                    this.start(cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(0xa1a1aa))
                            .child(format!(
                                "{} / {}",
                                crate::ui::format_mm_ss(self.current_time),
                                crate::ui::format_mm_ss(self.total_duration),
                            )),
                    )
                    .child(
                        primitives::pill_btn("关闭")
                            .id("playback-close")
                            .on_click(cx.listener(|_, _, window, _| window.remove_window())),
                    ),
            )
    }
}

impl super::super::MainWindow {
    /// 弹出一个**独立窗口**播放这支片子（内嵌自绘画面 + 字幕）。
    ///
    /// 只把渲染需要的三样东西（媒体路径 / 字幕 / 样式）搬进新窗口，不共享 `AppState`：
    /// 播放是只读的，窗口活着的时候主窗口还在被编辑，共享状态只会带来「一边改一边放」
    /// 的竞态。字幕取的是**点击那一刻**的快照，这也符合「我就是要看这一版」的预期。
    pub(crate) fn open_playback_window(
        &mut self,
        media: PathBuf,
        segments: Vec<Segment>,
        title: String,
        cx: &mut Context<Self>,
    ) {
        let ffmpeg = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let style = self.state.config.subtitle_style.clone();
        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(480.0), px(320.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| cx.new(|cx| PlaybackWindow::new(media, segments, style, ffmpeg, cx)),
        )
        .ok();
    }
}
