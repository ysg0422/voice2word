//! 硬件检测、性能评估与 AI 推理策略决策模块
//!
//! 实现从「硬件检测 → 综合性能等级评估 → 用户策略矩阵 → 推荐推理配置」的全流程。

use rayon::prelude::*;
use std::time::Instant;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::app::state::WhisperModelTier;

/// 性能等级
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceLevel {
    Low,    // 入门 / 轻量级设备 (如 <=4 核或 <=8GB 内存)
    Medium, // 主流配置 (如 6~8 核, 16GB 内存, 轻薄本/主流台式机)
    High,   // 性能级配置 (如 8+ 核 16+ 线程, 16GB~32GB 内存, 或配备独立 GPU)
    Ultra,  // 旗舰 / 工作站配置 (如 16+ 核 32+ 线程, >=32GB 内存, 高性能 GPU)
}

impl PerformanceLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "入门级 (Low)",
            Self::Medium => "主流级 (Medium)",
            Self::High => "性能级 (High)",
            Self::Ultra => "旗舰级 (Ultra)",
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Ultra => "Ultra",
        }
    }

    pub fn color_hex(self) -> u32 {
        match self {
            Self::Low => 0xeab308,    // 黄色
            Self::Medium => 0x38bdf8, // 天蓝
            Self::High => 0x4ade80,   // 亮绿
            Self::Ultra => 0xa855f7,  // 紫色
        }
    }
}

/// 用户策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserStrategy {
    Speed, // 速度优先：追求极速转写与低延迟
    #[default]
    Balanced, // 平衡模式：兼顾速度与准确率
    Quality, // 精度优先：追求最高识别准确率与深度语义润色
}

impl UserStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Speed => "速度优先",
            Self::Balanced => "平衡模式",
            Self::Quality => "精度优先",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Speed => "轻量模型 · 优先转写耗时",
            Self::Balanced => "日常推荐 · 兼顾速度与准确度",
            Self::Quality => "大模型 · 深度标点与纠错",
        }
    }
}

/// 当前硬件推理模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceMode {
    CpuHighPerformance,
    GpuAccelerated,
}

impl InferenceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::CpuHighPerformance => "CPU 高速多线程模式",
            Self::GpuAccelerated => "GPU 硬件加速模式",
        }
    }
}

/// GPU 适配器信息
#[derive(Debug, Clone)]
pub struct GpuInfo {
    pub name: String,
    pub backend: String,
    pub is_discrete: bool,
    pub vram_mb: Option<u64>,
}

/// 硬件检测结果
#[derive(Debug, Clone)]
pub struct HardwareInfo {
    pub cpu_brand: String,
    pub physical_cores: usize,
    pub logical_threads: usize,
    pub cpu_frequency_mhz: u64,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub gpus: Vec<GpuInfo>,
    pub inference_mode: InferenceMode,
}

