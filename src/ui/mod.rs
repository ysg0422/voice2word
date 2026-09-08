//! Voice2Word GPUI 界面主视窗
//! 遵循 Codex / Zed 极简现代深色风格

pub mod editor;
pub mod theme;

use gpui::prelude::*;
use gpui::*;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

use crate::app::state::{AppState, ProcessStatus, WorkspaceTab};
use crate::core::PipelineEvent;
use crate::subtitle::SubtitleWriter;
use crate::utils::time::format_duration_short;
use theme::Theme;

use tokio::sync::watch;

pub struct MainWindow {
    state: AppState,
    metrics_rx: watch::Receiver<crate::app::ResourceMetrics>,
}

impl MainWindow {
    pub fn new(state: AppState, _cx: &mut Context<Self>) -> Self {
        let metrics_rx = crate::utils::SystemMonitor::spawn_background_monitor();
        Self {
            state,
            metrics_rx,
        }
    }

    /// 打开系统原生文件对话框选择音视频
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        if let Some(file) = rfd::FileDialog::new()
            .add_filter(
                "音视频文件",
                &[
                    "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
                ],
            )
            .pick_file()
        {
            info!("用户选择了文件: {:?}", file);
            let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let dur = ffmpeg.get_duration(&file);
            self.state.total_duration = dur;
            self.state.current_time = 0.0;
            self.state.set_selected_file(file);
            self.state.segments.clear();
            self.state.status = ProcessStatus::Idle;
            self.trigger_extract_frame(cx);
            cx.notify();
        }
    }

    /// 触发流水线处理
    fn start_processing(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        let input_file = match &self.state.selected_file {
            Some(f) => f.clone(),
            None => return,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.state.status = ProcessStatus::Processing {
            stage: "准备中...".to_string(),
            progress: 0.0,
            detail: "正在启动异步任务管线".to_string(),
        };
        self.state.segments.clear();
        cx.notify();

        let pipeline = self.state.pipeline.clone();
        let lang = if self.state.language == "auto" {
            None
        } else {
            Some(self.state.language.clone())
        };
        let fmt = self.state.output_format.clone();
        let polish = self.state.enable_polish;
        let threads = Some(self.state.whisper_threads);

        // 后台通过独立线程执行 Tokio 异步管线，不阻塞 UI 主线程
        let in_file = input_file.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let _ = pipeline
                    .run(in_file, None, lang, fmt, polish, threads, tx)
                    .await;
            });
        });

        // 使用 GPUI 官方实体协程封装，避免复用 AsyncApp 引用造成重入借用冲突。
        cx.spawn(async move |this, cx| {
            while let Some(first_event) = rx.recv().await {
                // Whisper 会连续产生大量事件，先聚合一个短窗口，保证 GPUI
                // 每帧只执行一次实体更新，避免高频重入 App RefCell。
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let mut events = vec![first_event];
                while let Ok(event) = rx.try_recv() {
                    events.push(event);
                }

                let _ = this.update(cx, |this, cx| {
                    for event in events {
                        match event {
                                PipelineEvent::StageChanged(stage) => {
                                    if let ProcessStatus::Processing { stage: ref mut s, .. } =
                                        &mut this.state.status
                                    {
                                        *s = stage;
                                    }
                                }
                                PipelineEvent::Progress { stage, progress, detail } => {
                                    this.state.status = ProcessStatus::Processing {
                                        stage,
                                        progress,
                                        detail,
                                    };
                                }
                                PipelineEvent::SegmentStream(seg) => {
                                    this.state.segments.push(seg);
                                }
                                PipelineEvent::Finished(segments) => {
                                    this.state.status = ProcessStatus::Completed;
                                    this.state.segments = segments.clone();
                                    if let Some(first) = segments.first() {
                                        this.state.select_segment(first.index);
                                    }
                                    if this.state.total_duration <= 0.0 {
                                        this.state.total_duration =
                                            segments.last().map(|s| s.end).unwrap_or(0.0);
                                    }
                                    if let Some(ref input_file) = this.state.selected_file {
                                        let filename = input_file
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                             .unwrap_or("media")
                                            .to_string();
                                        let total_dur = this.state.total_duration;
                                        let _ = this.state.db.insert_task(
                                            &input_file.to_string_lossy(),
                                            &filename,
                                            total_dur,
                                            "completed",
                                            &segments,
                                        );
                                        this.state.refresh_recent_tasks();
                                    }
                                    this.state.active_tab = WorkspaceTab::Editor;
                                    this.trigger_extract_frame(cx);
                                }
                                PipelineEvent::Error(err) => {
                                    this.state.status = ProcessStatus::Failed(err);
                                }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 导出字幕文件
    fn export_subtitles(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }

        let fmt = &self.state.output_format;
        if let Some(save_path) = rfd::FileDialog::new()
            .set_file_name(&format!("subtitle.{}", fmt))
            .add_filter("Subtitle", &[fmt.as_str()])
            .save_file()
        {
            if let Err(e) =
                SubtitleWriter::write_to_file(&self.state.segments, &save_path, fmt)
            {
                self.state.status = ProcessStatus::Failed(format!("导出失败: {}", e));
            } else {
                info!("字幕成功导出至: {:?}", save_path);
            }
            cx.notify();
        }
    }

    /// 使用 FFplay 播放当前视频，并通过 subtitles 滤镜叠加已生成字幕。
    fn play_video(&mut self, cx: &mut Context<Self>) {
        let Some(input_file) = self.state.selected_file.clone() else {
            return;
        };
        if self.state.segments.is_empty() {
            self.state.status = ProcessStatus::Failed("请先完成字幕识别，再播放预览".to_string());
            cx.notify();
            return;
        }

        let subtitle_path = std::env::temp_dir().join(format!(
            "voice2word_preview_{}.srt",
            std::process::id()
        ));
        if let Err(error) = SubtitleWriter::write_srt(&self.state.segments, &subtitle_path) {
            self.state.status = ProcessStatus::Failed(format!("生成预览字幕失败: {}", error));
            cx.notify();
            return;
        }

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffplay_path = ffmpeg_path.with_file_name("ffplay.exe");
        // Run ffplay from the temp directory and pass only the SRT filename.
        // This avoids Windows drive-letter colons being parsed as filter options.
        let subtitle_name = subtitle_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("voice2word_preview.srt");
        let cur_time = format!("{:.3}", self.state.current_time);
        match std::process::Command::new(ffplay_path)
            .arg("-ss")
            .arg(&cur_time)
            .arg("-loglevel")
            .arg("error")
            .arg("-window_title")
            .arg("Voice2Word 视频字幕预览")
            .arg("-vf")
            .arg(format!("subtitles=filename='{subtitle_name}'"))
            .arg("-autoexit")
            .arg(input_file)
            .current_dir(std::env::temp_dir())
            .spawn()
        {
            Ok(_) => info!("已启动带字幕视频预览"),
            Err(error) => {
                self.state.status = ProcessStatus::Failed(format!("启动视频预览失败: {}", error));
                cx.notify();
            }
        }
    }

    /// 异步根据当前播放时间抽取单帧画面用于监视器显示（带智能缓存预加载）
    fn trigger_extract_frame(&mut self, cx: &mut Context<Self>) {
        let Some(video_path) = self.state.selected_file.clone() else { return; };
        if !video_path.exists() {
            return;
        }
        let time = self.state.current_time;
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
        let cache = self.state.frame_cache.clone();

        // 使用缓存系统获取帧（缓存命中时 <10ms，未命中时 200-500ms）
        let video_clone = video_path.clone();
        cx.spawn(async move |this, cx| {
            let frame_result = cx.background_executor().spawn(async move {
                cache.get_or_extract(&video_clone, time, &ffmpeg)
            })
            .await;

            if let Ok(frame_path) = frame_result {
                let _ = this.update(cx, |this, cx| {
                    this.state.preview_frame_path = Some(frame_path);
                    cx.notify();
                });
            }
        })
        .detach();

        // 异步预加载周围帧（前后 10 秒），提升后续拖动流畅度
        let ffmpeg_arc = std::sync::Arc::new(ffmpeg);
        cache.preload_surrounding_frames(video_path, time, ffmpeg_arc);
    }

    /// 上一句字幕
    fn jump_prev_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() { return; }
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx > 1 { cur_idx - 1 } else { 1 };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 下一句字幕
    fn jump_next_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() { return; }
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx < self.state.segments.len() { cur_idx + 1 } else { self.state.segments.len() };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 切换走帧播放模式
    fn toggle_play_preview(&mut self, cx: &mut Context<Self>) {
        self.state.is_playing = !self.state.is_playing;
        if self.state.is_playing {
            // 启动定时走帧任务
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
                    let should_continue = this.update(cx, |this, cx| {
                        if !this.state.is_playing {
                            return false;
                        }
                        let next_time = this.state.current_time + 0.5;
                        let max_dur = if this.state.total_duration > 0.0 {
                            this.state.total_duration
                        } else {
                            this.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                        };
                        if next_time >= max_dur {
                            this.state.is_playing = false;
                            cx.notify();
                            return false;
                        }
                        this.state.seek_to(next_time);
                        this.trigger_extract_frame(cx);
                        cx.notify();
                        true
                    }).unwrap_or(false);

                    if !should_continue {
                        break;
                    }
                }
            }).detach();
        }
        cx.notify();
    }
}

#[cfg(target_os = "windows")]
mod win_drag {
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn ReleaseCapture() -> i32;
        fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    pub fn drag_window() {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd != 0 {
                ReleaseCapture();
                const WM_NCLBUTTONDOWN: u32 = 0x00A1;
                const HTCAPTION: usize = 2;
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
            }
        }
    }
}

impl MainWindow {
    /// 渲染顶部现代化自定义标题栏 (支持原生窗口拖拽与系统控制按钮)
    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_file_name = self.state.selected_file.as_ref().map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

        div()
            .id("app-titlebar")
            .w_full()
            .h(px(36.0))
            .bg(Theme::bg_sidebar())
            .border_b_1()
            .border_color(Theme::border())
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .pl_3()
            // 左侧：品牌 Logo 与名称
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(8.0))
                            .h(px(8.0))
                            .rounded(px(4.0))
                            .bg(Theme::accent_mint()),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("Voice2Word"),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(Theme::text_muted())
                            .child("v2.0 Rust"),
                    ),
            )
            // 中间：居中显示当前打开的媒体文件名称，按住可拖拽移动窗口
            .child(
                div()
                    .id("titlebar-drag-area")
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                        #[cfg(target_os = "windows")]
                        win_drag::drag_window();
                    })
                    .child(
                        if let Some(name) = current_file_name {
                            div()
                                .text_size(px(12.0))
                                .text_color(Theme::text_secondary())
                                .child(format!("📄 {}", name))
                        } else {
                            div()
                                .text_size(px(11.0))
                                .text_color(Theme::text_muted())
                                .child("音视频智能字幕生成器 — 点击拖拽移动窗口")
                        }
                    ),
            )
            // 右侧：最小化、最大化、关闭按钮
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_full()
                    // 最小化
                    .child(
                        div()
                            .id("titlebar-btn-min")
                            .w(px(44.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_, _, window, _| {
                                window.minimize_window();
                            }))
                            .child("—"),
                    )
                    // 最大化 / 还原
                    .child(
                        div()
                            .id("titlebar-btn-max")
                            .w(px(44.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(Theme::bg_hover()).text_color(Theme::text_primary()))
                            .on_click(cx.listener(|_, _, window, _| {
                                window.zoom_window();
                            }))
                            .child("▢"),
                    )
                    // 关闭
                    .child(
                        div()
                            .id("titlebar-btn-close")
                            .w(px(44.0))
                            .h_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(13.0))
                            .text_color(Theme::text_secondary())
                            .hover(|s| s.bg(rgb(0xe11d48)).text_color(rgb(0xffffff)))
                            .on_click(cx.listener(|_, _, window, cx| {
                                window.remove_window();
                                cx.quit();
                            }))
                            .child("✕"),
                    ),
            )
    }
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.metrics_rx.has_changed().unwrap_or(false) {
            self.state.metrics = self.metrics_rx.borrow_and_update().clone();
        }

        let tab = self.state.active_tab;

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(Theme::bg_app())
            .text_color(Theme::text_primary())
            // 1. 顶部自定义标题栏
            .child(self.render_titlebar(cx))
            // 2. Tab 模式切换栏 (剪辑校对 vs 转写生成 vs 历史视频库)
            .child(self.render_tab_bar(cx))
            // 3. 核心布局切换
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(match tab {
                        WorkspaceTab::Editor => self.render_editor_layout(cx).into_any_element(),
                        WorkspaceTab::Generate => self.render_generate_layout(cx).into_any_element(),
                        WorkspaceTab::Library => self.render_library_layout(cx).into_any_element(),
                    }),
            )
    }
}

