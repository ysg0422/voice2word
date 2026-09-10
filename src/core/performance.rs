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
    Low,     // 入门 / 轻量级设备 (如 <=4 核或 <=8GB 内存)
    Medium,  // 主流配置 (如 6~8 核, 16GB 内存, 轻薄本/主流台式机)
    High,    // 性能级配置 (如 8+ 核 16+ 线程, 16GB~32GB 内存, 或配备独立 GPU)
    Ultra,   // 旗舰 / 工作站配置 (如 16+ 核 32+ 线程, >=32GB 内存, 高性能 GPU)
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
            Self::Low => 0xeab308,     // 黄色
            Self::Medium => 0x38bdf8,  // 天蓝
            Self::High => 0x4ade80,    // 亮绿
            Self::Ultra => 0xa855f7,   // 紫色
        }
    }
}

/// 用户策略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserStrategy {
    Speed,      // 速度优先：追求极速转写与低延迟
    #[default]
    Balanced,   // 平衡模式：兼顾速度与准确率
    Quality,    // 精度优先：追求最高识别准确率与深度语义润色
}

impl UserStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Speed => "速度优先 (Speed)",
            Self::Balanced => "平衡模式 (Balanced)",
            Self::Quality => "精度优先 (Quality)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Speed => "适合长视频快速粗剪、会议快速纪要。优先选用轻量模型，大幅缩短耗时。",
            Self::Balanced => "日常推荐模式。在识别速度与错字准确率之间取得最佳平衡。",
            Self::Quality => "适合专业课程、公开演讲与高要求字幕制作。选用高精大模型与深度标点纠错。",
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
    /// 全面检测当前系统硬件
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
        let physical_cores = system.physical_core_count().unwrap_or(logical_threads.max(1));

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

            if is_discrete || is_integrated || !matches!(info.device_type, wgpu::DeviceType::Cpu | wgpu::DeviceType::Other) {
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
#[derive(Debug, Clone)]
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
            PerformanceLevel::High => if strategy == UserStrategy::Speed { 2 } else { 1 },
            PerformanceLevel::Ultra => if strategy == UserStrategy::Speed { 3 } else { 2 },
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
                recommendation_reason: "入门级硬件下采用极速 Base 模型并强制开启 VAD 跳过静音段，保障流畅运行。".into(),
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
                recommendation_reason: "入门配置推荐 Base 模型配合适度线程，防止 CPU 满载导致系统卡顿。".into(),
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
                recommendation_reason: "主流配置结合速度策略，转写耗时可降至视频时长的 5% 以下。".into(),
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
                recommendation_reason: "主流 CPU 推荐 Small 档位 + VAD 加速，兼具高识别率与适中处理速度。".into(),
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
                recommendation_reason: "选用 Large-v3-Turbo 模型获取极高识别精度，配合多核 CPU 并行计算。".into(),
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
                recommendation_reason: "多核性能机速度优先：多线程驱动 Small 模型，快速交付准确字幕。".into(),
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
                recommendation_reason: "性能级电脑推荐以 Large-v3-Turbo 为基准，提供工业级转写质量。".into(),
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
                recommendation_reason: "精度极致模式：全音频精细扫描，配合深度 LLM 标点纠错。".into(),
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
                recommendation_reason: "顶级算力下开启批处理并发，实现超实时极速字幕批量生成。".into(),
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
                recommendation_reason: "旗舰配置以 Large-v3-Turbo 满速运行，兼具极速与最高准确度。".into(),
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
                recommendation_reason: "工作站旗舰精度：多线程高精度模型深度解码，无损解析每一个微弱发音。".into(),
            },
        }
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
