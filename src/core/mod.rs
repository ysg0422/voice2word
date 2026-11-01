pub mod dub_flow;
pub mod metrics;
pub mod performance;
pub mod pipeline;

pub use dub_flow::{dub_text_for, parse_source, run_dub, DubOptions, DubReport, DubSource};
pub use metrics::PipelinePerformanceMetrics;
pub use performance::{
    run_cpu_benchmark, BenchmarkResult, GpuInfo, HardwareInfo, InferenceMode, InferenceProfile,
    PerformanceLevel, UserStrategy,
};
pub use pipeline::{
    EngineCapabilities, ExtractAudioRecord, PipelineEvent, Stage, StageArtifacts, StageRecord,
    TaskPipeline,
};