impl HardwareInfo {
    /// 全面检测当前系统硬件（CPU / 内存 / 全部 GPU 适配器）。
    ///
    /// # 别在启动关键路径上直接调它
    ///
    /// 内部会新建 wgpu `Instance` 并把所有适配器枚举一遍——实测约 2.7 s。
    /// GUI 启动路径（`main.rs`）因此把它整段丢进后台采集线程，并且只在
    /// `GPU 开关打开` 时才会跑；有硬件档案可用时应改用
    /// [`Self::detect_with_media_profile`]，它复用媒体管线选中的适配器，不额外枚举。
    ///
    /// 仍需一次全量枚举的调用点（例如无 GUI 的命令行路径）请自行放到非 UI 线程。
    pub fn detect() -> Self {
        let mut system = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything()),
        );
        system.refresh_cpu_all();
        system.refresh_memory();

        let cpus = system.cpus();
        let cpu_brand = cpus
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_else(|| "未知 CPU".to_string());
        let cpu_frequency_mhz = cpus.first().map(|c| c.frequency()).unwrap_or(0);
        let logical_threads = cpus.len().max(1);
        let physical_cores = system
            .physical_core_count()
            .unwrap_or(logical_threads.max(1));

        let total_memory_bytes = system.total_memory();
        let available_memory_bytes = system.available_memory();

        // 探测 GPU 信息通过 wgpu (加全局互斥锁保护并发环境安全)
        static GPU_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = GPU_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapters = instance.enumerate_adapters(wgpu::Backends::all());

        let mut gpus = Vec::new();
        let mut has_discrete_gpu = false;

        for adapter in adapters {
            let info = adapter.get_info();
            let is_discrete = matches!(info.device_type, wgpu::DeviceType::DiscreteGpu);
            let is_integrated = matches!(info.device_type, wgpu::DeviceType::IntegratedGpu);

            if is_discrete {
                has_discrete_gpu = true;
            }

            if is_discrete
                || is_integrated
                || !matches!(
                    info.device_type,
                    wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
                )
            {
                let backend_str = format!("{:?}", info.backend);
                gpus.push(GpuInfo {
                    name: info.name,
                    backend: backend_str,
                    is_discrete,
                    vram_mb: None,
                });
            }
        }

        let inference_mode = if has_discrete_gpu {
            InferenceMode::GpuAccelerated
        } else {
            InferenceMode::CpuHighPerformance
        };

        Self {
            cpu_brand,
            physical_cores,
            logical_threads,
            cpu_frequency_mhz,
            total_memory_bytes,
            available_memory_bytes,
            gpus,
            inference_mode,
        }
    }

    /// Reuse the adapter selected by the media pipeline. This keeps the CPU
    /// and memory measurements but avoids a second wgpu adapter enumeration
    /// during application startup.
    ///
    /// sysinfo 的**首次**采集（约 850 ms，实测）也发生在这里：调用方应把它放在
    /// 启动关键路径之外（`AppState::with_hardware` 前的汇合点），或确保已有一个
    /// 线程先采过一次——结果缓存在进程级静态状态里，之后的调用只要 ~1 ms。
    pub fn detect_with_media_profile(profile: &crate::engines::HardwareProfile) -> Self {
        static CACHE: std::sync::OnceLock<(String, usize, usize, u64, u64, u64)> = std::sync::OnceLock::new();
        let (cpu_brand, physical_cores, logical_threads, cpu_frequency_mhz, total_memory_bytes, available_memory_bytes) =
            CACHE.get_or_init(|| {
                let mut system = System::new_with_specifics(
                    RefreshKind::new()
                        .with_cpu(CpuRefreshKind::everything())
                        .with_memory(MemoryRefreshKind::everything()),
                );
                system.refresh_cpu_all();
                system.refresh_memory();

                let cpus = system.cpus();
                let cpu_brand = cpus
                    .first()
                    .map(|c| c.brand().trim().to_string())
                    .unwrap_or_else(|| "未知 CPU".to_string());
                let cpu_frequency_mhz = cpus.first().map(|c| c.frequency()).unwrap_or(0);
                let logical_threads = cpus.len().max(1);
                let physical_cores = system
                    .physical_core_count()
                    .unwrap_or(logical_threads.max(1));
                let total_memory_bytes = system.total_memory();
                let available_memory_bytes = system.available_memory();
                (cpu_brand, physical_cores, logical_threads, cpu_frequency_mhz, total_memory_bytes, available_memory_bytes)
            }).clone();

        let gpus = if profile.use_gpu_pipeline() {
            vec![GpuInfo {
                name: profile.adapter_name.clone(),
                backend: "wgpu".to_string(),
                is_discrete: profile.is_discrete,
                vram_mb: None,
            }]
        } else {
            Vec::new()
        };
        let inference_mode = if gpus.is_empty() {
            InferenceMode::CpuHighPerformance
        } else {
            InferenceMode::GpuAccelerated
        };

        Self {
            cpu_brand,
            physical_cores,
            logical_threads,
            cpu_frequency_mhz,
            total_memory_bytes,
            available_memory_bytes,
            gpus,
            inference_mode,
        }
    }

    /// 综合性能评估算法（基于多维硬件特征评分）
    pub fn evaluate_performance(&self) -> PerformanceLevel {
        let mut score: u32 = 0;

        // 1. CPU 核心与线程权重 (最高 45 分)
        if self.logical_threads >= 32 {
            score += 45;
        } else if self.logical_threads >= 16 {
            score += 35;
        } else if self.logical_threads >= 12 {
            score += 28;
        } else if self.logical_threads >= 8 {
            score += 20;
        } else if self.logical_threads >= 4 {
            score += 12;
        } else {
            score += 5;
        }

        // 2. 内存总量权重 (最高 30 分)
        let ram_gb = self.total_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        if ram_gb >= 30.0 {
            score += 30;
        } else if ram_gb >= 15.0 {
            score += 22;
        } else if ram_gb >= 7.5 {
            score += 14;
        } else {
            score += 5;
        }

        // 3. GPU 与图形能力权重 (最高 25 分)
        let has_discrete = self.gpus.iter().any(|g| g.is_discrete);
        let has_gpu = !self.gpus.is_empty();
        if has_discrete {
            score += 25;
        } else if has_gpu {
            score += 12;
        }

        // 4. 判定级别
        if score >= 80 {
            PerformanceLevel::Ultra
        } else if score >= 55 {
            PerformanceLevel::High
        } else if score >= 35 {
            PerformanceLevel::Medium
        } else {
            PerformanceLevel::Low
        }
    }

    pub fn formatted_total_memory(&self) -> String {
        let gb = self.total_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{:.1} GB", gb)
    }

    pub fn formatted_available_memory(&self) -> String {
        let gb = self.available_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        format!("{:.1} GB", gb)
    }
}

