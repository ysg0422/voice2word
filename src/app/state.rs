//! 应用程序全局状态

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::TaskPipeline;
use crate::storage::{Database, TaskRecord};
use crate::subtitle::Segment;
use crate::utils::{AppConfig, FrameCache};
use crate::engines::{HardwareProfile, ProxyManager};

#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    Idle,
    Processing { stage: String, progress: f64, detail: String },
    Completed,
    Failed(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceMetrics {
    pub sys_cpu: f32,          // 系统总 CPU (0.0 ~ 100.0)
    pub sys_mem_used: u64,     // 系统已用内存 (bytes)
    pub sys_mem_total: u64,    // 系统总内存 (bytes)

    pub proc_name: String,     // 进程标识 (如 "Qwen2.5 LLM" 或 "Voice2Word")
    pub proc_cpu: f32,         // 进程 CPU (0.0 ~ 100.0)
    pub proc_mem: u64,         // 进程占用内存 (bytes)
    pub is_model_running: bool,// 是否有模型正在工作
}

impl ResourceMetrics {
    /// 格式化字节数，如 1.85 GB 或 420.5 MB
    pub fn format_bytes(bytes: u64) -> String {
        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if gb >= 1.0 {
            format!("{:.2} GB", gb)
        } else {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            format!("{:.1} MB", mb)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceTab {
    Editor,       // 剪辑校对工作台 (主界面，默认)
    Generate,     // 智能转写生成 (轻量化无卡顿进度)
    Library,      // 历史解析视频库 (视频资产库)
    Performance,  // 性能与推理设置 (硬件检测、性能评估、AI 推理决策)
}

/// 模型档位：控制 Whisper 模型大小与量化精度，平衡速度与抗口音能力
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhisperModelTier {
    Fast,        // 极速 Base — ggml-base.bin (39M 参数)
    Balanced,    // 均衡 Small — ggml-small.bin (244M 参数)
    TurboSpeed,  // 极速 Turbo — ggml-large-v3-turbo-q5_0.bin (Q5 破带宽版，提速 25%~30%)
    #[default]
    Precise,     // 高精 Turbo — ggml-large-v3-turbo-q8_0.bin (Q8 旗舰版，抗口音吞音)
}

/// 润色模式：CT-Punc 极速标点恢复 vs Qwen 大模型深度润色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolishMode {
    #[default]
    PuncFast,  // 极速标点 (CT-Punc · 仅数秒)
    QwenDeep,  // 深度润色 (Qwen · 较慢)
}

impl PolishMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolishMode::PuncFast => "punc",
            PolishMode::QwenDeep => "qwen",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "qwen" | "llm" => PolishMode::QwenDeep,
            _ => PolishMode::PuncFast,
        }
    }
}

impl WhisperModelTier {

