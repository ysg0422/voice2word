//! MainWindow 异步与后台业务动作逻辑

use gpui::prelude::*;
use std::path::PathBuf;
use tokio::sync::mpsc;
use tracing::info;

use crate::app::state::{ProcessStatus, QueueState, WorkspaceTab};
use crate::core::PipelineEvent;
use crate::subtitle::SubtitleWriter;
use super::types::{BatchSummary, CompletionDialogInfo};
use super::{EditorExportFormat, MainWindow};

/// 支持导入的音视频扩展名。文件对话框、拖放、批量队列三处共用同一份，
/// 避免某个入口悄悄漏掉一种格式。
pub(crate) const MEDIA_EXTS: [&str; 10] = [
    "mp4", "mkv", "mov", "avi", "flv", "webm", "mp3", "wav", "flac", "m4a",
];

impl MainWindow {
    /// CPU 机器强制走 H.264 代理；有 GPU 默认原片，用户仍可手动打开。
    pub(crate) fn ensure_preview_proxy(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
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
                let _ = this.update(cx, |this, cx| {
                    this.adopt_media_file(file, cx);
                });
            }
        })
        .detach();
    }

    /// 接纳一个刚导入的媒体文件：立刻发布选中状态，再在后台探测时长。
    ///
    /// 「浏览本地文件」与「拖拽文件进窗口」共用这一条路径，避免两种入口
    /// 各写一套时长探测逻辑而产生行为漂移。
    pub(crate) fn adopt_media_file(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if !file.is_file() {
            self.state.status = ProcessStatus::Failed(format!(
                "无法导入：{} 不是有效文件",
                file.display()
            ));
            cx.notify();
            return;
        }
        info!("用户导入了转写文件: {:?}", file);
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        // 先把选中状态发布出去，让界面立刻有反馈；ffmpeg 探测放到后台执行器，
        // 避免阻塞这次实体更新闭包。
        self.state.transcribe_duration = 0.0;
        self.state.transcribe_file = Some(file.clone());
        self.state.status = ProcessStatus::Idle;
        self.state.active_tab = WorkspaceTab::Generate;
        cx.notify();

        let duration_file = file.clone();
        cx.spawn(async move |this, cx| {
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let duration = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_duration(&duration_file) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.state.transcribe_file.as_ref() == Some(&file) {
                    this.state.transcribe_duration = duration;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 处理从操作系统拖入窗口的文件（GPUI 把外部文件拖放包装成一次内部 drag/drop）。
    ///
    /// 拖入多个文件时全部进入批量队列 (F-012)：先入队并统一探测时长，再等用户
    /// 点「开始全部」。单个文件仍走原有的「选中 + 探测时长」路径，保持单文件
    /// 交互手感不变。全部扩展名都不支持时给出明确提示，而不是静默无反应。
    pub(crate) fn handle_dropped_files(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let media: Vec<PathBuf> = paths
            .iter()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();

        match media.len() {
            0 => {
                self.state.status = ProcessStatus::Failed(
                    "拖入的文件不是支持的音视频格式（支持 mp4/mkv/mov/avi/flv/webm/mp3/wav/flac/m4a）"
                        .to_string(),
                );
                cx.notify();
            }
            1 => self.adopt_media_file(media.into_iter().next().unwrap(), cx),
            _ => {
                let added = self.state.enqueue_files(media);
                if added == 0 {
                    self.state.status = ProcessStatus::Failed(
                        "这些文件已经在批量队列里，未重复添加".to_string(),
                    );
                    cx.notify();
                    return;
                }
                info!("批量队列新增 {} 个文件", added);
                // 队列面板在转写页，切过去用户才看得见刚入队的东西
                self.state.active_tab = WorkspaceTab::Generate;
                self.probe_queue_durations(cx);
                cx.notify();
            }
        }
    }

    /// 批量选择文件加入队列（转写页「批量导入」按钮）
    pub(crate) fn choose_batch_files(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let handles = rfd::AsyncFileDialog::new()
                .add_filter("音视频文件", &MEDIA_EXTS)
                .pick_files()
                .await;
            let Some(handles) = handles else { return };
            let files: Vec<PathBuf> = handles.iter().map(|h| h.path().to_path_buf()).collect();
            let _ = this.update(cx, |this, cx| {
                let added = this.state.enqueue_files(files);
                if added == 0 {
                    this.state.status =
                        ProcessStatus::Failed("选中的文件已经在批量队列里".to_string());
                } else {
                    info!("批量队列新增 {} 个文件", added);
                }
                this.state.active_tab = WorkspaceTab::Generate;
                this.probe_queue_durations(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 为队列中尚未探测时长的条目补齐时长（批量完成弹窗要按累计时长做统计）。
    ///
    /// 串行探测而不是并发：`ffmpeg -i` 对本地文件是毫秒级操作，并发派发只会让
    /// 进程表瞬间多出一堆短命进程，收益为零。
    pub(crate) fn probe_queue_durations(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<PathBuf> = self
            .state
            .batch_queue
            .iter()
            .filter(|item| item.duration <= 0.0)
            .map(|item| item.path.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        cx.spawn(async move |this, cx| {
            for path in pending {
                let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path.clone());
                let probe_target = path.clone();
                let duration = cx
                    .background_executor()
                    .spawn(async move { ffmpeg.get_duration(&probe_target) })
                    .await;
                let probe_key = path.clone();
                let _ = this.update(cx, |this, cx| {
                    // 用路径而不是下标回写：探测期间用户可能删过队列条目，
                    // 下标会整体前移，按下标写会张冠李戴。
                    if let Some(idx) = this.state.queue_index_of(&probe_key) {
                        this.state.batch_queue[idx].duration = duration;
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 从队列里移除一条（用户手动剔除不想要的片源）
    pub(crate) fn remove_queue_item(&mut self, idx: usize, cx: &mut Context<Self>) {
        if self.state.remove_queue_item(idx) {
            cx.notify();
        }
    }

    /// 清空队列并退出批量模式（已转写完成、已落库的工程不受影响）
    pub(crate) fn clear_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        }
        self.state.clear_batch_queue();
        cx.notify();
    }

    /// 删除一条视频库记录，并把「当前工程被删掉了」这件事显式告诉用户。
    ///
    /// `AppState::delete_task_record` 在删掉的恰好是当前打开的工程时，会**静默**把
    /// 工作区换成列表里的下一条记录。用户回到剪辑台会发现字幕变成了另一个视频的，
    /// 却没有任何线索说明发生了什么。这里在删除后检查当前工程是否真的被换掉，
    /// 是则发一条**中性提示**（不是错误——删除本身成功了）说明切换到了哪一条。
    pub(crate) fn delete_task_record(&mut self, id: i64, cx: &mut Context<Self>) {
        let was_current = self
            .state
            .recent_tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| {
                self.state.selected_file.as_ref().is_some_and(|p| {
                    p.to_string_lossy().replace('\\', "/") == t.file_path.replace('\\', "/")
                })
            })
            .unwrap_or(false);

        self.state.delete_task_record(id);

        self.notice = Some(if was_current {
            match self
                .state
                .selected_file
                .as_ref()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            {
                Some(name) => format!("已删除当前工程，工作区已切换到「{name}」"),
                None => "已删除当前工程，工作区已清空".to_string(),
            }
        } else {
            "已删除该记录".to_string()
        });

        if self.state.selected_file.is_some() {
            self.trigger_extract_frame(cx);
        }
        cx.notify();
    }

    /// 「开始全部」：从第一个待处理项起，连续转写直到队列跑完。
    ///
    /// 失败项会被重新排队——用户点「开始全部」的语义就是「把没成功的再跑一遍」，
    /// 已经成功的条目则跳过，不重复烧算力。
    pub(crate) fn start_batch_queue(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        if self.state.batch_queue.is_empty() {
            self.state.status = ProcessStatus::Failed("批量队列是空的，先导入文件".to_string());
            cx.notify();
            return;
        }
        for item in self.state.batch_queue.iter_mut() {
            if item.state.is_failed() {
                item.state = QueueState::Pending;
            }
        }
        let Some(next) = self.state.queue_next_actionable() else {
            self.state.status = ProcessStatus::Failed(
                "队列中的文件都已转写完成，无需重复处理".to_string(),
            );
            cx.notify();
            return;
        };
        self.state.batch_running = true;
        let path = self.state.batch_queue[next].path.clone();
        self.start_processing_for(path, cx);
    }

    /// 终止批量队列：停掉当前任务并停止续跑（已完成的条目保留结果）
    pub(crate) fn cancel_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            self.request_cancel_processing(cx);
        } else {
            cx.notify();
        }
    }

    /// 取出队列里下一个待处理项并立即开跑；返回 `true` 表示已续上。
    ///
    /// 队列跑完时在这里收尾：复位批量模式并弹出批量汇总弹窗。
    fn advance_batch_queue(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.state.batch_running {
            return false;
        }
        match self.state.queue_next_actionable() {
            Some(next) => {
                let path = self.state.batch_queue[next].path.clone();
                self.start_processing_for(path, cx);
                true
            }
            None => {
                self.finish_batch_queue(cx);
                false
            }
        }
    }

    /// 批量队列收尾：统计结果并弹出汇总弹窗。
    ///
    /// 队列跑完后工作台停在上一条完成的工程上（`Finished` 分支已同步过），
    /// 这里只负责把「这一批到底成了几个、败了几个」讲清楚。
    fn finish_batch_queue(&mut self, cx: &mut Context<Self>) {
        self.state.batch_running = false;
        self.state.batch_active = None;
        let (done, failed, segments) = self.state.queue_summary();
        let total = self.state.batch_queue.len();
        let total_duration = self.state.queue_total_duration();
        let failed_names = self.state.queue_failed_names();
        info!("批量转写结束：{done} 成功 / {failed} 失败 / 共 {total} 个文件");
        self.state.status = ProcessStatus::Idle;
        // 工作台停在最后一条完成的工程上，波形在整个队列跑完后再抽一次即可
        self.ensure_waveform(cx);
        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: format!("本次共处理 {total} 个文件"),
            segment_count: segments,
            total_duration,
            metrics: None,
            batch: Some(BatchSummary {
                total,
                done,
                failed,
                segments,
                total_duration,
                failed_names,
            }),
        });
        cx.notify();
    }

    /// 智能缓存命中处理：直接 0 秒载入已缓存的解析结果
    pub(crate) fn load_cached_result(&mut self, cx: &mut Context<Self>, cached: crate::storage::TaskRecord) {
        let filename = cached.file_name.clone();
        let seg_count = cached.segments.len();
        let total_dur = cached.duration;
        let metrics = cached.metrics.clone();

        info!("智能缓存命中: 0 秒载入 {:?}", filename);
        self.state.load_from_cache(cached);
        self.state.active_tab = crate::app::state::WorkspaceTab::Editor;
        self.ensure_waveform(cx);

        self.completion_dialog = Some(CompletionDialogInfo {
            file_name: filename,
            segment_count: seg_count,
            total_duration: total_dur,
            metrics,
            batch: None,
        });
        cx.notify();
    }

    /// 触发流水线处理（单文件入口：跑完弹单文件完成框）
    pub(crate) fn start_processing(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.state.transcribe_file.clone() else {
            return;
        };
        // 单文件入口显式退出批量续跑语义：否则这一条跑完会把整个队列也顺手带跑，
        // 用户点「开始转写」时并没有这个预期。
        self.state.batch_running = false;
        self.state.batch_active = None;
        self.start_processing_for(file, cx);
    }

    /// 启动一次转写，显式指定输入文件。
    ///
    /// 单文件入口与批量续跑共用这一条启动路径：差别只在「跑完之后做什么」，
    /// 启动前的准备（停预览、拼运行时参数、派发后台线程）完全一致。
    pub(crate) fn start_processing_for(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        if matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        // Preview decoding is CPU-heavy on a CPU-only machine. Stop it before
        // Whisper starts so the ASR workers get the available cores and memory
        // bandwidth instead of competing with 25 fps BGRA conversion.
        if self.state.is_playing {
            self.state.current_time = self.state.video_player.current_play_time();
        }
        self.state.video_player.stop();
        self.state.is_playing = false;
        self.play_tick_generation = self.state.video_player.generation();

        // 批量模式：把这一条标记为「转写中」，并把入队时探测好的时长喂给进度条。
        // 时长必须在这里回填——管线只上报进度，不上报总时长。
        if self.state.batch_running {
            if let Some(idx) = self.state.queue_index_of(&file) {
                self.state.batch_active = Some(idx);
                self.state.mark_queue_running(idx);
                let probed = self.state.batch_queue[idx].duration;
                if probed > 0.0 {
                    self.state.transcribe_duration = probed;
                }
            }
        }

        let input_file = file;
        self.state.transcribe_file = Some(input_file.clone());

        let (tx, mut rx) = mpsc::unbounded_channel();
        self.state.clear_streaming();
        self.state.cancel_requested = false;
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
        let rescue_logprob = self.state.config.pipeline.whisper_rescue_logprob;
        // 音频前端预处理：把配置里的开关映射成管线参数。默认全开——
        // 它改的是「喂给模型的音频量」，是唯一还有量级空间的提速方向。
        let preprocess = crate::core::pipeline::PreprocessOptions {
            enabled: self.state.config.pipeline.preprocess_enabled,
            filters: crate::engines::SpeechFilterOptions {
                highpass_hz: self.state.config.pipeline.preprocess_highpass_hz,
                denoise: self.state.config.pipeline.preprocess_denoise,
                normalize: self.state.config.pipeline.preprocess_normalize,
                target_rate: crate::engines::ASR_SAMPLE_RATE,
            },
            compaction: crate::engines::CompactionConfig::default(),
            compact: self.state.config.pipeline.preprocess_compact,
            min_saving: self.state.config.pipeline.preprocess_min_saving,
        };
        let whisper_options = crate::core::pipeline::WhisperRuntimeOptions {
            audio_speed: self.state.config.pipeline.whisper_audio_speed,
            vad_threshold: self.state.config.pipeline.whisper_vad_threshold,
            preprocess,
        };
        // 说话人分离（F-015）：开关与期望人数都取自配置，关时管线完全跳过该阶段
        let diarization = crate::core::pipeline::DiarizationOptions {
            enabled: self.state.config.pipeline.enable_diarization,
            speakers: self.state.config.pipeline.speaker_count,
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
                    .run_with_options(in_file, None, lang, fmt, polish, polish_mode, threads, model_override, rescue_logprob, whisper_options, diarization, tx.clone())
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
                                PipelineEvent::SegmentStream(seg) => {
                                    this.state.push_stream_segment(seg);
                                }
                                PipelineEvent::Finished(segments, metrics) => {
                                    this.state.clear_streaming();
                                    if this.state.cancel_requested {
                                        // 用户主动终止：静默收尾，不弹完成框、不写历史库
                                        this.state.cancel_requested = false;
                                        this.state.status = ProcessStatus::Idle;
                                        this.state.transcribe_file = None;
                                        this.state.transcribe_duration = 0.0;
                                        // 批量模式下终止：当前项记为取消，并停掉续跑。
                                        // 已经跑完的条目保留结果，不清零。
                                        this.state.finish_active_queue_item(Err("已取消".to_string()));
                                        this.state.batch_running = false;
                                    } else if segments.is_empty() {
                                        this.state.status = ProcessStatus::Failed("未能识别出任何有效字幕，请检查音频音量或识别语言设置".to_string());
                                        this.state.finish_active_queue_item(Err("未识别出有效字幕".to_string()));
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
                                        // 用 batch_running 而不是 batch_active 判断：用户删掉队列里
                                        // 正在跑的条目时 batch_active 会被清空，但这一批仍在续跑。
                                        // 若按 batch_active 判断，就会在批量中途弹出单文件完成框
                                        // 并逐条重抽波形。
                                        let in_batch = this.state.batch_running;

                                        // 1. 存入数据库历史记录 (包含 5 阶段性能统计指标)
                                        let mut new_task_id: Option<i64> = None;
                                        if let Some(ref path) = file_path {
                                            new_task_id = this
                                                .state
                                                .db
                                                .insert_task(
                                                    &path.to_string_lossy(),
                                                    &filename,
                                                    total_dur,
                                                    "completed",
                                                    &segments,
                                                    Some(&metrics),
                                                )
                                                .ok();
                                            this.state.refresh_recent_tasks();
                                        }

                                        // 2. 将本次成果同步至剪辑校对工作台。
                                        // 覆盖前先把上一个工程的未保存改动落库：批量连跑时
                                        // 用户可能在上一支片子上改过字幕，不能默默丢掉。
                                        // 注意顺序：这一步必须用**旧**的 active_task_id 写回，
                                        // 所以新 id 要等 segments 换完之后才登记。
                                        this.state.flush_segments_if_dirty();
                                        if let Some(path) = file_path {
                                            this.state.selected_file = Some(path.clone());
                                            this.state.preview_source = Some(path);
                                        }
                                        this.state.segments = segments;
                                        this.state.active_task_id = new_task_id;
                                        // 新一轮转写的结果替换了整份文档：撤销栈里还是上一支
                                        // 片子的快照，留着的话 Ctrl+Z 会把上一支的字幕灌进这一支，
                                        // 并按新的 active_task_id 落库覆盖刚写好的记录。
                                        this.state.reset_edit_history();
                                        this.state.bump_segments_revision();
                                        if let Some(first) = this.state.segments.first() {
                                            this.state.select_segment(first.index);
                                        }
                                        this.state.total_duration = total_dur;

                                        // 3. 语音转写界面彻底解耦复位（等待下一次导入）
                                        this.state.status = ProcessStatus::Idle;
                                        this.state.transcribe_file = None;
                                        this.state.transcribe_duration = 0.0;

                                        if in_batch {
                                            // 批量模式：只结算这一条。波形提取与完成弹窗都留到
                                            // 队列跑完再一次性处理，否则每完成一条都要重抽一次
                                            // 波形，纯属白烧解码。
                                            this.state.finish_active_queue_item(Ok(seg_count));
                                        } else {
                                            this.ensure_waveform(cx);

                                            // 4. 弹出全屏转写完成提醒弹窗 (包含性能基准看板)
                                            this.completion_dialog = Some(CompletionDialogInfo {
                                                file_name: filename,
                                                segment_count: seg_count,
                                                total_duration: total_dur,
                                                metrics: Some(metrics),
                                                batch: None,
                                            });
                                        }
                                    }
                                    // 批量续跑：队列没跑完就立刻取下一条，跑完则弹批量汇总
                                    if this.state.batch_running {
                                        this.advance_batch_queue(cx);
                                    }
                                }
                                PipelineEvent::Error(err) => {
                                    if this.state.cancel_requested {
                                        // 终止导致的子进程报错按取消处理，不显示为失败
                                        this.state.cancel_requested = false;
                                        this.state.status = ProcessStatus::Idle;
                                        this.state.transcribe_file = None;
                                        this.state.transcribe_duration = 0.0;
                                        this.state.finish_active_queue_item(Err("已取消".to_string()));
                                        this.state.batch_running = false;
                                    } else if this.state.batch_running {
                                        // 批量模式：单个文件出错不该拖垮整批。失败原因记在队列卡片上，
                                        // 立刻续跑下一个，最后在汇总弹窗里一并点名。
                                        // 这里同样按 batch_running 判断，理由见 Finished 分支。
                                        this.state.transcribe_file = None;
                                        this.state.transcribe_duration = 0.0;
                                        this.state.finish_active_queue_item(Err(err));
                                    } else {
                                        this.state.status = ProcessStatus::Failed(err);
                                    }
                                    if this.state.batch_running {
                                        this.advance_batch_queue(cx);
                                    }
                                }
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 通用导出流程：编辑落库 -> 弹系统保存对话框 -> 后台写出文件 -> 状态回写。
    /// 统一四个导出入口的重复骨架（对话框参数与写出函数因格式而异）。
    pub(crate) fn export_with_save_dialog(
        &mut self,
        cx: &mut Context<Self>,
        default_name: String,
        filter_label: &str,
        filter_ext: String,
        write_file: impl FnOnce(&[crate::subtitle::Segment], &std::path::Path) -> anyhow::Result<()> + Send + 'static,
    ) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        let filter_label = filter_label.to_string();
        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_file_name(&default_name)
                .add_filter(&filter_label, &[filter_ext.as_str()])
                .save_file()
                .await;
            if let Some(save_handle) = handle {
                let save_path = save_handle.path().to_path_buf();
                let res = write_file(&segments, &save_path);
                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(()) => {
                            info!("字幕成功导出至: {:?}", save_path);
                        }
                        Err(e) => {
                            this.state.status = ProcessStatus::Failed(format!("导出失败: {}", e));
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// 剪映专业版草稿一键直接注入（自动写入本机剪映草稿库，打开剪映即可见）
    pub(crate) fn export_jianying_local(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        self.state.flush_segments_if_dirty();

        let segments = self.state.segments.clone();
        let video_path = self.state.selected_file.clone();
        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();

        cx.spawn(async move |this, cx| {
            let res = cx.background_executor().spawn(async move {
                crate::subtitle::JianYingExporter::inject_to_local_jianying(
                    &segments,
                    video_path.as_deref(),
                    &stem,
                )
            }).await;

            let _ = this.update(cx, |this, cx| {
                match res {
                    Ok(draft_path) => {
                        info!("成功注入剪映草稿: {:?}", draft_path);
                        let _ = std::process::Command::new("explorer").arg(&draft_path).spawn();
                        this.state.status = ProcessStatus::Idle;
                    }
                    Err(e) => {
                        tracing::error!("剪映草稿注入失败: {:?}", e);
                        this.state.status = ProcessStatus::Failed(format!("剪映草稿注入失败: {}", e));
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    /// 导出剪映草稿至指定独立文件夹
    pub(crate) fn export_jianying_folder(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }

        let segments = self.state.segments.clone();
        let video_path = self.state.selected_file.clone();
        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("Voice2Word")
            .to_string();

        cx.spawn(async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_title("选择剪映草稿保存目录")
                .pick_folder()
                .await;
            if let Some(folder_handle) = handle {
                let target_dir = folder_handle.path().join(&stem);
                let res = cx.background_executor().spawn(async move {
                    crate::subtitle::JianYingExporter::export_to_folder(
                        &segments,
                        video_path.as_deref(),
                        &target_dir,
                        &stem,
                    )
                }).await;

                let _ = this.update(cx, |this, cx| {
                    match res {
                        Ok(p) => {
                            info!("剪映草稿文件夹导出成功: {:?}", p);
                            let _ = std::process::Command::new("explorer").arg(&p).spawn();
                        }
                        Err(e) => {
                            this.state.status = ProcessStatus::Failed(format!("剪映草稿导出失败: {}", e));
                        }
                    }
                    cx.notify();
                });
            }
        }).detach();
    }

    /// 导出 FCPXML (Final Cut Pro / 达芬奇) 工程文件
    pub(crate) fn export_fcpxml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        self.export_with_save_dialog(
            cx,
            format!("{}.fcpxml", stem),
            "Final Cut Pro XML (*.fcpxml)",
            "fcpxml".to_string(),
            move |segs, path| crate::subtitle::FcpXmlExporter::write_to_file(segs, path, &stem),
        );
    }

    /// 导出 Adobe Premiere Pro XML (FCP7 XML / xmeml) 工程文件
    pub(crate) fn export_premiere_xml(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() {
            return;
        }
        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        self.export_with_save_dialog(
            cx,
            format!("{}.xml", stem),
            "Premiere Pro XML (*.xml)",
            "xml".to_string(),
            move |segs, path| crate::subtitle::PremiereXmlExporter::write_to_file(segs, path, &stem),
        );
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
        self.state.flush_segments_if_dirty();

        let subtitle_path = std::env::temp_dir().join(format!(
            "voice2word_preview_{}.srt",
            std::process::id()
        ));
        // 复用同一临时文件名，写新前先清旧，避免历次预览 SRT 在 temp 无限累积
        let _ = std::fs::remove_file(&subtitle_path);
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
        // 预览播放器是「发起后没人 wait」的短命进程：句柄一丢就成孤儿进程，
        // 而 `-autoexit` 只在「正常播完」时生效——用户在播放中关窗、或反复点预览，
        // 都会在系统里留下没人回收的 ffplay.exe（占着视频文件句柄与音频设备）。
        // 因此：先收掉上一次预览（避免堆积），再把新进程登记到全局表，
        // 由 main.rs 在应用退出时统一 kill + wait。
        crate::utils::child_registry::retire_all();
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
            Ok(child) => {
                // 先并入全局作业对象，再登记：这样「本进程被强杀」时 ffplay 也会
                // 被系统一并清掉，而不仅是靠退出路径里的 retire_all。
                crate::utils::child_registry::adopt(&child);
                crate::utils::child_registry::register(child);
                info!("已启动带字幕视频预览");
            }
            Err(error) => {
                self.state.status = ProcessStatus::Failed(format!("启动视频预览失败: {}", error));
                cx.notify();
            }
        }
    }

    /// 请求终止当前转写任务：标记取消并强杀识别子进程。
    ///
    /// 状态先切到「终止中」给用户即时反馈，真正的复位由管线的收尾事件统一完成
    /// （见 `start_processing` 里 `cancel_requested` 的分支），避免两处各写一套收尾。
    /// 未在转写时调用是安全的空操作。
    pub(crate) fn request_cancel_processing(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state.status, ProcessStatus::Processing { .. }) {
            return;
        }
        self.state.cancel_requested = true;
        self.state.status = ProcessStatus::Processing {
            stage: "终止中".to_string(),
            progress: 1.0,
            detail: "正在终止识别进程，请稍候...".to_string(),
        };
        self.state.pipeline.cancel();
        cx.notify();
    }

    /// 异步探测当前视频的真实分辨率（驱动监视器画面等比适配，杜绝字幕掉进黑边、画面拉伸变形）
    pub(crate) fn probe_video_dimensions(&mut self, cx: &mut Context<Self>) {
        let Some(video) = self.state.preview_media().cloned() else {
            return;
        };
        if self.state.dims_probed_for.as_ref() == Some(&video) {
            return;
        }
        self.state.dims_probed_for = Some(video.clone());
        let ffmpeg_path = crate::utils::AppConfig::resolve_path(&self.state.config.paths.ffmpeg);
        let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ffmpeg.get_resolution(&video) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some((w, h)) = result {
                    this.state.video_width = w;
                    this.state.video_height = h;
                    this.state.video_player.set_frame_dimensions(w, h);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 异步提取当前工程的音频波形包络，驱动时间轴波形轨。
    ///
    /// 与 `probe_video_dimensions` 同样是「按媒体路径去重」的惰性任务：
    /// 来源一致直接复用已有包络；来源变了先清空再重算，避免时间轴在新波形
    /// 到达前画出上一支视频的包络（时间刻度完全对不上）。
    pub(crate) fn ensure_waveform(&mut self, cx: &mut Context<Self>) {
        let Some(media) = self.state.preview_media().cloned() else {
            return;
        };
        if !media.is_file() {
            return;
        }
        if self.state.waveform_ready_for(&media) {
            return;
        }
        if self.state.waveform_busy_for.as_ref() == Some(&media) {
            return;
        }

        self.state.waveform = None;
        self.state.waveform_busy_for = Some(media.clone());

        let ffmpeg = crate::engines::FFmpegEngine::new(crate::utils::AppConfig::resolve_path(
            &self.state.config.paths.ffmpeg,
        ));
        let buckets = crate::engines::waveform::MAX_WAVEFORM_BUCKETS;
        let extract_target = media.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { crate::engines::waveform::extract(&ffmpeg, &extract_target, buckets) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.state.waveform_busy_for.as_ref() == Some(&media) {
                    this.state.waveform_busy_for = None;
                }
                match result {
                    Ok(data) => {
                        info!(buckets = data.peaks.len(), "时间轴波形包络提取完成");
                        this.state.waveform = Some(std::sync::Arc::new(data));
                    }
                    // 波形只是辅助视觉，失败就静默不显示，绝不能因此打断转写主流程
                    Err(error) => {
                        tracing::warn!(error = %error, "波形提取失败，时间轴将不显示波形轨");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 异步根据当前播放时间抽取单帧画面（单飞队列：最多 1 个 FFmpeg 实例并发，合并多余拖动请求）
    pub(crate) fn trigger_extract_frame(&mut self, cx: &mut Context<Self>) {
        self.probe_video_dimensions(cx);
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
        self.state.flush_segments_if_dirty();
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
            self.state.flush_segments_if_dirty();
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

    /// 根据当前剪辑工作台选中的格式执行统一导出
    pub(crate) fn perform_editor_export(&mut self, cx: &mut Context<Self>) {
        self.state.flush_segments_if_dirty();
        if self.state.segments.is_empty() {
            return;
        }

        let stem = self.state.selected_file.as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("subtitle")
            .to_string();

        match self.editor_export_format {
            EditorExportFormat::JianYing => self.export_jianying_local(cx),
            EditorExportFormat::JianYingFolder => self.export_jianying_folder(cx),
            EditorExportFormat::Fcpxml => self.export_fcpxml(cx),
            EditorExportFormat::PremiereXml => self.export_premiere_xml(cx),
            EditorExportFormat::Srt | EditorExportFormat::Ass | EditorExportFormat::Txt | EditorExportFormat::Vtt => {
                let ext = match self.editor_export_format {
                    EditorExportFormat::Srt => "srt",
                    EditorExportFormat::Ass => "ass",
                    EditorExportFormat::Txt => "txt",
                    EditorExportFormat::Vtt => "vtt",
                    _ => unreachable!(),
                };
                // 直接按目标扩展名导出，不再临时改写全局 output_format（避免副作用泄漏到后续管线调用）
                // ASS 会带上主界面配置的字幕样式（字号/字间距/底边距/预设配色）
                let style = self.state.config.subtitle_style.clone();
                self.export_with_save_dialog(
                    cx,
                    format!("{}.{}", stem, ext),
                    "Subtitle",
                    ext.to_string(),
                    move |segs, path| {
                        let mode = if segs.iter().any(|s| s.translation.is_some()) {
                            crate::subtitle::ExportMode::Bilingual
                        } else {
                            crate::subtitle::ExportMode::RawOnly
                        };
                        SubtitleWriter::write_to_file_with_style(segs, path, ext, mode, &style)
                    },
                );
            }
        }
    }

    /// 触发大模型多语言字幕后台流式翻译。
    ///
    /// 引擎按 `state.translate_mode` 二选一：本地 Qwen（llama.cpp）或在线
    /// OpenAI 兼容接口；两条链路的进度回调协议一致，UI 侧无需分支。
    pub(crate) fn trigger_llm_translation(&mut self, cx: &mut Context<Self>) {
        if self.state.segments.is_empty() || self.state.is_translating {
            return;
        }
        if self.state.translated_count() == self.state.segments.len() {
            // 全部已有译文时重复触发只是白白烧额度/耗时，直接提示
            self.state.translate_status_msg =
                format!("{} 句已全部翻译完成", self.state.segments.len());
            cx.notify();
            return;
        }

        let mode = self.state.translate_mode;
        self.state.is_translating = true;
        self.state.translate_progress = 0.0;
        self.state.translate_status_msg = match mode {
            crate::engines::TranslateMode::OnlineApi => {
                format!("正在初始化在线翻译引擎 ({})...", self.state.config.translate.api_model)
            }
            crate::engines::TranslateMode::OfflineQwen => {
                "正在初始化 Qwen 翻译引擎...".to_string()
            }
        };
        cx.notify();

        let segments = self.state.segments.clone();
        let target_lang = self.state.translate_target_lang.clone();
        let online_cfg = self.state.online_translate_config();
        let llama_cli = crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llama_cli);
        let llm_model = crate::utils::config::AppConfig::resolve_path(&self.state.config.paths.llm_model);
        let llm_ctx = self.state.config.pipeline.llm_ctx;
        let llm_threads = self.state.config.pipeline.llm_threads;
        // 取消标志：任务起手先复位（上一轮取消后它会停在 true），再交给引擎
        let cancel_flag = self.translate_cancel.clone();
        cancel_flag.store(false, std::sync::atomic::Ordering::SeqCst);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(f64, String)>();

        cx.spawn(async move |this, cx| {
            let handle = cx.background_executor().spawn(async move {
                let translate_engine = match mode {
                    crate::engines::TranslateMode::OnlineApi => {
                        crate::engines::TranslateEngine::online(online_cfg)
                    }
                    crate::engines::TranslateMode::OfflineQwen => {
                        let llm_engine = crate::engines::LLMEngine::new(
                            llama_cli,
                            llm_model,
                            llm_ctx,
                            llm_threads,
                        );
                        crate::engines::TranslateEngine::offline(llm_engine)
                    }
                };
                let tx_clone = tx.clone();
                translate_engine.translate_subtitles(
                    segments,
                    &target_lang,
                    Some(Box::new(move |p, msg| {
                        let _ = tx_clone.send((p, msg.to_string()));
                    })),
                    cancel_flag,
                )
            });

            let progress_this = this.clone();
            cx.spawn(async move |cx| {
                while let Some((progress, msg)) = rx.recv().await {
                    let _ = progress_this.update(cx, |this, cx| {
                        this.state.translate_progress = progress as f32;
                        this.state.translate_status_msg = msg;
                        cx.notify();
                    });
                }
            }).detach();

            let result = handle.await;
            // 取消位要在 update 闭包里读、在闭包里复位——中间不能被别的任务改掉
            let was_cancelled = this
                .update(cx, |this, cx| {
                    let c = this
                        .translate_cancel
                        .load(std::sync::atomic::Ordering::SeqCst);
                    this.state.is_translating = false;
                    this.translate_cancel
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    cx.notify();
                    c
                })
                .unwrap_or(false);
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(translated_segs) => {
                        let done = translated_segs
                            .iter()
                            .filter(|s| s.translation.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false))
                            .count();
                        // 引擎在取消时返回**已完成的偏序结果**而非错误，所以这里必须
                        // 显式区分「跑完了」与「被取消了」。不区分的话，用户点了取消
                        // 却看到「翻译已完成（N 句）」，会以为取消没生效。
                        if was_cancelled {
                            info!("字幕多语言翻译已取消（已完成 {} 句）", done);
                            this.state.translate_status_msg =
                                format!("已取消翻译（保留已完成的 {} 句）", done);
                        } else {
                            info!("字幕多语言翻译成功完成 ({} 句带译文)", done);
                            this.state.translate_progress = 1.0;
                            this.state.translate_status_msg = format!("翻译已完成（{} 句）", done);
                        }
                        // 无论完成还是取消，已产出的译文都要并入当前字幕表
                        this.state.segments = translated_segs;
                        this.state.bump_segments_revision();
                        // 译文必须落库，否则重启后历史库里的双语对照会凭空消失
                        this.state.segments_dirty = true;
                        this.state.flush_segments_if_dirty();
                    }
                    Err(e) => {
                        tracing::warn!("字幕多语言翻译过程报错: {}", e);
                        this.state.translate_progress = 0.0;
                        this.state.translate_status_msg = format!("翻译失败: {}", e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 取消正在进行的字幕翻译。
    ///
    /// 只置位取消标志：引擎在**每个批次之间**检查（离线 Qwen 与在线 API 两条链路
    /// 都接了），因此最多损失当前一批，已翻译的句子原样保留。任务收尾时统一复位
    /// （见 `trigger_llm_translation` 的 `handle.await` 之后）。
    pub(crate) fn cancel_llm_translation(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_translating {
            return;
        }
        self.translate_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.state.translate_status_msg = "正在取消翻译，等待当前批次结束…".to_string();
        cx.notify();
    }

    /// 在线翻译接口连通性自检（设置页「测试连接」按钮）
    pub(crate) fn probe_online_translate_api(&mut self, cx: &mut Context<Self>) {
        if self.is_probing_translate {
            return;
        }
        if self.state.config.translate.api_model.trim().is_empty() {
            self.translate_probe_msg = Some((false, "请先填写模型名".to_string()));
            cx.notify();
            return;
        }
        if self.state.config.translate.effective_api_key().trim().is_empty() {
            self.translate_probe_msg =
                Some((false, "请先填写 API Key（或设置环境变量 VOICE2WORD_API_KEY）".to_string()));
            cx.notify();
            return;
        }

        self.is_probing_translate = true;
        self.translate_probe_msg = Some((true, "正在连接在线接口...".to_string()));
        cx.notify();

        let cfg = self.state.online_translate_config();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    crate::engines::TranslateEngine::online(cfg).probe_online()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.is_probing_translate = false;
                this.translate_probe_msg = Some(match result {
                    Ok(reply) => (true, format!("连接成功，模型回显: {}", reply.trim())),
                    Err(e) => (false, format!("连接失败: {e}")),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
