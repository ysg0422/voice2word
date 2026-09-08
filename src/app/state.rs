//! 应用程序全局状态

use std::path::PathBuf;
use std::sync::Arc;

use crate::core::TaskPipeline;
use crate::storage::{Database, TaskRecord};
use crate::subtitle::Segment;
use crate::utils::AppConfig;

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
    Generate, // 智能生成管线
    Editor,   // 类似剪映/Premiere的剪辑校对工作台
}

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub db: Database,
    pub pipeline: Arc<TaskPipeline>,

    pub selected_file: Option<PathBuf>,
    pub status: ProcessStatus,
    pub segments: Vec<Segment>,
    pub recent_tasks: Vec<TaskRecord>,

    // 处理选项
    pub language: String,
    pub output_format: String,
    pub enable_polish: bool,
    pub whisper_threads: u32,

    // 硬件与模型资源监控
    pub metrics: ResourceMetrics,

    // 剪辑校对工作台状态 (类似剪映 / Premiere)
    pub active_tab: WorkspaceTab,
    pub current_time: f64,
    pub total_duration: f64,
    pub is_playing: bool,
    pub selected_segment_index: Option<usize>,
    pub preview_frame_path: Option<PathBuf>,
    pub timeline_zoom: f64,
    pub editing_text: String,
}

impl AppState {
    pub fn new(config: AppConfig, db: Database, pipeline: Arc<TaskPipeline>) -> Self {
        let recent_tasks = db.list_recent_tasks(20).unwrap_or_default();
        let lang = config.pipeline.language.clone();
        let fmt = config.pipeline.output_format.clone();
        let polish = config.pipeline.enable_polish;
        let threads = config.pipeline.whisper_threads.max(8);

        Self {
            config,
            db,
            pipeline,
            selected_file: None,
            status: ProcessStatus::Idle,
            segments: Vec::new(),
            recent_tasks,
            language: lang,
            output_format: fmt,
            enable_polish: polish,
            whisper_threads: threads,
            metrics: ResourceMetrics::default(),

            active_tab: WorkspaceTab::Generate,
            current_time: 0.0,
            total_duration: 0.0,
            is_playing: false,
            selected_segment_index: None,
            preview_frame_path: None,
            timeline_zoom: 1.0,
            editing_text: String::new(),
        }
    }

    pub fn set_selected_file(&mut self, path: PathBuf) {
        self.selected_file = Some(path);
    }

    pub fn refresh_recent_tasks(&mut self) {
        if let Ok(tasks) = self.db.list_recent_tasks(20) {
            self.recent_tasks = tasks;
        }
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