/// 统一推荐推理配置（与具体引擎解耦）
///
/// 字段全部是「档位 + 数字 + 文案」，PartialEq 只服务于单测里「换一套档案后推荐
/// 确实变了」这类断言，没有任何运行时语义。
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceProfile {
    pub whisper_tier: WhisperModelTier,
    pub whisper_model_name: &'static str,
    pub whisper_threads: u32,
    pub enable_vad: bool,
    pub llm_model_name: &'static str,
    pub llm_threads: u32,
    pub gpu_offload: bool,
    pub max_concurrency: u32,
    pub recommendation_reason: String,
}

impl InferenceProfile {
    /// 决策矩阵：根据 (PerformanceLevel, UserStrategy, has_gpu) 计算推荐参数
    pub fn decide(
        level: PerformanceLevel,
        strategy: UserStrategy,
        hardware: &HardwareInfo,
    ) -> Self {
        let has_gpu = !hardware.gpus.is_empty() && hardware.gpus.iter().any(|g| g.is_discrete);
        let threads = hardware.logical_threads as u32;

        // 合理分配 Whisper 与 LLM 线程，避免超线程争用，同时兼顾吞吐
        let whisper_threads = match level {
            PerformanceLevel::Low => threads.clamp(2, 4),
            PerformanceLevel::Medium => (threads * 3 / 4).clamp(4, 8),
            PerformanceLevel::High => (threads * 3 / 4).clamp(6, 12),
            PerformanceLevel::Ultra => (threads / 2).clamp(8, 16),
        };

        let llm_threads = match level {
            PerformanceLevel::Low => threads.clamp(2, 4),
            PerformanceLevel::Medium => (threads / 2).clamp(4, 8),
            PerformanceLevel::High => (threads / 2).clamp(6, 10),
            PerformanceLevel::Ultra => (threads / 2).clamp(8, 12),
        };

        let concurrency = match level {
            PerformanceLevel::Low => 1,
            PerformanceLevel::Medium => 1,
            PerformanceLevel::High => {
                if strategy == UserStrategy::Speed {
                    2
                } else {
                    1
                }
            }
            PerformanceLevel::Ultra => {
                if strategy == UserStrategy::Speed {
                    3
                } else {
                    2
                }
            }
        };

        match (level, strategy) {
            // ── 入门级 ──
            (PerformanceLevel::Low, UserStrategy::Speed) => Self {
                whisper_tier: WhisperModelTier::Fast,
                whisper_model_name: "Whisper Base (极速)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M (轻量版)",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason:
                    "入门级硬件下采用极速 Base 模型并强制开启 VAD 跳过静音段，保障流畅运行。".into(),
            },
            (PerformanceLevel::Low, UserStrategy::Balanced) => Self {
                whisper_tier: WhisperModelTier::Fast,
                whisper_model_name: "Whisper Base (极速)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason:
                    "入门配置推荐 Base 模型配合适度线程，防止 CPU 满载导致系统卡顿。".into(),
            },
            (PerformanceLevel::Low, UserStrategy::Quality) => Self {
                whisper_tier: WhisperModelTier::Balanced,
                whisper_model_name: "Whisper Small (均衡)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason: "在内存允许范围内提升至 Small 模型以提高识别精度。".into(),
            },

            // ── 主流级 ──
            (PerformanceLevel::Medium, UserStrategy::Speed) => Self {
                whisper_tier: WhisperModelTier::Fast,
                whisper_model_name: "Whisper Base (极速)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason: "主流配置结合速度策略，转写耗时可降至视频时长的 5% 以下。"
                    .into(),
            },
            (PerformanceLevel::Medium, UserStrategy::Balanced) => Self {
                whisper_tier: WhisperModelTier::Balanced,
                whisper_model_name: "Whisper Small (均衡)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason:
                    "主流 CPU 推荐 Small 档位 + VAD 加速，兼具高识别率与适中处理速度。".into(),
            },
            (PerformanceLevel::Medium, UserStrategy::Quality) => Self {
                whisper_tier: WhisperModelTier::Precise,
                whisper_model_name: "Whisper Large-v3-Turbo (高精)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason:
                    "选用 Large-v3-Turbo 模型获取极高识别精度，配合多核 CPU 并行计算。".into(),
            },

            // ── 性能级 ──
            (PerformanceLevel::High, UserStrategy::Speed) => Self {
                whisper_tier: WhisperModelTier::Balanced,
                whisper_model_name: "Whisper Small (均衡)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: concurrency,
                recommendation_reason:
                    "多核性能机速度优先：多线程驱动 Small 模型，快速交付准确字幕。".into(),
            },
            (PerformanceLevel::High, UserStrategy::Balanced) => Self {
                whisper_tier: WhisperModelTier::Precise,
                whisper_model_name: "Whisper Large-v3-Turbo (高精)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason:
                    "性能级电脑推荐以 Large-v3-Turbo 为基准，提供工业级转写质量。".into(),
            },
            (PerformanceLevel::High, UserStrategy::Quality) => Self {
                whisper_tier: WhisperModelTier::Precise,
                whisper_model_name: "Whisper Large-v3-Turbo (高精)",
                whisper_threads,
                enable_vad: false, // 精度优先可禁用粗粒度 VAD，完整扫描弱音细节
                llm_model_name: "Qwen2.5-0.5B Q4_K_M (全上下文)",
                llm_threads,
                gpu_offload: has_gpu,
                max_concurrency: 1,
                recommendation_reason: "精度极致模式：全音频精细扫描，配合深度 LLM 标点纠错。"
                    .into(),
            },

            // ── 旗舰级 / 工作站 ──
            (PerformanceLevel::Ultra, UserStrategy::Speed) => Self {
                whisper_tier: WhisperModelTier::Balanced,
                whisper_model_name: "Whisper Small (均衡)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: true,
                max_concurrency: concurrency,
                recommendation_reason: "顶级算力下开启批处理并发，实现超实时极速字幕批量生成。"
                    .into(),
            },
            (PerformanceLevel::Ultra, UserStrategy::Balanced) => Self {
                whisper_tier: WhisperModelTier::Precise,
                whisper_model_name: "Whisper Large-v3-Turbo (高精)",
                whisper_threads,
                enable_vad: true,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: true,
                max_concurrency: concurrency,
                recommendation_reason: "旗舰配置以 Large-v3-Turbo 满速运行，兼具极速与最高准确度。"
                    .into(),
            },
            (PerformanceLevel::Ultra, UserStrategy::Quality) => Self {
                whisper_tier: WhisperModelTier::Precise,
                whisper_model_name: "Whisper Large-v3-Turbo (高精)",
                whisper_threads,
                enable_vad: false,
                llm_model_name: "Qwen2.5-0.5B Q4_K_M",
                llm_threads,
                gpu_offload: true,
                max_concurrency: 1,
                recommendation_reason:
                    "工作站旗舰精度：多线程高精度模型深度解码，无损解析每一个微弱发音。".into(),
            },
        }
    }
}

