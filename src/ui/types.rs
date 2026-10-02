//! UI 共享数据类型

use crate::core::PipelinePerformanceMetrics;

#[derive(Clone, Debug)]
pub struct CompletionDialogInfo {
    pub file_name: String,
    pub segment_count: usize,
    pub total_duration: f64,
    pub metrics: Option<PipelinePerformanceMetrics>,
    /// 批量转写跑完时的汇总；单文件转写为 `None`。
    /// 有值时弹窗展示队列统计而不是单文件性能看板。
    pub batch: Option<BatchSummary>,
}

/// 批量转写队列跑完后的汇总
#[derive(Clone, Debug)]
pub struct BatchSummary {
    /// 队列文件总数
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    /// 累计字幕句数
    pub segments: usize,
    /// 队列累计媒体时长（秒）
    pub total_duration: f64,
    /// 失败文件名，便于用户直接定位重试对象
    pub failed_names: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct BenchmarkDialogInfo {
    pub file_name: String,
    pub metrics: PipelinePerformanceMetrics,
}