impl MainWindow {
    /// 渲染历史视频库 (视频资产管理与一键载入工作台)
    fn render_library_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(20.0))
                                            .font_weight(FontWeight::BOLD)
                                            .child("📚 历史视频资产库"),
                                    )
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded_md()
                                            .bg(Theme::bg_card())
                                            .text_size(px(11.0))
                                            .text_color(Theme::accent_mint())
                                            .child(format!("共 {} 个视频工程", total_count)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_muted())
                                    .child("本地 SQLite 数据库持久保存的所有历史任务。点击任意项目直接载入剪辑工作台进行音画核对或一键导出字幕。"),
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
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
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
                                    .child("➕ 导入新视频转写"),
                            )
                            .child(
                                div()
                                    .id("library-refresh-btn")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .bg(Theme::bg_sidebar())
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
                                    .child("🔄 刷新列表"),
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
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(40.0))
                                .child("🎬"),
                        )
                        .child(
                            div()
                                .text_size(px(16.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("视频库暂无解析历史"),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(Theme::text_muted())
                                .child("您转写过的所有视频与字幕工程都会自动保存在这里，随时可再次打开精修"),
                        )
                        .child(
                            div()
                                .id("empty-lib-goto-gen")
                                .mt_2()
                                .px_4()
                                .py_2()
                                .rounded_md()
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
                                .child("立即导入第一个视频进行转写"),
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
                                .rounded_lg()
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
                                                .w(px(52.0))
                                                .h(px(52.0))
                                                .rounded_md()
                                                .bg(rgb(0x18181c))
                                                .border_1()
                                                .border_color(Theme::border())
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_size(px(24.0))
                                                .child("🎬"),
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
                                                                .child(task.file_name),
                                                        )
                                                        .child(
                                                            div()
                                                                .px_1p5()
                                                                .py_0p5()
                                                                .rounded(px(3.0))
                                                                .bg(rgba(0x2dd4bf20))
                                                                .text_size(px(10.0))
                                                                .text_color(Theme::accent_mint())
                                                                .child("已就绪"),
                                                        ),
                                                )
                                                .child(
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .gap_3()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_muted())
                                                        .child(format!("⏱️ 时长: {}", dur_str))
                                                        .child(format!("📝 字幕: {} 句", seg_len))
                                                        .child(format!("📅 解析时间: {}", task.created_at)),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(11.0))
                                                        .text_color(Theme::text_secondary())
                                                        .child(format!("首句预览: \"{}\"", sample_text)),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            div()
                                                .id(("lib-edit-btn", task_id as usize))
                                                .px_3()
                                                .py_1p5()
                                                .rounded_md()
                                                .bg(Theme::accent_mint())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x09090b))
                                                .hover(|s| s.opacity(0.9))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.state.load_task(&task_clone);
                                                    this.trigger_extract_frame(cx);
                                                    cx.notify();
                                                }))
                                                .child("🎬 进入剪辑工作台"),
                                        )
                                        .child(
                                            div()
                                                .id(("lib-export-btn", task_id as usize))
                                                .px_3()
                                                .py_1p5()
                                                .rounded_md()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::text_secondary())
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(move |_this, _, _, _cx| {
                                                    if let Some(save_path) = rfd::FileDialog::new()
                                                        .set_file_name(&format!("{}.srt", task_export.file_name))
                                                        .add_filter("SubRip Subtitle", &["srt"])
                                                        .save_file()
                                                    {
                                                        let _ = SubtitleWriter::write_srt(&task_export.segments, &save_path);
                                                    }
                                                }))
                                                .child("💾 导出 SRT"),
                                        )
                                        .child(
                                            div()
                                                .id(("lib-del-btn", task_id as usize))
                                                .px_2()
                                                .py_1p5()
                                                .rounded_md()
                                                .bg(Theme::bg_sidebar())
                                                .border_1()
                                                .border_color(Theme::border())
                                                .cursor_pointer()
                                                .text_size(px(11.0))
                                                .text_color(Theme::accent_red())
                                                .hover(|s| s.bg(Theme::bg_hover()))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.state.delete_task_record(task_id);
                                                    cx.notify();
                                                }))
                                                .child("🗑️ 删除"),
                                        )
                                )
                        }))
                        .into_any_element()
                }
            )
    }

    /// 渲染智能生成模式主体布局
    fn render_generate_layout(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .flex_1()
            .w_full()
            .h_full()
            .overflow_hidden()
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .child(self.render_main_workspace(cx))
                    .child(self.render_bottom_timeline(cx)),
            )
    }

    /// 渲染左侧边栏 (Codex / Zed 风格极简深灰)
    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(300.0))
            .h_full()
            .bg(Theme::bg_sidebar())
            .border_r_1()
            .border_color(Theme::border())
            .p_4()
            .flex()
            .flex_col()
            .gap_4()
            // 文件导入卡片
            .child(
                div()
                    .id("media-select-card")
                    .p_3()
                    .rounded_md()
                    .bg(Theme::bg_card())
                    .border_1()
                    .border_color(Theme::border())
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.choose_file(cx);
                    }))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(Theme::text_muted())
                            .child("INPUT MEDIA"),
                    )
                    .child(
                        div()
                            .pt_1()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(match &self.state.selected_file {
                                Some(path) => path
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("已选择文件")
                                    .to_string(),
                                None => "点击选择音视频文件...".to_string(),
                            }),
                    ),
            )
            // 选项配置区 (胶囊选择器)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_muted())
                            .child("CONFIGURATIONS"),
                    )
                    // 语言选择
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .child("识别语言"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(self.render_option_pill("zh", "中文", cx))
                                    .child(self.render_option_pill("en", "英文", cx))
                                    .child(self.render_option_pill("ja", "日文", cx))
                                    .child(self.render_option_pill("auto", "自动", cx)),
                            ),
                    )
                    // 导出格式
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .child("导出格式"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(self.render_format_pill("srt", "SRT", cx))
                                    .child(self.render_format_pill("ass", "ASS", cx))
                                    .child(self.render_format_pill("txt", "TXT", cx)),
                            ),
                    )
                    // CPU 线程选择
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .child("Whisper 并发核心"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(self.render_thread_pill(4, "4核", cx))
                                    .child(self.render_thread_pill(8, "8核(推)", cx))
                                    .child(self.render_thread_pill(12, "12核", cx))
                                    .child(self.render_thread_pill(16, "16核(满)", cx)),
                            ),
                    )
                    // LLM 润色切换
                    .child(
                        div()
                            .id("toggle-polish-btn")
                            .flex()
                            .items_center()
                            .justify_between()
                            .p_2()
                            .rounded_md()
                            .bg(Theme::bg_card())
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.enable_polish = !this.state.enable_polish;
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_secondary())
                                    .child("Qwen LLM 润色"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(if self.state.enable_polish {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_muted()
                                    })
                                    .child(if self.state.enable_polish {
                                        "已启用"
                                    } else {
                                        "已停用"
                                    }),
                            ),
                    ),
            )
            // 硬件与本地模型资源监控对比卡片
            .child(self.render_hardware_monitor_card(cx))
            // 最近历史
            .child(
                        div()
                            .flex_1()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(Theme::text_muted())
                            .child("RECENT TASKS"),
                    )
                    .child(
                        div()
                            .id("task-history-scroll")
                            .flex_1()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(self.state.recent_tasks.iter().enumerate().map(|(idx, task)| {
                                let task_clone = task.clone();
                                div()
                                    .id(("task-item", idx))
                                    .p_2()
                                    .rounded_md()
                                    .bg(Theme::bg_card())
                                    .cursor_pointer()
                                    .hover(|s| s.bg(Theme::bg_hover()))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.selected_file = Some(PathBuf::from(&task_clone.file_path));
                                        this.state.status = ProcessStatus::Completed;
                                        this.state.segments = task_clone.segments.clone();
                                        let duration = if task_clone.duration > 0.0 {
                                            task_clone.duration
                                        } else {
                                            task_clone.segments.last().map(|s| s.end).unwrap_or(0.0)
                                        };
                                        this.state.total_duration = duration;
                                        if let Some(first) = this.state.segments.first() {
                                            this.state.select_segment(first.index);
                                        } else {
                                            this.state.current_time = 0.0;
                                            this.state.selected_segment_index = None;
                                            this.state.editing_text.clear();
                                        }
                                        this.state.active_tab = WorkspaceTab::Editor;
                                        this.trigger_extract_frame(cx);
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .text_size(px(12.0))
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(task.file_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(Theme::text_muted())
                                            .child(format!(
                                                "{} · {} 段",
                                                format_duration_short(task.duration),
                                                task.segments.len()
                                            )),
                                    )
                            })),
                    ),
            )
    }

    /// 渲染硬件与模型资源监控对比卡片 (CPU / 内存实时对比)
    fn render_hardware_monitor_card(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.state.metrics;
        let sys_cpu_pct = m.sys_cpu.clamp(0.0, 100.0);
        let proc_cpu_pct = m.proc_cpu.clamp(0.0, 100.0);

        let sys_mem_gb = m.sys_mem_used as f64 / (1024.0 * 1024.0 * 1024.0);
        let total_mem_gb = (m.sys_mem_total as f64 / (1024.0 * 1024.0 * 1024.0)).max(1.0);
        let sys_mem_pct = ((sys_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        let proc_mem_gb = m.proc_mem as f64 / (1024.0 * 1024.0 * 1024.0);
        let proc_mem_pct = ((proc_mem_gb / total_mem_gb) * 100.0).clamp(0.0, 100.0) as f32;

        div()
            .id("hardware-monitor-card")
            .p_3()
            .rounded_md()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border())
            .flex()
            .flex_col()
            .gap_2()
            // 标头行：标题 + 状态胶囊
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_muted())
                            .child("HARDWARE / MODEL METRICS"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded(px(3.0))
                                    .bg(if m.is_model_running {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_muted()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if m.is_model_running {
                                        Theme::accent_mint()
                                    } else {
                                        Theme::text_secondary()
                                    })
                                    .child(m.proc_name.clone()),
                            ),
                    ),
            )
            // 1. CPU 对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(if m.is_model_running {
                                        "CPU (大模型):"
                                    } else {
                                        "CPU (应用待机):"
                                    }))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_mint())
                                            .child(format!("{:.1}%", proc_cpu_pct)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("系统: {:.1}%", sys_cpu_pct)),
                            ),
                    )
                    // 双层条
                    .child(
                        div()
                            .w_full()
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(rgb(0x18181c))
                            .relative()
                            .overflow_hidden()
                            // 系统占用槽 (暗深灰)
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((sys_cpu_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(rgb(0x4a4a58)),
                            )
                            // 模型进程高亮条 (鲜艳 Mint 绿)
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((proc_cpu_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(Theme::accent_mint()),
                            ),
                    ),
            )
            // 2. 内存对比条
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .text_size(px(11.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(Theme::text_muted()).child(if m.is_model_running {
                                        "内存 (大模型):"
                                    } else {
                                        "内存 (应用待机):"
                                    }))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::accent_blue())
                                            .child(crate::app::ResourceMetrics::format_bytes(m.proc_mem)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(Theme::text_muted())
                                    .child(format!("{:.1} / {:.0} GB", sys_mem_gb, total_mem_gb)),
                            ),
                    )
                    // 双层内存条
                    .child(
                        div()
                            .w_full()
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(rgb(0x18181c))
                            .relative()
                            .overflow_hidden()
                            // 系统已用内存槽
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((sys_mem_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(rgb(0x4a4a58)),
                            )
                            // 模型占用内存条
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .h_full()
                                    .w(relative((proc_mem_pct / 100.0).clamp(0.0, 1.0)))
                                    .bg(Theme::accent_blue()),
                            ),
                    ),
            )
            // 3. 底部状态提示
            .child(
                div()
                    .pt_1()
                    .text_size(px(10.0))
                    .text_color(Theme::text_muted())
                    .child(if m.is_model_running {
                        "⚡ 本地计算模型正在全速运行中"
                    } else {
                        "💡 模型待命中 (开始处理时按需载入内存)"
                    }),
            )
    }

    fn render_option_pill(
        &mut self,
        val: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.language == val;
        div()
            .id(val)
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                Theme::accent_blue()
            } else {
                Theme::bg_card()
            })
            .text_color(if is_selected {
                Theme::text_primary()
            } else {
                Theme::text_secondary()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.language = val.to_string();
                cx.notify();
            }))
            .child(label)
    }

    fn render_format_pill(
        &mut self,
        val: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.output_format == val;
        div()
            .id(val)
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                Theme::accent_blue()
            } else {
                Theme::bg_card()
            })
            .text_color(if is_selected {
                Theme::text_primary()
            } else {
                Theme::text_secondary()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.output_format = val.to_string();
                cx.notify();
            }))
            .child(label)
    }

    fn render_thread_pill(
        &mut self,
        val: u32,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.state.whisper_threads == val;
        div()
            .id(label)
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(11.0))
            .cursor_pointer()
            .bg(if is_selected {
                Theme::accent_blue()
            } else {
                Theme::bg_card()
            })
            .text_color(if is_selected {
                Theme::text_primary()
            } else {
                Theme::text_secondary()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.whisper_threads = val;
                cx.notify();
            }))
            .child(label)
    }

    /// 渲染中央处理工作区：轻量化实时看板，彻底告别庞大表格卡顿
    fn render_main_workspace(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let is_completed = self.state.status == ProcessStatus::Completed;
        let seg_count = self.state.segments.len();
        let last_seg_text = self.state.segments.last().map(|s| s.display_text().to_string());

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
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(18.0))
                                    .font_weight(FontWeight::BOLD)
                                    .child("⚡ 智能语音转写生成"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(Theme::text_muted())
                                    .child("Silero VAD 静音切片加速 + Whisper ASR + 本地大模型标点纠错"),
                            ),
                    )
                    .child(
                        if let Some(ref file) = self.state.selected_file {
                            div()
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .text_size(px(11.0))
                                .text_color(Theme::accent_mint())
                                .child(format!("目标: {}", file.file_name().and_then(|s| s.to_str()).unwrap_or("视频")))
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
                                .w(px(64.0))
                                .h(px(64.0))
                                .rounded_full()
                                .bg(rgba(0x2dd4bf20))
                                .border_2()
                                .border_color(Theme::accent_mint())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(28.0))
                                .child("⚡"),
                        )
                        .child(
                            div()
                                .text_size(px(22.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child(stage),
                        )
                        // 大进度条
                        .child(
                            div()
                                .w(px(520.0))
                                .h(px(10.0))
                                .rounded(px(5.0))
                                .bg(rgb(0x18181c))
                                .border_1()
                                .border_color(Theme::border())
                                .overflow_hidden()
                                .child(
                                    div()
                                        .h_full()
                                        .rounded(px(5.0))
                                        .w(relative(progress.clamp(0.0, 1.0) as f32))
                                        .bg(Theme::accent_mint()),
                                ),
                        )
                        // 3个实时关键指标
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_4()
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("总体进度"))
                                        .child(div().text_size(px(16.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_mint()).child(format!("{:.1}%", progress * 100.0))),
                                )
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("已识别字幕"))
                                        .child(div().text_size(px(16.0)).font_weight(FontWeight::BOLD).text_color(Theme::text_primary()).child(format!("{} 句", seg_count))),
                                )
                                .child(
                                    div()
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(Theme::bg_card())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .child(div().text_size(px(10.0)).text_color(Theme::text_muted()).child("CPU 线程并发"))
                                        .child(div().text_size(px(16.0)).font_weight(FontWeight::BOLD).text_color(Theme::accent_blue()).child(format!("{} 核心", self.state.whisper_threads))),
                                ),
                        )
                        // 实时最新识别语句滚动跑马灯 (只渲染最新 1 句，毫秒级更新，0 卡顿)
                        .child(
                            div()
                                .w(px(520.0))
                                .p_3()
                                .rounded_md()
                                .bg(Theme::bg_sidebar())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(px(10.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(Theme::accent_mint())
                                        .child("🟢 实时捕获最新语句 (剪辑区可查看全量):"),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .text_color(Theme::text_primary())
                                        .child(if let Some(text) = last_seg_text {
                                            format!("\"{}\"", text)
                                        } else {
                                            "正在倾听与切片语音中...".to_string()
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(Theme::text_muted())
                                .child("💡 提示：转写完成后将自动切入「剪辑校对工作台」，您可直接拖动时间轴核对与修改错别字"),
                        )
                        .into_any_element()
                } else if is_completed || seg_count > 0 {
                    // 转写已完成大卡片
                    div()
                        .id("lightweight-completed-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_5()
                        .child(
                            div()
                                .w(px(64.0))
                                .h(px(64.0))
                                .rounded_full()
                                .bg(rgba(0x2dd4bf20))
                                .border_2()
                                .border_color(Theme::accent_mint())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(28.0))
                                .child("🎉"),
                        )
                        .child(
                            div()
                                .text_size(px(22.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(Theme::text_primary())
                                .child("转写处理已圆满完成！"),
                        )
                        .child(
                            div()
                                .text_size(px(13.0))
                                .text_color(Theme::text_secondary())
                                .child(format!(
                                    "共为您生成 {} 条精炼字幕，总时长 {}，已自动保存至本地视频库。",
                                    seg_count,
                                    format_duration_short(self.state.total_duration)
                                )),
                        )
                        // 大号行动按钮组
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_4()
                                .mt_2()
                                .child(
                                    div()
                                        .id("goto-editor-main-btn")
                                        .px_6()
                                        .py_3()
                                        .rounded_lg()
                                        .bg(Theme::accent_mint())
                                        .cursor_pointer()
                                        .text_size(px(14.0))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(0x09090b))
                                        .hover(|s| s.opacity(0.9))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.state.active_tab = WorkspaceTab::Editor;
                                            this.trigger_extract_frame(cx);
                                            cx.notify();
                                        }))
                                        .child("🎬 立即进入剪辑校对工作台 (主界面)"),
                                )
                                .child(
                                    div()
                                        .id("quick-export-main-btn")
                                        .px_5()
                                        .py_3()
                                        .rounded_lg()
                                        .bg(Theme::bg_sidebar())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .cursor_pointer()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(Theme::text_primary())
                                        .hover(|s| s.bg(Theme::bg_hover()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.export_subtitles(cx);
                                        }))
                                        .child("💾 导出字幕 (SRT / ASS)"),
                                )
                                .child(
                                    div()
                                        .id("goto-library-btn")
                                        .px_5()
                                        .py_3()
                                        .rounded_lg()
                                        .bg(Theme::bg_sidebar())
                                        .border_1()
                                        .border_color(Theme::border())
                                        .cursor_pointer()
                                        .text_size(px(13.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(Theme::text_secondary())
                                        .hover(|s| s.bg(Theme::bg_hover()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.state.active_tab = WorkspaceTab::Library;
                                            this.state.refresh_recent_tasks();
                                            cx.notify();
                                        }))
                                        .child("📚 打开视频库"),
                                ),
                        )
                        .into_any_element()
                } else {
                    // 空闲就绪引导卡片
                    div()
                        .id("lightweight-idle-dashboard")
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .child(
                            div()
                                .w(px(64.0))
                                .h(px(64.0))
                                .rounded_full()
                                .bg(Theme::bg_card())
                                .border_1()
                                .border_color(Theme::border())
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(28.0))
                                .child("📁"),
                        )
                        .child(
                            div()
                                .text_size(px(18.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(Theme::text_primary())
                                .child(if self.state.selected_file.is_some() {
                                    "已选定文件，点击下方「开始生成」启动转写"
                                } else {
                                    "请在左侧面板选择音视频文件"
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(Theme::text_muted())
                                .child("支持 MP4, MKV, MOV, FLV, MP3, WAV 等主流格式，全本地离线高速解析"),
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
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
                                        .bg(Theme::accent_mint())
                                        .cursor_pointer()
                                        .text_size(px(12.0))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(0x09090b))
                                        .hover(|s| s.opacity(0.9))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.choose_file(cx);
                                        }))
                                        .child("📁 选择音视频文件"),
                                )
                                .child(
                                    div()
                                        .id("idle-goto-library-btn")
                                        .px_4()
                                        .py_2()
                                        .rounded_md()
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
                                        .child("📚 从历史视频库打开"),
                                ),
                        )
                        .into_any_element()
                },
            )
    }

    /// 渲染底部状态与时间轴操作区
    fn render_bottom_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_processing = matches!(self.state.status, ProcessStatus::Processing { .. });
        let can_start = self.state.selected_file.is_some() && !is_processing;
        let can_export = !self.state.segments.is_empty();
        let can_play = self.state.selected_file.is_some() && can_export && !is_processing;

        let (stage_text, progress_val, detail_text) = match &self.state.status {
            ProcessStatus::Idle => ("就绪", 0.0, "请导入文件后开始".to_string()),
            ProcessStatus::Processing { stage, progress, detail } => {
                (stage.as_str(), *progress, detail.clone())
            }
            ProcessStatus::Completed => ("完成", 1.0, "字幕处理完成，支持直接导出".to_string()),
            ProcessStatus::Failed(e) => ("出错", 0.0, e.clone()),
        };

        div()
            .h(px(80.0))
            .bg(Theme::bg_sidebar())
            .border_t_1()
            .border_color(Theme::border())
            .px_6()
            .flex()
            .items_center()
            .justify_between()
            // 左侧状态指示
            .child(
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
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(stage_text.to_string()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::accent_mint())
                                    .child(format!("{:.1}%", progress_val * 100.0)),
                            ),
                    )
                    // 进度指示条
                    .child(
                        div()
                            .w(px(320.0))
                            .h(px(5.0))
                            .rounded(px(2.5))
                            .bg(Theme::bg_card())
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(progress_val.clamp(0.0, 1.0) as f32))
                                    .rounded(px(2.5))
                                    .bg(Theme::accent_mint()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(if is_processing {
                                Theme::text_primary()
                            } else {
                                Theme::text_muted()
                            })
                            .child(detail_text),
                    ),
            )
            // 右侧核心操作按钮
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .id("play-video-btn")
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if can_play { Theme::bg_card() } else { Theme::bg_app() })
                            .text_color(if can_play { Theme::text_primary() } else { Theme::text_muted() })
                            .border_1()
                            .border_color(Theme::border())
                            .on_click(cx.listener(|this, _, _, cx| this.play_video(cx)))
                            .child("▶ 播放预览"),
                    )
                    .child(
                        div()
                            .id("export-subtitles-btn")
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if can_export {
                                Theme::bg_card()
                            } else {
                                Theme::bg_app()
                            })
                            .text_color(if can_export {
                                Theme::text_primary()
                            } else {
                                Theme::text_muted()
                            })
                            .border_1()
                            .border_color(Theme::border())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.export_subtitles(cx);
                            }))
                            .child("💾 导出字幕"),
                    )
                    .child(
                        div()
                            .id("start-pipeline-btn")
                            .px_5()
                            .py_2()
                            .rounded_md()
                            .cursor_pointer()
                            .bg(if can_start {
                                Theme::accent_mint()
                            } else {
                                Theme::border()
                            })
                            .text_color(if can_start {
                                Theme::bg_app()
                            } else {
                                Theme::text_muted()
                            })
                            .font_weight(FontWeight::BOLD)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_processing(cx);
                            }))
                            .child(if is_processing {
                                "处理中..."
                            } else {
                                "▶ 开始处理"
                            }),
                    ),
            )
    }
}
