//! MainWindow 异步与后台业务动作逻辑

use gpui::prelude::*;
use gpui::*;
use tokio::sync::mpsc;
use tracing::info;

use crate::app::state::ProcessStatus;
use crate::core::PipelineEvent;
use crate::subtitle::SubtitleWriter;
use super::types::CompletionDialogInfo;
use super::MainWindow;

impl MainWindow {
    /// CPU 机器强制走 H.264 代理；有 GPU 默认原片，用户仍可手动打开。
    pub(crate) fn ensure_preview_proxy(&mut self, cx: &mut Context<Self>) {
        if !self.state.proxy_enabled {
            self.state.preview_source = self.state.selected_file.clone();
            self.state.proxy_busy = false;
            return;
        }
        let Some(source) = self.state.selected_file.clone() else {
            return;
        };
        if self.state.proxy_busy {
            return;
        }
        let cpu = !self.state.hardware.use_gpu_pipeline();
        let height = self.state.proxy_manager.preview_height(&source, cpu);
        if let Some(existing) = self.state.proxy_manager.existing_proxy(&source, height) {
            self.state.preview_source = Some(existing);
            self.state.proxy_busy = false;
            return;
        }
        self.state.proxy_busy = true;
        let proxy = self.state.proxy_manager.clone();
        info!(path = %source.display(), height, "后台生成 H.264 预览代理");
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { proxy.ensure_proxy(&source, height) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.state.proxy_busy = false;
                match result {
                    Ok(path) => {
                        this.state.preview_source = Some(path);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "代理生成失败，预览回退原片");
                        this.state.preview_source = this.state.selected_file.clone();
                    }
                }
                this.trigger_extract_frame(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 打开系统原生文件对话框选择音视频（异步非阻塞，杜绝 Win32 模态循环导致的 GPUI RefCell 重入借用冲突）
    pub(crate) fn choose_file(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .add_filter(
                    "音视频文件",
                    &[
                        "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
                    ],
                )
                .pick_file()
                .await;
            if let Some(file_handle) = handle {
                let file = file_handle.path().to_path_buf();
                info!("用户选择了转写文件: {:?}", file);
                let _ = this.update(cx, |this, cx| {
                    let ffmpeg_path = crate::utils::AppConfig::resolve_path(&this.state.config.paths.ffmpeg);
                    let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
                    let dur = ffmpeg.get_duration(&file);
                    this.state.transcribe_duration = dur;
                    this.state.transcribe_file = Some(file);
                    this.state.status = ProcessStatus::Idle;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 触发流水线处理
    pub(crate) fn start_processing(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        let input_file = match &self.state.transcribe_file {
            Some(f) => f.clone(),
            None => return,
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.state.status = ProcessStatus::Processing {
            stage: "准备中...".to_string(),
            progress: 0.0,
            detail: "正在启动异步任务管线".to_string(),
        };
        cx.notify();

        let pipeline = self.state.pipeline.clone();
        let lang = if self.state.language == "auto" {
            None
        } else {
            Some(self.state.language.clone())
        };
        let fmt = self.state.output_format.clone();
        let polish = self.state.enable_polish;
        let polish_mode = Some(self.state.polish_mode.as_str().to_string());
        let threads = Some(self.state.whisper_threads);

        // 根据档位计算运行时模型路径覆盖（绝对路径）
        let model_override = {
            let rel = self.state.whisper_model_tier.model_relative_path();
            let abs = crate::utils::AppConfig::resolve_path(&rel);
            Some(abs)
        };

        // 后台通过独立线程执行 Tokio 异步管线，不阻塞 UI 主线程
        let in_file = input_file.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                if let Err(err) = pipeline
                    .run(in_file, None, lang, fmt, polish, polish_mode, threads, model_override, tx.clone())
                    .await
                {
                    let _ = tx.send(PipelineEvent::Error(format!("转写失败: {err}")));
                }
            });
        });

        // 使用 GPUI 官方实体协程封装，避免复用 AsyncApp 引用造成重入借用冲突。
        cx.spawn(async move |this, cx| {
            while let Some(first_event) = rx.recv().await {
                // Whisper 会连续产生大量事件，先聚合一个短窗口，保证 GPUI
                // 每帧只执行一次实体更新，避免高频重入 App RefCell。
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(80))
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
                                PipelineEvent::SegmentStream(_) => {}
                                PipelineEvent::Finished(segments, metrics) => {
                                    if segments.is_empty() {
                                        this.state.status = ProcessStatus::Failed("未能识别出任何有效字幕，请检查音频音量或识别语言设置".to_string());
                                    } else {
                                        let finished_file = this.state.transcribe_file.take();
                                        let file_path = finished_file.or_else(|| this.state.selected_file.clone());
                                        let filename = file_path
                                            .as_ref()
                                            .and_then(|p| p.file_name())
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("media.mp4")
                                            .to_string();
                                        let total_dur = if this.state.transcribe_duration > 0.0 {
                                            this.state.transcribe_duration
                                        } else if this.state.total_duration > 0.0 {
                                            this.state.total_duration
                                        } else {
                                            segments.last().map(|s| s.end).unwrap_or(0.0)
                                        };
                                        let seg_count = segments.len();

                                        // 1. 存入数据库历史记录 (包含 5 阶段性能统计指标)
                                        if let Some(ref path) = file_path {
                                            let _ = this.state.db.insert_task(
                                                &path.to_string_lossy(),
                                                &filename,
                                                total_dur,
                                                "completed",
                                                &segments,
                                                Some(&metrics),
                                            );
                                            this.state.refresh_recent_tasks();
                                        }

                                        // 2. 将本次成果同步至剪辑校对工作台
                                        if let Some(path) = file_path {
                                            this.state.selected_file = Some(path.clone());
                                            this.state.preview_source = Some(path);
                                        }
                                        this.state.segments = segments;
                                        if let Some(first) = this.state.segments.first() {
                                            this.state.select_segment(first.index);
                                        }
                                        this.state.total_duration = total_dur;

                                        // 3. 语音转写界面彻底解耦复位（等待下一次导入）
                                        this.state.status = ProcessStatus::Idle;
                                        this.state.transcribe_file = None;
                                        this.state.transcribe_duration = 0.0;

                                        // 4. 弹出全屏转写完成提醒弹窗 (包含性能基准看板)
                                        this.completion_dialog = Some(CompletionDialogInfo {
                                            file_name: filename,
                                            segment_count: seg_count,
                                            total_duration: total_dur,
                                            metrics: Some(metrics),
                                        });
                                    }
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

    /// 导出字幕文件（异步非阻塞）
    pub(crate) fn export_subtitles(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }

        let fmt = self.state.output_format.clone();
        let segments = self.state.segments.clone();
        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle");
        let default_name = format!("{}.{}", stem, fmt);

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_file_name(&default_name)
                .add_filter("Subtitle", &[fmt.as_str()])
                .save_file()
                .await;
            if let Some(save_handle) = handle {
                let save_path = save_handle.path().to_path_buf();
                let res = SubtitleWriter::write_to_file(&segments, &save_path, &fmt);
                let _ = this.update(cx, |this, cx| {
                    if let Err(e) = res {
                        this.state.status = ProcessStatus::Failed(format!("导出失败: {}", e));
                    } else {
                        info!("字幕成功导出至: {:?}", save_path);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 使用 FFplay 播放当前视频，并通过 subtitles 滤镜叠加已生成字幕。
    pub(crate) fn play_video(&mut self, cx: &mut Context<Self>) {
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

    /// 异步根据当前播放时间抽取单帧画面（单飞队列：最多 1 个 FFmpeg 实例并发，合并多余拖动请求）
    pub(crate) fn trigger_extract_frame(&mut self, cx: &mut Context<Self>) {
        let Some(video_path) = self.state.preview_media().cloned() else { return; };
        if !video_path.exists() {
            return;
        }
        let target_time = self.state.current_time;

        // 记录最新期望的时间戳
        {
            let mut pending = self.pending_extract_time.lock().unwrap();
            *pending = Some(target_time);
        }

        // 如果当前已有后台抽帧 worker 在运行，直接返回！运行中的 worker 抽完后会自动接取 pending_time
        if self.is_extracting_frame.compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ).is_err() {
            return;
        }

        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = std::sync::Arc::new(crate::engines::FFmpegEngine::new(ffmpeg_path));
        let cache = self.state.frame_cache.clone();
        let is_extracting = self.is_extracting_frame.clone();
        let pending_time = self.pending_extract_time.clone();

        cx.spawn(async move |this, cx| {
            loop {
                // 取出当前最新的目标时间
                let next_time = {
                    let mut lock = pending_time.lock().unwrap();
                    lock.take()
                };

                let time = match next_time {
                    Some(t) => t,
                    None => {
                        // 无待处理请求，释放锁并退出
                        is_extracting.store(false, std::sync::atomic::Ordering::SeqCst);
                        // 双重检查避免竞态退出
                        let has_more = pending_time.lock().unwrap().is_some();
                        if has_more && is_extracting.compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        ).is_ok() {
                            continue;
                        }
                        break;
                    }
                };

                let video_clone = video_path.clone();
                let ffmpeg_clone = ffmpeg.clone();
                let cache_clone = cache.clone();

                let frame_result = cx.background_executor().spawn(async move {
                    cache_clone.get_or_extract(&video_clone, time, &ffmpeg_clone)
                }).await;

                let is_latest = {
                    let lock = pending_time.lock().unwrap();
                    lock.is_none()
                };

                // 仅当当前抽取结果依然是最新位置时才提交 UI 渲染，杜绝旧帧闪现与滞后延迟感
                if is_latest {
                    if let Ok(frame_path) = frame_result {
                        let _ = this.update(cx, |this, cx| {
                            this.state.preview_frame_path = Some(frame_path);
                            cx.notify();
                        });
                    }
                }
            }
        }).detach();
    }

    pub(crate) fn halt_preview_playback(&mut self) {
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
            self.state.is_playing = false;
        }
        self.state.video_player.stop();
        self.play_tick_generation = self.state.video_player.generation();
    }

    /// 上一句字幕
    pub(crate) fn jump_prev_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() { return; }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx > 1 { cur_idx - 1 } else { 1 };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 下一句字幕
    pub(crate) fn jump_next_segment(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() { return; }
        self.halt_preview_playback();
        let cur_idx = self.state.selected_segment_index.unwrap_or(1);
        let new_idx = if cur_idx < self.state.segments.len() { cur_idx + 1 } else { self.state.segments.len() };
        self.state.select_segment(new_idx);
        self.trigger_extract_frame(cx);
        cx.notify();
    }

    /// 切换内嵌实时播放模式 (25fps 动态图像 + FFplay 同步伴音)
    pub(crate) fn toggle_play_preview(&mut self, cx: &mut Context<Self>) {
        if self.state.is_playing {
            // 暂停：先记下墙钟时间，保留最后一帧，避免闪黑或抽帧滞后。
            self.state.current_time = self.state.video_player.current_play_time();
            self.state.is_playing = false;
            self.state.video_player.pause_clock();
            self.trigger_extract_frame(cx);
            cx.notify();
        } else {
            let Some(video_file) = self.state.preview_media().cloned() else {
                return;
            };
            self.state.is_playing = true;
            let start_sec = self.state.current_time;
            self.state.video_player.play(video_file, start_sec);
            let tick_gen = self.state.video_player.generation();
            self.play_tick_generation = tick_gen;

            let player = self.state.video_player.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_millis(40)).await;
                    let should_continue = this.update(cx, |this, cx| {
                        if this.play_tick_generation != tick_gen {
                            return false;
                        }
                        if !this.state.is_playing {
                            return false;
                        }
                        let play_time = player.current_play_time();
                        let max_dur = if this.state.total_duration > 0.0 {
                            this.state.total_duration
                        } else {
                            this.state.segments.last().map(|s| s.end).unwrap_or(0.0)
                        };

                        if play_time >= max_dur && max_dur > 0.0 {
                            this.state.current_time = max_dur;
                            this.state.is_playing = false;
                            this.state.video_player.pause_clock();
                            this.trigger_extract_frame(cx);
                            cx.notify();
                            return false;
                        }

                        this.state.current_time = play_time;
                        if let Some(seg) = this.state.segments.iter().find(|s| s.start <= play_time && play_time <= s.end) {
                            this.state.selected_segment_index = Some(seg.index);
                        }
                        cx.notify();
                        true
                    }).unwrap_or(false);

                    if !should_continue {
                        break;
                    }
                }
            }).detach();
            cx.notify();
        }
    }
}
