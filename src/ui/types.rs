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

/// 需要二次确认的破坏性操作。
///
/// 用枚举而不是存闭包：GPUI 的状态要能 `Clone + PartialEq`，闭包做不到；
/// 把「确认后做什么」编码成可比较的动作，渲染层与执行层就解耦了。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    /// 清空批量转写队列（队列非空或有转写在跑时会顺带终止）
    ClearBatchQueue,
    /// 删除视频库里的一条任务记录
    DeleteTaskRecord(i64),
}

/// 二次确认弹窗的内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmDialogInfo {
    pub title: String,
    pub message: String,
    /// 确认按钮上的文案（如「清空」「删除」），动词比「确定」更能让人看清在点什么
    pub confirm_label: String,
    /// 是否把确认键渲染成危险样式（不可逆操作用红）
    pub danger: bool,
    pub action: ConfirmAction,
}