/// 把推荐配置里的 GPU 开关消费进引擎配置（`GpuConfig::whisper_offload`）。
///
/// 这是 [`InferenceProfile::gpu_offload`] 的**唯一消费点**：决策矩阵只负责
/// 「建不建议开 GPU 转写」，真正被引擎读取的开关是 `config.gpu.whisper_offload`
/// （启动时 `main.rs` / `cli.rs` 把它与硬件能力相与后传给 `WhisperEngine`）。
/// 在此之前矩阵算出的 `gpu_offload` 没有任何读取方：推荐说「开 GPU」，
/// 「套用推荐配置」之后开关仍是旧值，属于假装生效。
///
/// 语义刻意只做单向提升（用 `Option<bool>` 区分两种结果）：
/// - 推荐 `true`（检测到独显，或 Ultra 旗舰档）→ 打开 `whisper_offload`，
///   返回 `Some(true)`。无 GPU 机器不会被它误开：引擎侧还会与
///   `HardwareProfile::use_gpu_pipeline()` 相与（`main.rs:101` 一带），
///   纯 CPU 机器上该值为 `false`。
/// - 推荐 `false`（在矩阵里只代表**没检测到独显**，核显不算）→ 不改动用户
///   当前开关，返回 `None`。若在这里强制关闭，正在用核显（如 Radeon 680M）
///   跑 Vulkan 转写的用户会被静默降级回纯 CPU。
pub fn consume_gpu_offload_recommendation(
    config: &mut crate::utils::config::GpuConfig,
    profile: &InferenceProfile,
) -> Option<bool> {
    if profile.gpu_offload {
        config.whisper_offload = true;
        Some(true)
    } else {
        None
    }
}