    /// 返回对应的模型文件名（相对于 models/whisper/ 目录）
    pub fn model_filename(self) -> &'static str {
        match self {
            Self::Fast       => "ggml-base.bin",
            Self::Balanced   => "ggml-small.bin",
            Self::TurboSpeed => "ggml-large-v3-turbo-q5_0.bin",
            Self::Precise    => "ggml-large-v3-turbo-q8_0.bin",
        }
    }

    /// 返回相对路径（如果 TurboSpeed 本地未下载完成，自动平滑回退至 Q8）
    pub fn model_relative_path(self) -> String {
        let preferred = format!("models/whisper/{}", self.model_filename());
        if self == Self::TurboSpeed && !crate::utils::AppConfig::resolve_path(&preferred).exists() {
            "models/whisper/ggml-large-v3-turbo-q8_0.bin".to_string()
        } else {
            preferred
        }
    }

    /// 8 线程 CPU 下，相对视频时长的转写耗时系数（校准自 32 分钟样片）。
    fn cpu_realtime_factor(self) -> f64 {
        match self {
            Self::Fast       => 1.5 / 32.0,
            Self::Balanced   => 4.5 / 32.0,
            Self::TurboSpeed => 5.6 / 32.0, // Q5 降低内存带宽传输，提速约 30%
            Self::Precise    => 8.0 / 32.0,
        }
    }

    /// Whisper.cpp 线程加速有收益递减：4→8 约 1.45×，8→16 约 1.39×。
    pub fn thread_time_factor(threads: u32) -> f64 {
        let t = threads.max(1) as f64;
        (8.0 / t).powf(0.45).clamp(0.55, 2.2)
    }

    /// 预估转写秒数。GPU 上线程收益更弱；润色额外加固定开销。
    pub fn estimate_seconds(
        self,
        duration_sec: f64,
        threads: u32,
        use_gpu: bool,
        enable_polish: bool,
    ) -> f64 {
        let media = if duration_sec > 1.0 { duration_sec } else { 32.0 * 60.0 };
        let mut secs = media * self.cpu_realtime_factor();
        let thread_f = Self::thread_time_factor(threads);
        if use_gpu {
            secs *= 0.28 * (0.72 + 0.28 * thread_f);
        } else {
            secs *= thread_f;
        }
        if enable_polish {
            secs += (media / 60.0) * 2.4;
        }
        secs.max(8.0)
    }

    pub fn format_eta(seconds: f64) -> String {
        if seconds < 60.0 {
            format!("约 {:.0} 秒", seconds.max(5.0))
        } else if seconds < 3600.0 {
            let mins = seconds / 60.0;
            if mins < 10.0 {
                format!("约 {:.1} 分钟", mins)
            } else {
                format!("约 {:.0} 分钟", mins)
            }
        } else {
            format!("约 {:.1} 小时", seconds / 3600.0)
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub db: Database,
    pub pipeline: Arc<TaskPipeline>,

    pub selected_file: Option<PathBuf>,
    pub transcribe_file: Option<PathBuf>,
    pub transcribe_duration: f64,
    pub status: ProcessStatus,
    pub segments: Vec<Segment>,
    pub recent_tasks: Vec<TaskRecord>,

    // 实时流式转写数据与当前识别推进绝对时间 (秒)
    pub streaming_segments: Vec<Segment>,
    pub streaming_current_sec: f64,

    // 处理选项
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    pub polish_mode: PolishMode,
    pub whisper_threads: u32,
    pub whisper_model_tier: WhisperModelTier,

    // 硬件与模型资源监控
    pub metrics: ResourceMetrics,

    // 剪辑校对工作台状态 (主界面)
    pub active_tab: WorkspaceTab,
    pub current_time: f64,
    pub total_duration: f64,
    pub is_playing: bool,
    pub selected_segment_index: Option<usize>,
    pub preview_frame_path: Option<PathBuf>,
    pub timeline_zoom: f64,
    pub editing_text: String,

    // 视频帧缓存（优化拖动时间轴性能）
    pub frame_cache: Arc<FrameCache>,

    // 实时内嵌视频播放引擎
    pub video_player: Arc<crate::engines::VideoPlayerEngine>,
    /// Runtime-selected rendering/decode policy shared by preview and seek.
    pub hardware: HardwareProfile,
    pub proxy_manager: Arc<ProxyManager>,
    pub preview_source: Option<PathBuf>,
    /// User-overridable: CPU machines force proxy on by default, but it can be turned off.
    pub proxy_enabled: bool,
    pub proxy_busy: bool,

    // 性能检测与 AI 推理决策
    pub hardware_info: crate::core::HardwareInfo,
    pub performance_level: crate::core::PerformanceLevel,
    pub user_strategy: crate::core::UserStrategy,
    pub recommended_profile: crate::core::InferenceProfile,
    pub benchmark_result: Option<crate::core::BenchmarkResult>,
    pub is_benchmarking: bool,
}

impl AppState {
    pub fn new(config: AppConfig, db: Database, pipeline: Arc<TaskPipeline>) -> Self {
        Self::with_hardware(config, db, pipeline, HardwareProfile::detect())
    }

    pub fn with_hardware(
        config: AppConfig,
        db: Database,
        pipeline: Arc<TaskPipeline>,
        hardware: HardwareProfile,
    ) -> Self {
        let recent_tasks = db.list_recent_tasks(50).unwrap_or_default();
        let lang = config.pipeline.language.clone();
        let fmt = config.pipeline.output_format.clone();
        let polish = config.pipeline.enable_polish;
        let polish_mode = PolishMode::from_str(&config.pipeline.polish_mode);
        let threads = config.pipeline.whisper_threads.max(8);
        let ffmpeg_path = AppConfig::resolve_path(&config.paths.ffmpeg);
        let proxy_manager = Arc::new(ProxyManager::new(&ffmpeg_path));
        let proxy_enabled = hardware.force_proxy;
        let video_player = Arc::new(crate::engines::VideoPlayerEngine::with_policy(
            ffmpeg_path,
            hardware.decode_policy(),
            hardware.use_gpu_pipeline(),
        ));

        // 硬件检测与决策评估
        let hardware_info = crate::core::HardwareInfo::detect();
        let performance_level = hardware_info.evaluate_performance();
        let user_strategy = crate::core::UserStrategy::Balanced;
        let recommended_profile = crate::core::InferenceProfile::decide(
            performance_level,
            user_strategy,
            &hardware_info,
        );

        let mut state = Self {
            config,
            db,
            pipeline,
            selected_file: None,
            transcribe_file: None,
            transcribe_duration: 0.0,
            status: ProcessStatus::Idle,
            segments: Vec::new(),
            recent_tasks,
            streaming_segments: Vec::new(),
            streaming_current_sec: 0.0,
            language: lang,
            output_format: fmt,
            enable_polish: polish,
            polish_mode,
            whisper_threads: threads,
            whisper_model_tier: WhisperModelTier::Precise, // 默认精准档 (turbo)
            metrics: ResourceMetrics::default(),

            active_tab: WorkspaceTab::Editor, // 默认主界面为剪辑校对工作台
            current_time: 0.0,
            total_duration: 0.0,
            is_playing: false,
            selected_segment_index: None,
            preview_frame_path: None,
            timeline_zoom: 1.0,
            editing_text: String::new(),
            frame_cache: Arc::new(FrameCache::new(100)), // 缓存 100 帧（约占 5-10MB）
            video_player,
            proxy_manager,
            hardware,
            preview_source: None,
            proxy_enabled,
            proxy_busy: false,

            hardware_info,
            performance_level,
            user_strategy,
            recommended_profile,
            benchmark_result: None,
            is_benchmarking: false,
        };

        // 如果存在历史记录，启动时自动加载最近一次的工程，避免开屏黑屏或空数据
        if let Some(recent) = state.recent_tasks.first().cloned() {
            state.load_task(&recent);
        }

        state
    }

    /// 切换用户策略并重新计算推荐配置
    pub fn set_user_strategy(&mut self, strategy: crate::core::UserStrategy) {
        self.user_strategy = strategy;
        self.recommended_profile = crate::core::InferenceProfile::decide(
            self.performance_level,
            self.user_strategy,
            &self.hardware_info,
        );
    }

    /// 重新检测硬件并重新评估
    pub fn refresh_hardware_detection(&mut self) {
        self.hardware_info = crate::core::HardwareInfo::detect();
        self.performance_level = self.hardware_info.evaluate_performance();
        self.recommended_profile = crate::core::InferenceProfile::decide(
            self.performance_level,
            self.user_strategy,
            &self.hardware_info,
        );
    }

    /// 将推荐配置应用至当前系统状态与 Pipeline 配置
    pub fn apply_recommended_profile(&mut self) {
        self.whisper_model_tier = self.recommended_profile.whisper_tier;
        self.whisper_threads = self.recommended_profile.whisper_threads;
        self.config.pipeline.whisper_threads = self.recommended_profile.whisper_threads;
        self.config.pipeline.enable_vad = self.recommended_profile.enable_vad;
        self.config.pipeline.llm_threads = self.recommended_profile.llm_threads;
        self.config.pipeline.whisper_processors = self.recommended_profile.max_concurrency;

        let rel_path = self.recommended_profile.whisper_tier.model_relative_path();
        if AppConfig::resolve_path(&rel_path).exists() {
            self.config.paths.whisper_model = rel_path;
        }

        let _ = self.config.save_to_file("config.toml");
    }

    pub fn set_selected_file(&mut self, path: PathBuf) {
        self.selected_file = Some(path);
    }

    pub fn refresh_recent_tasks(&mut self) {
        if let Ok(tasks) = self.db.list_recent_tasks(50) {
            self.recent_tasks = tasks;
        }
    }

    /// 载入历史任务并无缝切换至剪辑工作台
    pub fn load_task(&mut self, task: &TaskRecord) {
        self.selected_file = Some(PathBuf::from(&task.file_path));
        self.status = ProcessStatus::Idle;
        self.segments = task.segments.clone();
        let dur = if task.duration > 0.0 {
            task.duration
        } else {
            task.segments.last().map(|s| s.end).unwrap_or(0.0)
        };
        self.total_duration = dur;
        if let Some(first) = self.segments.first() {
            self.select_segment(first.index);
        } else {
            self.current_time = 0.0;
            self.selected_segment_index = None;
            self.editing_text.clear();
        }
        self.active_tab = WorkspaceTab::Editor;
        self.preview_source = self.selected_file.clone();
        self.proxy_busy = false;
        if self.proxy_enabled {
            if let Some(src) = self.selected_file.as_ref() {
                let h = self.proxy_manager.preview_height(src, !self.hardware.use_gpu_pipeline());
                if let Some(existing) = self.proxy_manager.existing_proxy(src, h) {
                    self.preview_source = Some(existing);
                }
            }
        }
    }

    /// 删除历史任务记录（若当前工作区正显示该工程，则无缝切换至下一个或彻底清空）
    pub fn delete_task_record(&mut self, id: i64) {
        let deleted_task = self.recent_tasks.iter().find(|t| t.id == id).cloned();
        let _ = self.db.delete_task(id);
        self.refresh_recent_tasks();

        if let Some(task) = deleted_task {
            let is_current = self.selected_file.as_ref().map(|p| {
                p == &PathBuf::from(&task.file_path)
                    || p.to_string_lossy().replace('\\', "/") == task.file_path.replace('\\', "/")
            }).unwrap_or(false);

            if is_current {
                if let Some(next_task) = self.recent_tasks.first().cloned() {
                    self.load_task(&next_task);
                } else {
                    self.clear_current_workspace();
                }
            }
        }
    }

    /// 彻底清空当前工作区工程状态（重置为空闲初始状态）
    pub fn clear_current_workspace(&mut self) {
        self.selected_file = None;
        self.status = ProcessStatus::Idle;
        self.segments.clear();
        self.current_time = 0.0;
        self.total_duration = 0.0;
        self.selected_segment_index = None;
        self.preview_frame_path = None;
        self.editing_text.clear();
        self.is_playing = false;
        self.preview_source = None;
        self.proxy_busy = false;
        self.clear_streaming();
    }

    /// 追加实时流式转写片段并推进时间戳
    pub fn push_stream_segment(&mut self, seg: Segment) {
        if seg.end > self.streaming_current_sec {
            self.streaming_current_sec = seg.end;
        }
        self.streaming_segments.push(seg);
    }

    /// 清理重置实时流式转写状态
    pub fn clear_streaming(&mut self) {
        self.streaming_segments.clear();
        self.streaming_current_sec = 0.0;
    }

    /// 检查当前待转写文件是否已存在本地已完成解析记录 (用于 0 秒智能缓存命中)
    pub fn get_cached_transcription(&self) -> Option<TaskRecord> {
        let file = self.transcribe_file.as_ref()?;
        let path_str = file.to_string_lossy();
        self.db.find_cached_task(&path_str).ok().flatten()
    }

    /// 0 秒智能缓存命中载入：瞬间恢复已缓存的完整字幕片段与各项指标，彻底跳过重复计算
    pub fn load_from_cache(&mut self, cached: TaskRecord) {
        let file_path = PathBuf::from(&cached.file_path);
        let total_dur = if cached.duration > 0.0 {
            cached.duration
        } else if self.transcribe_duration > 0.0 {
            self.transcribe_duration
        } else {
            cached.segments.last().map(|s| s.end).unwrap_or(0.0)
        };

        self.selected_file = Some(file_path.clone());
        self.preview_source = Some(file_path);
        self.segments = cached.segments;
        if let Some(first) = self.segments.first() {
            self.select_segment(first.index);
        }
        self.total_duration = total_dur;

        self.status = ProcessStatus::Idle;
        self.transcribe_file = None;
        self.transcribe_duration = 0.0;
        self.clear_streaming();
    }

    pub fn preview_media(&self) -> Option<&PathBuf> {
        self.preview_source.as_ref().or(self.selected_file.as_ref())
    }

    pub fn whisper_eta_label(&self) -> String {
        let secs = self.whisper_model_tier.estimate_seconds(
            self.total_duration,
            self.whisper_threads,
            self.hardware.use_gpu_pipeline(),
            self.enable_polish,
        );
        WhisperModelTier::format_eta(secs)
    }

    pub fn should_use_proxy(&self) -> bool {
        self.proxy_enabled
    }

    /// 获取当前播放时间对应的有效字幕片段
    pub fn get_active_segment(&self) -> Option<&Segment> {
        let t = self.current_time;
        self.segments.iter().find(|seg| t >= seg.start && t <= seg.end)
    }

    /// 选中指定索引的字幕片段
    pub fn select_segment(&mut self, index: usize) {
        self.selected_segment_index = Some(index);
        if let Some(seg) = self.segments.iter().find(|s| s.index == index) {
            self.editing_text = seg.display_text().to_string();
            self.current_time = seg.start;
        }
    }

    /// 跳转播放指针时间
    pub fn seek_to(&mut self, time_sec: f64) {
        let max_dur = if self.total_duration > 0.0 {
            self.total_duration
        } else {
            self.segments.last().map(|s| s.end).unwrap_or(3600.0)
        };
        self.current_time = time_sec.clamp(0.0, max_dur);

        // 如果跳转到的时间落在某个字幕内，且当前没有选或者选的不同，自动联动
        if let Some(seg) = self.get_active_segment() {
            let seg_idx = seg.index;
            if self.selected_segment_index != Some(seg_idx) {
                self.selected_segment_index = Some(seg_idx);
                if let Some(s) = self.segments.iter().find(|s| s.index == seg_idx) {
                    self.editing_text = s.display_text().to_string();
                }
            }
        }
    }

    /// 保存当前选中字幕片段的修改文本并写回数据库
    pub fn save_selected_text(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let new_text = self.editing_text.trim().to_string();
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            if !seg.polished.is_empty() {
                seg.polished = new_text;
            } else {
                seg.text = new_text;
            }
        }
        self.sync_segments_to_db();
    }

    /// 微调选中字幕片段的起止时间
    pub fn adjust_selected_times(&mut self, delta_start: f64, delta_end: f64) {
        let Some(idx) = self.selected_segment_index else { return; };
        if let Some(seg) = self.segments.iter_mut().find(|s| s.index == idx) {
            seg.start = (seg.start + delta_start).max(0.0);
            seg.end = (seg.end + delta_end).max(seg.start + 0.1);
        }
        self.sync_segments_to_db();
    }

    /// 拆分当前选中的字幕片段为两段
    pub fn split_selected_segment(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        let orig = self.segments[pos].clone();
        let mid_time = (orig.start + orig.end) / 2.0;

        let cur_text = orig.display_text().to_string();
        let char_count = cur_text.chars().count();
        let split_pos = (char_count / 2).max(1);
        let part1: String = cur_text.chars().take(split_pos).collect();
        let part2: String = cur_text.chars().skip(split_pos).collect();

        self.segments[pos].end = mid_time;
        if !self.segments[pos].polished.is_empty() {
            self.segments[pos].polished = part1;
        } else {
            self.segments[pos].text = part1;
        }

        let new_seg = Segment {
            index: orig.index + 1,
            start: mid_time,
            end: orig.end,
            text: part2.clone(),
            polished: if !orig.polished.is_empty() { part2 } else { String::new() },
            language: orig.language.clone(),
        };
        self.segments.insert(pos + 1, new_seg);

        // 重新规范化所有序号
        self.reindex_segments();
        self.select_segment(idx + 1);
        self.sync_segments_to_db();
    }

    /// 将当前选中字幕与下一段字幕合并
    pub fn merge_selected_with_next(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        if pos + 1 >= self.segments.len() { return; }

        let next = self.segments.remove(pos + 1);
        let cur = &mut self.segments[pos];
        cur.end = next.end;
        let combined = format!("{}{}", cur.display_text(), next.display_text());
        if !cur.polished.is_empty() || !next.polished.is_empty() {
            cur.polished = combined;
        } else {
            cur.text = combined;
        }

        self.reindex_segments();
        self.select_segment(idx);
        self.sync_segments_to_db();
    }

    /// 删除当前选中的字幕片段
    pub fn delete_selected_segment(&mut self) {
        let Some(idx) = self.selected_segment_index else { return; };
        let Some(pos) = self.segments.iter().position(|s| s.index == idx) else { return; };
        self.segments.remove(pos);

        self.reindex_segments();
        if !self.segments.is_empty() {
            let next_idx = self.segments[pos.min(self.segments.len() - 1)].index;
            self.select_segment(next_idx);
        } else {
            self.selected_segment_index = None;
            self.editing_text.clear();
        }
        self.sync_segments_to_db();
    }

    fn reindex_segments(&mut self) {
        for (i, seg) in self.segments.iter_mut().enumerate() {
            seg.index = i + 1;
        }
    }

    /// 同步当前字幕到 SQLite 数据库
    pub fn sync_segments_to_db(&self) {
        if let Some(ref file) = self.selected_file {
            let file_str = file.to_string_lossy();
            let _ = self.db.update_task_segments(&file_str, &self.segments);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WhisperModelTier;

    #[test]
    fn more_threads_shortens_cpu_eta() {
        let d = 32.0 * 60.0;
        let t4 = WhisperModelTier::Balanced.estimate_seconds(d, 4, false, false);
        let t8 = WhisperModelTier::Balanced.estimate_seconds(d, 8, false, false);
        let t16 = WhisperModelTier::Balanced.estimate_seconds(d, 16, false, false);
        assert!(t4 > t8, "4 threads ({t4}) should be slower than 8 ({t8})");
        assert!(t8 > t16, "8 threads ({t8}) should be slower than 16 ({t16})");
        assert_ne!(
            WhisperModelTier::format_eta(t4),
            WhisperModelTier::format_eta(t16)
        );
    }

    #[test]
    fn gpu_eta_is_faster_than_cpu() {
        let d = 32.0 * 60.0;
        let cpu = WhisperModelTier::Precise.estimate_seconds(d, 8, false, false);
        let gpu = WhisperModelTier::Precise.estimate_seconds(d, 8, true, false);
        assert!(gpu < cpu * 0.5);
    }
}
