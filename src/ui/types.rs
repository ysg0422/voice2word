//! UI 共享数据类型

use crate::core::PipelinePerformanceMetrics;

#[derive(Clone, Debug)]
pub struct CompletionDialogInfo {
    pub file_name: String,
    pub segment_count: usize,
    pub total_duration: f64,
    pub metrics: Option<PipelinePerformanceMetrics>,
}

#[derive(Clone, Debug)]
pub struct BenchmarkDialogInfo {
    pub file_name: String,
    pub metrics: PipelinePerformanceMetrics,
}