/// 基准测试结果
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub duration_ms: u128,
    pub score: u64,
    pub gflops_estimate: f64,
    pub throughput_rating: &'static str,
}

/// 执行 CPU 性能基准测试（多线程矩阵与浮点吞吐压测，耗时约 400~800ms）
pub fn run_cpu_benchmark() -> BenchmarkResult {
    let start = Instant::now();
    let num_threads = rayon::current_num_threads();

    // 并行计算密集型测试：多次矩阵相乘与浮点运算
    let iterations = 16 * num_threads.max(1);
    let size = 120;

    let _total: f64 = (0..iterations)
        .into_par_iter()
        .map(|seed| {
            let a = vec![seed as f64 * 0.01; size * size];
            let b = vec![0.5f64; size * size];
            let mut c = vec![0.0f64; size * size];

            for i in 0..size {
                for k in 0..size {
                    for j in 0..size {
                        c[i * size + j] += a[i * size + k] * b[k * size + j];
                    }
                }
            }

            // 非线性激活
            for val in &mut c {
                *val = val.sin().cos().abs();
            }
            c.iter().sum::<f64>()
        })
        .sum();

    let elapsed = start.elapsed();
    let duration_ms = elapsed.as_millis().max(1);

    // 预估浮点运算次数：iterations * 2 * size^3
    let ops = (iterations as f64) * 2.0 * ((size * size * size) as f64);
    let gflops = (ops / (elapsed.as_secs_f64() * 1e9)).max(0.1);
    let score = (gflops * 1000.0) as u64;

    let throughput_rating = if score > 80_000 {
        "极强 (Ultra Fast)"
    } else if score > 40_000 {
        "强劲 (High Performance)"
    } else if score > 15_000 {
        "良好 (Moderate)"
    } else {
        "一般 (Entry Level)"
    };

    BenchmarkResult {
        duration_ms,
        score,
        gflops_estimate: gflops,
        throughput_rating,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::config::GpuConfig;

    /// 造一个可控的硬件快照：`decide()` 只读逻辑线程数与 GPU 列表。
    fn fixture_hardware(logical_threads: usize, gpus: Vec<GpuInfo>) -> HardwareInfo {
        HardwareInfo {
            cpu_brand: "Fixture CPU".into(),
            physical_cores: logical_threads,
            logical_threads,
            cpu_frequency_mhz: 3200,
            total_memory_bytes: 32 * 1024 * 1024 * 1024,
            available_memory_bytes: 16 * 1024 * 1024 * 1024,
            gpus,
            inference_mode: InferenceMode::CpuHighPerformance,
        }
    }

    fn discrete_gpu() -> GpuInfo {
        GpuInfo {
            name: "Fixture RTX 4070".into(),
            backend: "Vulkan".into(),
            is_discrete: true,
            vram_mb: Some(12 * 1024),
        }
    }

    fn integrated_gpu() -> GpuInfo {
        GpuInfo {
            name: "Fixture Radeon 680M".into(),
            backend: "Vulkan".into(),
            is_discrete: false,
            vram_mb: None,
        }
    }

    /// `gpu_offload` 不是死字段：矩阵给出的推荐值必须能落进
    /// `GpuConfig::whisper_offload`（引擎真正读取的开关）。
    #[test]
    fn gpu_offload_recommendation_is_consumed() {
        let levels = [
            PerformanceLevel::Low,
            PerformanceLevel::Medium,
            PerformanceLevel::High,
            PerformanceLevel::Ultra,
        ];
        let strategies = [
            UserStrategy::Speed,
            UserStrategy::Balanced,
            UserStrategy::Quality,
        ];

        // 独显机器：四档 × 三策略全部推荐开启；消费后开关必须真的变成 true。
        // （修复前 `consume_gpu_offload_recommendation` 不存在，本用例编译即失败。）
        let with_discrete = fixture_hardware(16, vec![discrete_gpu()]);
        for level in levels {
            for strategy in strategies {
                let profile = InferenceProfile::decide(level, strategy, &with_discrete);
                assert!(
                    profile.gpu_offload,
                    "{level:?} / {strategy:?}：检测到独显时应推荐 GPU 转写"
                );
                let mut gpu = GpuConfig::default();
                assert_eq!(
                    consume_gpu_offload_recommendation(&mut gpu, &profile),
                    Some(true)
                );
                assert!(gpu.whisper_offload, "推荐为真时必须写进 config.gpu");
            }
        }

        // 核显机器：矩阵按「无独显」计算（false），消费端只做单向提升——
        // 不得把用户正在使用的核显 Vulkan 加速悄悄关掉。
        let integrated = fixture_hardware(8, vec![integrated_gpu()]);
        for level in [
            PerformanceLevel::Low,
            PerformanceLevel::Medium,
            PerformanceLevel::High,
        ] {
            for strategy in strategies {
                let profile = InferenceProfile::decide(level, strategy, &integrated);
                assert!(!profile.gpu_offload);
                let mut gpu = GpuConfig::from_mode("balanced");
                assert!(
                    gpu.whisper_offload,
                    "fixture 前置：balanced 档开着 GPU 转写"
                );
                assert_eq!(consume_gpu_offload_recommendation(&mut gpu, &profile), None);
                assert!(
                    gpu.whisper_offload,
                    "推荐 false 只代表没独显，不能顺手关掉核显用户的开关"
                );
            }
        }

        // Ultra 档在无独显时也硬编码推荐 true；写入开关是安全的：
        // 引擎侧与 `use_gpu_pipeline()` 相与，无 GPU 机器不会真的跑 GPU。
        let ultra_soft = fixture_hardware(24, Vec::new());
        let profile =
            InferenceProfile::decide(PerformanceLevel::Ultra, UserStrategy::Balanced, &ultra_soft);
        assert!(profile.gpu_offload);
        let mut gpu = GpuConfig::default();
        assert_eq!(
            consume_gpu_offload_recommendation(&mut gpu, &profile),
            Some(true)
        );
        assert!(gpu.whisper_offload);
    }
}
