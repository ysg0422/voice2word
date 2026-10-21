//! Unified media pipeline policy (Jianying-style).
//!
//! One NV12 WGSL shader is used for both GPU adapters and wgpu's CPU/WARP
//! software backend. Preview always prefers an H.264 proxy on machines without
//! a discrete/integrated GPU; export / independent playback keeps the original.
//!
//! # 输入侧硬解闭环
//!
//! 预览 / 代理链路的输入侧硬解现在**优先 `d3d12va`**，失败后按
//! [`HWACCEL_PREFERENCE`] 回退 `d3d11va`，再失败回退纯软解。候选顺序由
//! [`hwaccel_attempt_chain`] 统一给出，执行见 [`transcode_with_fallback`]；
//! 每一步失败都只影响本次转码的耗时，不会让预览 / 代理整体失败。
//!
//! # 已知回落：ONNX DirectML
//!
//! ONNX Runtime 的 DirectML 执行提供器需要带 DirectML 的 `onnxruntime` 构建，
//! 而本项目当前依赖的是 **CPU-only** 构建，因此 DirectML 路径会**静默回落到
//! CPU**。这是依赖侧的既有限制，不在本模块的决策范围内，此处只作如实记录。

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tracing::{info, warn};

use crate::engines::ffmpeg::{run_with_deadline, SUBPROCESS_TIMEOUT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBackend {
    Gpu,
    CpuSoftware,
}

#[derive(Debug, Clone)]
pub struct HardwareProfile {
    pub backend: RenderBackend,
    pub adapter_name: String,
    pub hardware_decode: bool,
    pub force_proxy: bool,
    pub is_discrete: bool,
    /// 厂商归类（从 `adapter_name` 推导），供「按厂商选后端」的策略分流。
    /// `None` 仅出现在软件后端（WARP/lavapipe）与纯 CPU 档案上。
    pub vendor: Option<Vendor>,
}

/// GPU 厂商归类。
///
/// 从 wgpu 的 `adapter_name` 字符串模糊归类。**只作策略提示，不作硬约束**——
/// 即使猜错，也只是选了一个未必最优的后端，不影响正确性。后续 AMD→DirectML、
/// NVIDIA→CUDA/NVENC、Intel→QSV 的分流都读这一个字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Nvidia,
    Intel,
    /// 其他厂商（Apple Silicon 等，本项目虽面向 Windows 也保留该分支）
    Other,
}

impl Vendor {
    /// 从适配器名模糊归类厂商。
    pub fn from_adapter_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        // 顺序即归类优先级：先按 "radeon"/"amd" 归类 AMD，再依次 NVIDIA、Intel、
        // Apple，命中即返回。同一适配器名可能命中多条关键词（如 "AMD Radeon(TM)
        // Graphics" 同时含 "amd" 与 "radeon"），固定顺序才能保证归类唯一、可预期。
        if n.contains("radeon") || n.contains("amd") {
            return Some(Vendor::Amd);
        }
        if n.contains("nvidia")
            || n.contains("geforce")
            || n.contains("quadro")
            || n.contains("rtx")
        {
            return Some(Vendor::Nvidia);
        }
        if n.contains("intel") || n.contains("iris") || n.contains("uhd") || n.contains("arc") {
            return Some(Vendor::Intel);
        }
        if n.contains("apple")
            || n.contains("m1")
            || n.contains("m2")
            || n.contains("m3")
            || n.contains("m4")
        {
            return Some(Vendor::Other);
        }
        None
    }

    pub fn label(&self) -> &'static str {
        match self {
            Vendor::Amd => "AMD",
            Vendor::Nvidia => "NVIDIA",
            Vendor::Intel => "Intel",
            Vendor::Other => "其他",
        }
    }
}

/// FFmpeg 输入侧硬解后端。
///
/// 本项目只面向 Windows，候选就这两种，顺序见 [`HWACCEL_PREFERENCE`]：
/// `d3d12va` 优先（较新的接口，在支持的驱动上通常开销更低），`d3d11va` 兜底
/// （覆盖面最广，AMD / Intel / NVIDIA 通吃，是唯一「一个后端打通三家」的选择）。
/// `None`（= 不加 `-hwaccel`）才是默认：无 GPU、厂商未知、或用户在设置页关掉
/// `gpu.hwaccel_decode` 时都走纯软解。
///
/// 两个变体现在都**会被真正尝试**：先首选后端，失败后依次回退其余候选，末尾
/// 兜底纯软解（见 [`hwaccel_attempt_chain`] / [`transcode_with_fallback`]）。
///
/// 只用于**预览 / 代理**链路（见 [`proxy_args`]）；抽音频 / 转写链路一律纯软解，
/// 边界见 `ffmpeg.rs` 顶部说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwAccel {
    D3d11va,
    D3d12va,
}

impl HwAccel {
    /// 传给 FFmpeg `-hwaccel` 的字面量（`"d3d11va"` / `"d3d12va"`）。
    pub fn as_ffmpeg_arg(self) -> &'static str {
        match self {
            HwAccel::D3d11va => "d3d11va",
            HwAccel::D3d12va => "d3d12va",
        }
    }

    /// [`Self::as_ffmpeg_arg`] 的历史别名，保留给既有调用点。
    pub fn flag(&self) -> &'static str {
        self.as_ffmpeg_arg()
    }

    /// 面向日志 / UI 的短标签（软解不在此枚举内，统一显示为 `software`）。
    pub fn label(&self) -> &'static str {
        match self {
            HwAccel::D3d11va => "D3D11VA",
            HwAccel::D3d12va => "D3D12VA",
        }
    }
}

/// 硬解候选的偏好顺序（不含末尾的软解兜底），顺序即尝试顺序。
///
/// `d3d12va` 优先：D3D12 Video Decode 是较新的接口，在支持的驱动上通常有更低的
/// 拷贝 / 同步开销。`d3d11va` 兜底：D3D11 Video Decoder 覆盖面最广，是 Windows 上
/// 唯一能一家通吃（AMD / Intel / NVIDIA）的后端，老驱动与部分核显只实现了它。
///
/// 两个变体都必须在此出现——这既是策略来源，也保证「定义了的变体都被使用」。
pub const HWACCEL_PREFERENCE: [HwAccel; 2] = [HwAccel::D3d12va, HwAccel::D3d11va];

/// 把「首选后端」展开成完整尝试链：顺序即尝试顺序，末尾一定是软解（`None`）。
///
/// - `Some(hw)`：先试 `hw`，再试 [`HWACCEL_PREFERENCE`] 里其余的硬件候选，最后软解。
/// - `None`（无 GPU / 厂商未知 / 用户关掉 `gpu.hwaccel_decode`）：只有软解一步。
///
/// 纯函数、无副作用，便于单测逐项断言顺序；执行侧见 [`transcode_with_fallback`]。
pub fn hwaccel_attempt_chain(preferred: Option<HwAccel>) -> Vec<Option<HwAccel>> {
    let mut chain: Vec<Option<HwAccel>> = Vec::new();
    if let Some(hw) = preferred {
        chain.push(Some(hw));
        chain.extend(
            HWACCEL_PREFERENCE
                .iter()
                .copied()
                .filter(|candidate| *candidate != hw)
                .map(Some),
        );
    }
    chain.push(None);
    chain
}

/// 日志用的后端名：软解统一显示为 `software`，硬件后端显示 FFmpeg 参数名。
fn hwaccel_name(backend: Option<HwAccel>) -> &'static str {
    backend.map_or("software", HwAccel::as_ffmpeg_arg)
}

#[derive(Debug, Clone, Copy)]
pub struct DecodePolicy {
    pub try_hwaccel: bool,
    pub software_threads: u32,
}

impl HardwareProfile {
    /// Enumerate adapters through wgpu. A software adapter is classified as CPU
    /// so proxy generation stays mandatory on that path.
    pub fn detect() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::all());
        adapters.sort_by_key(|a| match a.get_info().backend {
            wgpu::Backend::Vulkan | wgpu::Backend::Dx12 | wgpu::Backend::Metal => 0,
            wgpu::Backend::Gl => 1,
            _ => 2,
        });

        if let Some(adapter) = adapters.into_iter().find(|a| {
            let info = a.get_info();
            !matches!(
                info.device_type,
                wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
            )
        }) {
            let info = adapter.get_info();
            info!(
                adapter = %info.name,
                backend = ?info.backend,
                device_type = ?info.device_type,
                "GPU media pipeline enabled"
            );
            return Self {
                backend: RenderBackend::Gpu,
                adapter_name: info.name.clone(),
                hardware_decode: true,
                force_proxy: false,
                is_discrete: matches!(info.device_type, wgpu::DeviceType::DiscreteGpu),
                vendor: Vendor::from_adapter_name(&info.name),
            };
        }

        warn!("No hardware GPU adapter found; using FFmpeg software decode + wgpu software rasterizer");
        Self {
            backend: RenderBackend::CpuSoftware,
            adapter_name: "software (wgpu WARP / lavapipe)".into(),
            hardware_decode: false,
            force_proxy: true,
            is_discrete: false,
            vendor: None,
        }
    }

    /// CPU-only profile used when both GPU switches are disabled. It avoids
    /// creating a wgpu instance just to discover an adapter that will not be
    /// used by either preview or Whisper.
    pub fn cpu_only() -> Self {
        Self {
            backend: RenderBackend::CpuSoftware,
            adapter_name: "CPU software (GPU probe disabled)".into(),
            hardware_decode: false,
            force_proxy: true,
            is_discrete: false,
            vendor: None,
        }
    }

    pub fn use_gpu_pipeline(&self) -> bool {
        self.backend == RenderBackend::Gpu
    }

    pub fn decode_policy(&self) -> DecodePolicy {
        DecodePolicy {
            try_hwaccel: self.hardware_decode,
            software_threads: 0, // ffmpeg 0 = auto
        }
    }

    /// 预览 / 代理链路的**首选**输入侧硬解后端（`-hwaccel` 的取值）。
    ///
    /// 规则（路线图 P0-A3）：
    /// - AMD / Intel / NVIDIA → `d3d12va`（首选）。三家在 Win10 1809+ 的驱动都
    ///   提供 D3D12 Video Decode；用不上时由 [`hwaccel_attempt_chain`] 自动回退
    ///   `d3d11va`，再回退纯软解。NVIDIA 的 `cuda`/`nvdec` 在核显机器上不可用，
    ///   故不在此分流。
    /// - 无 GPU（软件后端 / 纯 CPU 档案）、厂商未知、`Vendor::Other` → `None`，
    ///   保持纯软解。
    ///
    /// 注意这只是**首选候选**：是否真用上、用不上时如何回退由
    /// [`hwaccel_attempt_chain`] 与 [`transcode_with_fallback`] 决定；默认构造
    /// （[`ProxyManager::new`]）不加任何 `-hwaccel`。
    pub fn hwaccel_backend(&self) -> Option<HwAccel> {
        let supported = matches!(
            self.vendor,
            Some(Vendor::Amd) | Some(Vendor::Nvidia) | Some(Vendor::Intel)
        );
        (self.hardware_decode && supported).then_some(HwAccel::D3d12va)
    }
}

/// 代理转码的执行器：按候选跑一次硬解/软解尝试，返回该次是否成功。
///
/// 抽成 `FnMut` 不是为了好看，而是为了**可测**：硬解在部分素材/驱动上真的会失败，
/// 而在任何一台机器上都无法保证构造出「d3d12va / d3d11va 必然失败」的素材来跑
/// 端到端测试。把「失败后怎么办」从「怎么跑」里切出来后，回退逻辑可以用一个假
/// 执行器完整覆盖（见 `hardware_failure_falls_back_through_the_candidate_chain`）。
type ProxyAttempt<'a> = Box<dyn FnMut(Option<HwAccel>) -> Result<bool> + 'a>;

/// 跑代理转码：**硬解候选按顺序尝试，失败逐级回退，最后兜底纯软解**；返回最终是否成功。
///
/// # 候选顺序
///
/// 顺序来自 [`hwaccel_attempt_chain`]：`preferred` 指定的后端先试，其余硬件候选
/// 随后，末尾一定是纯软解（`None`）。每个候选最多跑一次，任一成功立即返回，因此
/// 尝试次数有上界（2 个硬件 + 1 次软解），不存在循环。
///
/// # 为什么这条回退是必须的
///
/// `-hwaccel` 能不能用起来取决于素材编码、显卡驱动版本、图形会话类型（本地 /
/// RDP / 虚拟显示器）等本进程看不见的因素。不加回退，「加硬解」就等于
/// 「原本能转的素材现在转不了」——净负收益。软解那一步走的是同一套参数，只把
/// `-hwaccel` 去掉，因此结果必然等价于加硬解之前的历史行为。
///
/// 软解也失败就收手：那是素材本身的问题，继续换后端重试没有意义。
fn transcode_with_fallback(preferred: Option<HwAccel>, mut run: ProxyAttempt<'_>) -> Result<bool> {
    let chain = hwaccel_attempt_chain(preferred);
    let last = chain.len().saturating_sub(1);
    for (idx, backend) in chain.iter().enumerate() {
        if run(*backend)? {
            return Ok(true);
        }
        if idx < last {
            warn!(
                hwaccel = hwaccel_name(*backend),
                next = hwaccel_name(chain[idx + 1]),
                "hardware decode failed; falling back to the next candidate"
            );
        }
    }
    Ok(false)
}

/// 一次代理尝试的收尾：失败时删掉可能已经写出的截断 mp4，成功时保留它。
///
/// 为什么必须清理「执行器报错」这条路径：`proxy_ready` 的判据只有
/// 「存在且 >1KB」，而带 `+faststart` 的输出在转码中途被中断时 moov 还没写、
/// 文件却早就超过 1KB。这种半成品一旦留在 `%TEMP%`，下一次 `ensure_proxy` 的
/// 无锁快路径就会把它当成合格代理直接返回，同一个素材的预览会**一直**坏到用户
/// 清空临时目录为止。旧实现只在 `!ok`（退出码非零）时清理，而超时被杀 /
/// 启动失败是从执行器里 `?` 上抛的 `Err`——绕过清理，正是最需要清的那一类。
fn settle_proxy_attempt(out: &Path, result: Result<bool>) -> Result<bool> {
    match result {
        Ok(true) => Ok(true),
        Ok(false) => {
            let _ = std::fs::remove_file(out);
            Ok(false)
        }
        Err(err) => {
            let _ = std::fs::remove_file(out);
            Err(err)
        }
    }
}

/// 代理转码的输出侧参数（`-i <src>` 之后的那一段）。
///
/// `-hwaccel` **绝不**出现在这里：硬解属于输入侧，且只有 [`proxy_args`] 的
/// 策略判定有权决定加不加。抽出来是为了让「硬解开关只影响输入侧」这件事
/// 有一个单一出口，也方便单测直接比对软解 / 硬解两条路径。
fn soft_encode_args(vf: &str, threads: u32, out: &Path) -> Vec<String> {
    vec![
        "-vf".to_string(),
        vf.to_string(),
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "veryfast".to_string(),
        "-crf".to_string(),
        "28".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
        "-profile:v".to_string(),
        "baseline".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-b:a".to_string(),
        "96k".to_string(),
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-threads".to_string(),
        threads.to_string(),
        out.to_string_lossy().into_owned(),
    ]
}

/// 预览 / 代理转码的完整参数向量（**预览链路专用**）。
///
/// 多抽这一层的目的是「硬解策略可测」：`ensure_proxy` 过去把整条命令内联在
/// `Command::new(...).args([...])` 里，硬解与否只能靠真跑一次进程才知道，
/// 于是这条策略一直没人敢动。拆成「构造 `Vec<String>` → 喂给 `Command`」后，
/// 硬解 / 软解 / 回退三条路径都能在单测里逐参数断言。
///
/// 硬解写法刻意保守：只加 `-hwaccel <backend>`，**不加**
/// `-hwaccel_output_format`，让 FFmpeg 自己把帧下载回系统内存——这样后面的
/// `-vf scale` 与 `libx264` 都仍面对普通软件帧，滤镜链与编码器一行都不用改。
///
/// `hwaccel = None`（厂商未知 / 无 GPU / 用户关掉开关 / 硬解回退）时产出的
/// 参数与加硬解之前的历史版本**逐参数相同**。
pub fn proxy_args(
    source: &Path,
    out: &Path,
    height: u32,
    hwaccel: Option<HwAccel>,
    threads: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-y".to_string(),
    ];
    // 输入侧硬解：必须落在 `-i` 之前。放在 `-i` 之后 FFmpeg 会把它当成输出
    // 选项，轻则忽略、重则报错。
    if let Some(hw) = hwaccel {
        args.push("-hwaccel".to_string());
        args.push(hw.flag().to_string());
    }
    args.push("-i".to_string());
    args.push(source.to_string_lossy().into_owned());
    let vf = format!("scale=-2:{}", height);
    args.extend(soft_encode_args(&vf, threads, out));
    args
}

/// H.264 proxy generator. Proxy files are never HEVC.
pub struct ProxyManager {
    ffmpeg_path: PathBuf,
    /// 代理转码的输入侧硬解后端；`None` = 纯软解。
    ///
    /// 为什么是**实例字段**而不是进程级全局量：`ProxyManager` 在 `app::state`、
    /// UI 预览与测试里都有构造点，但真正决定「这台机器要不要硬解」的是
    /// [`HardwareProfile`]——把它显式传进来，比藏一个全局开关更难配错，也不会
    /// 出现「设置页关了硬解、代理却还在用」的错位。默认构造（[`Self::new`]）
    /// 取 `None`，与加硬解之前的历史行为逐字节相同。
    ///
    /// `Some(hw)` 只是**首选**后端：执行时按 [`hwaccel_attempt_chain`] 依次回退
    /// 其余硬件候选，最后兜底软解，因此单个后端不可用不会让代理失败。
    hwaccel: Option<HwAccel>,
    /// 转码闸门表：**输出文件路径 → 该路转码的互斥量**。
    ///
    /// `ensure_proxy` 真正需要串行化的共享资源只有一个：`proxy_path()` 算出来的
    /// 那个 `%TEMP%` 输出文件。同一路输出由「源文件 stem + 字节数 + 修改秒数 +
    /// 目标高度」决定，两条请求撞上同一素材时 `-y` 会互相覆盖，因此**同一路径**
    /// 只能有一条转码在跑；而不同输出之间没有任何共享句柄，不该互相排队。
    ///
    /// 这张表取代了过去那把把整段转码 + 回退链罩住的单把 `Mutex<()>`：那把锁让
    /// 「A 的 4K 代理」把「B 的代理请求（哪怕缓存早就命中）」和别的阶段一起堵住，
    /// 纯属白等。现在全局锁只在取 / 建闸门时短暂持有（微秒级），耗时转码只压住
    /// 同一个输出路径的并发请求；等锁的人拿到锁后会先复查文件，通常直接复用前
    /// 一条刚转完的结果，不会再重复转一遍。
    in_flight: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    height_cache: Mutex<HashMap<PathBuf, (FileSignature, Option<u32>)>>,
    /// 源素材时长缓存：路径 → (文件签名, 时长秒)。
    ///
    /// 只为**代理转码的预算缩放**服务：预算必须与素材时长成正比，否则长素材
    /// 必然超时（见 `run_proxy`）。探测用 `ffmpeg -i`，有 120 秒截止期；
    /// 按文件签名缓存，重复请求不会重复起进程。
    duration_cache: Mutex<HashMap<PathBuf, (FileSignature, Option<f64>)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    /// 修改时间的**纳秒**精度。
    ///
    /// 用秒精度会出真问题：同一秒内被覆盖的同名同大小素材签名完全相同，
    /// 于是代理路径不变，`proxy_ready` 直接把**上一版视频的代理**当成可用，
    /// 预览里播的是别的画面。
    modified_nanos: u128,
}

fn file_signature(path: &Path) -> FileSignature {
    match std::fs::metadata(path) {
        Ok(meta) => FileSignature {
            len: meta.len(),
            modified_nanos: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        },
        Err(_) => FileSignature {
            len: 0,
            modified_nanos: 0,
        },
    }
}

/// 代理文件是否已经可用：存在且大于 1KB。
///
/// 1KB 是原 `ensure_proxy` / `existing_proxy` 里就有的「完整代理」判据（`-y`
/// 写到一半被中断的截断 mp4 远小于它）。抽成一个函数，是为了让**无锁快路径**
/// 与**等锁后的复查**共用同一判据，不会再出现两处阈值各改各的。
/// 代理文件是否**确实可用**。
///
/// 判据不能只看「大于 1KB」：转码被超时 kill / 进程崩溃时，ffmpeg 已经写出的
/// 半截 mp4 通常有几十 MB，于是这条弱判据会把它当成合格代理，下一次预览直接复用，
/// 播放器打不开、用户以为「预览坏了」，而且**换素材前一直好不了**。
///
/// 真正的完成信号是 `moov` 原子：`proxy_args` 用了 `-movflags +faststart`，
/// ffmpeg 会在编码**全部结束**后把 moov 搬到文件头部。所以「前 1MB 内出现 moov」
/// 等价于「这次转码跑到了最后」；半截文件里根本不存在 moov（它本来在文件末尾，
/// 而 faststart 的重排发生在收尾阶段）。
///
/// 代价是一次最多 1MB 的读取，比再起一个 ffprobe 便宜得多，也够快——
/// 这条路径会被 UI 线程同步调用（`existing_proxy`）。
fn proxy_ready(out: &Path) -> bool {
    const HEAD_BYTES: u64 = 1024 * 1024;
    let Ok(meta) = out.metadata() else {
        return false;
    };
    if meta.len() <= 1024 {
        return false;
    }
    let Ok(file) = std::fs::File::open(out) else {
        return false;
    };
    use std::io::Read;
    let mut head = Vec::with_capacity(HEAD_BYTES as usize);
    if file.take(HEAD_BYTES).read_to_end(&mut head).is_err() {
        return false;
    }
    // MP4 的原子是 4 字节大端长度 + 4 字节类型；直接在头部字节里找 `moov` 即可
    // （对比 8 字节序列，避免把内容是文本的 mdat 误判）。
    head.windows(4).any(|w| w == b"moov")
}

impl ProxyManager {
    /// 默认构造：**纯软解**，与加硬解之前的历史行为完全一致。
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self::with_hwaccel(ffmpeg_path, None)
    }

    /// 指定输入侧硬解后端（`None` = 纯软解）。
    ///
    /// 调用方应传入 [`HardwareProfile::hwaccel_backend`] 的结果——厂商归类与
    /// 「无 GPU / 厂商未知就不加」的规则都在那里，本类型只负责用它拼命令。
    pub fn with_hwaccel<P: AsRef<Path>>(ffmpeg_path: P, hwaccel: Option<HwAccel>) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            hwaccel,
            in_flight: Mutex::new(HashMap::new()),
            height_cache: Mutex::new(HashMap::new()),
            duration_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 按硬件档案构造：一条语句就把「厂商归类 → 后端选择」接进代理链路。
    ///
    /// ```ignore
    /// let proxy_manager = Arc::new(ProxyManager::with_profile(&ffmpeg_path, &hardware));
    /// ```
    ///
    /// 之所以要显式传 [`HardwareProfile`] 而不是在内部自己 `detect()`：`hardware_decode`
    /// 在 `main.rs` 里会被 `gpu.hwaccel_decode` 覆盖过，内部重新探测会丢掉这个覆盖，
    /// 出现「设置页关了硬解、代理仍在用显卡」的错位。
    pub fn with_profile<P: AsRef<Path>>(ffmpeg_path: P, profile: &HardwareProfile) -> Self {
        Self::with_hwaccel(ffmpeg_path, profile.hwaccel_backend())
    }

    /// 本实例当前配置的硬解后端（`None` = 纯软解）。
    pub fn hwaccel(&self) -> Option<HwAccel> {
        self.hwaccel
    }

    /// 组装代理转码命令——**唯一的构造点**，参数全部来自 [`proxy_args`]。
    ///
    /// 硬解 / 软解 / 回退三种情形都走这里，因此不可能出现「某一分支漏了
    /// `-threads`」或「回退路径偷偷带着半截硬解参数」这类漂移。让路优先级
    /// （[`apply_background_priority`]）也在这里统一套用。
    pub fn proxy_cmd(
        &self,
        source: &Path,
        out: &Path,
        height: u32,
        hwaccel: Option<HwAccel>,
        threads: u32,
    ) -> Command {
        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_background_priority(&mut cmd);
        cmd.args(proxy_args(source, out, height, hwaccel, threads));
        cmd
    }

    pub fn preview_height<P: AsRef<Path>>(&self, source: P, cpu_machine: bool) -> u32 {
        let height = self.probe_height(source.as_ref()).unwrap_or(1080);
        if cpu_machine && height >= 2160 {
            540
        } else {
            720
        }
    }

    pub fn proxy_path<P: AsRef<Path>>(&self, source: P, height: u32) -> PathBuf {
        let source = source.as_ref();
        let name = source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("media");
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let signature = file_signature(source);
        // 路径也要进键：只有「stem + 字节数 + mtime」时，两个**不同目录**下同名、
        // 同大小、同一秒写入的文件会算出同一个代理路径，第二个文件的预览会播
        // 第一个文件的画面——而且不报错，用户只觉得「预览串了」。
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        let path_hash = hasher.finish();
        std::env::temp_dir().join(format!(
            "voice2word_proxy_{}_{:016x}_{}_{}_{}p.mp4",
            safe, path_hash, signature.len, signature.modified_nanos, height
        ))
    }

    /// 取某条输出路径的转码闸门：同一路径的并发调用拿到同一把锁，不同路径互不相干。
    ///
    /// 顺手回收没人持有的闸门（表里只剩它自己这一份强引用），避免「预览过的文件
    /// 越多、表越大」。回收是安全的：只有在线程既不持有、也不等待它时才会发生
    /// （等待者手里也有一份 `Arc`，强引用计数至少为 2），因此新插入的闸门不会与
    /// 旧闸门并发生效。
    fn transcode_gate(&self, out: &Path) -> Arc<Mutex<()>> {
        let mut gates = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        gates
            .entry(out.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn existing_proxy<P: AsRef<Path>>(&self, source: P, height: u32) -> Option<PathBuf> {
        let source = source.as_ref();
        if let Some(h) = self.probe_height(source) {
            if h <= height {
                return Some(source.to_path_buf());
            }
        }
        let out = self.proxy_path(source, height);
        if proxy_ready(&out) {
            Some(out)
        } else {
            None
        }
    }

    /// Returns the path that preview should play. Original is returned when the
    /// source is already H.264-friendly and not taller than the proxy height.
    pub fn ensure_proxy<P: AsRef<Path>>(&self, source: P, height: u32) -> Result<PathBuf> {
        let source = source.as_ref();
        if let Some(h) = self.probe_height(source) {
            if h <= height {
                info!(
                    height = h,
                    "source already within proxy resolution; skip transcode"
                );
                return Ok(source.to_path_buf());
            }
        }

        // 无锁快路径：现成的完整代理直接复用。旧代码把这段判定圈在全局锁里，
        // 于是「代理早就转好了」的请求也要排在别人几分钟的 4K 转码后面白等。
        let out = self.proxy_path(source, height);
        if proxy_ready(&out) {
            return Ok(out);
        }

        // 慢路径：只有真要转码才进锁，且锁**按输出路径**分配（见 `in_flight`
        // 字段说明）：不同素材的转码互不阻塞，同一素材的后续请求在这里排队，
        // 拿到锁后先复查文件，多数情况直接复用刚转完的结果。
        let gate = self.transcode_gate(&out);
        let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
        if proxy_ready(&out) {
            return Ok(out);
        }

        // 输入侧硬解策略：构造时由 [`HardwareProfile::hwaccel_backend`] 决定首选后端，
        // `None` 即纯软解；实际尝试顺序（含回退）见 [`hwaccel_attempt_chain`]。
        let hwaccel = self.hwaccel;
        let fallback_plan: Vec<&str> = hwaccel_attempt_chain(hwaccel)
            .iter()
            .map(|backend| (*backend).map_or("software", |hw| hw.label()))
            .collect();
        info!(
            source = %source.display(),
            height,
            out = %out.display(),
            hwaccel = hwaccel_name(hwaccel),
            fallback_plan = ?fallback_plan,
            "generating H.264 proxy (libx264, never HEVC)"
        );

        let encode_threads = proxy_encode_threads();
        info!(
            threads = encode_threads,
            "proxy transcode runs at below-normal priority with a bounded thread budget"
        );

        // 硬解候选按 d3d12va → d3d11va → 软解 的顺序回退，决策逻辑见
        // `transcode_with_fallback` / `hwaccel_attempt_chain`。
        // 每次尝试前先清掉半成品：失败的那次可能已经写出了截断的 mp4，`-y` 虽然
        // 会覆盖，但残留文件会让「成功判定」之外的下游误以为代理已就绪。
        let ok = transcode_with_fallback(
            hwaccel,
            Box::new(|backend| {
                // 每轮开始先清残留：上一轮失败（或更早一次被超时杀掉）可能已经
                // 写出截断的 mp4，而 `proxy_ready` 只按「>1KB」判完整。
                let _ = std::fs::remove_file(&out);
                settle_proxy_attempt(
                    &out,
                    self.run_proxy(source, &out, height, backend, encode_threads)
                        .map(|status| status.success()),
                )
            }),
        )?;

        if !ok {
            let _ = std::fs::remove_file(&out);
            anyhow::bail!("FFmpeg 代理生成失败");
        }
        Ok(out)
    }

    /// 跑一次代理转码，只负责「启动 + 等退出码」，成败判定留给调用方
    /// （[`Self::ensure_proxy`] 需要据退出码决定要不要回退重试）。
    ///
    /// 这里复用 `ffmpeg.rs` 的截止期执行器（`run_with_deadline`：轮询 + 超时 kill +
    /// 后台线程抽干管道），本文件不再写第二套「轮询 + kill」：过去这里是裸的
    /// `cmd.status()`，没有任何上界，坏素材会让转码线程永远挂住、UI 一直转圈。
    /// 转码耗时与素材时长成正比，因此用长任务截止期（600 秒）而不是探测用的 120 秒。
    ///
    /// **但 600 秒是与素材时长无关的固定值**：2 小时 4K 素材转 720p 约需 20~40 分钟
    /// （1200~2400 秒），必然被 kill；回退链再跑一次软解（同样超时），最终
    /// `ensure_proxy` 报错、UI 回退播原片——也就是「长素材在无 GPU 机器上永远拿不到代理」。
    ///
    /// 现在按**源时长线性缩放**预算：实测 720p 编码速度约 3~6 倍实时，给足 3 倍余量，
    /// 再加固定开销（进程启动 + 写盘）。短素材仍是原来的量级（不会因为缩放而变慢），
    /// 长素材则能拿到与之匹配的预算。
    fn run_proxy(
        &self,
        source: &Path,
        out: &Path,
        height: u32,
        hwaccel: Option<HwAccel>,
        threads: u32,
    ) -> Result<std::process::ExitStatus> {
        let mut cmd = self.proxy_cmd(source, out, height, hwaccel, threads);
        let budget = proxy_timeout_for(self.source_duration_secs(source));
        let output = run_with_deadline(&mut cmd, budget)
            .with_context(|| format!("启动代理生成失败: {:?}", self.ffmpeg_path))?;
        if !output.status.success() {
            // 旧写法让子进程继承控制台，错误信息由 FFmpeg 自己吐；换成截止期执行器
            // 后 stdout / stderr 由我们收齐，失败时在这里补一条，别把排障信息丢掉。
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                stderr = %stderr.trim(),
                "代理转码退出码非零，FFmpeg 输出如下"
            );
        }
        Ok(output.status)
    }

    /// 源素材时长（秒）。探测失败返回 `None`（预算退回基准值）。
    ///
    /// 只为**代理转码的预算缩放**服务：预算必须与素材时长成正比，否则长素材
    /// 必然被固定 600 秒的截止期杀掉（见 `run_proxy`）。
    ///
    /// 按文件签名缓存：素材变了签名就变，缓存自动失效。
    fn source_duration_secs(&self, source: &Path) -> Option<f64> {
        let signature = file_signature(source);
        if let Some((cached_signature, dur)) = self
            .duration_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source)
            .copied()
        {
            if cached_signature == signature {
                return dur;
            }
        }

        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_no_window(&mut cmd);
        cmd.arg("-i").arg(source);
        // 与 `probe_height` 同一套截止期执行器：坏素材 / 网络盘可能长时间不返回。
        let duration = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT)
            .ok()
            .and_then(|out| {
                super::ffmpeg::parse_ffmpeg_container_duration(&String::from_utf8_lossy(
                    &out.stderr,
                ))
            });
        self.duration_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_path_buf(), (signature, duration));
        duration
    }

    fn probe_height(&self, source: &Path) -> Option<u32> {
        let signature = file_signature(source);
        if let Some((cached_signature, height)) = self
            .height_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source)
            .copied()
        {
            if cached_signature == signature {
                return height;
            }
        }

        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_no_window(&mut cmd);
        cmd.arg("-i").arg(source);
        // 与 `run_proxy` 复用同一套截止期执行器（见 `run_with_deadline`）：`ffmpeg -i`
        // 探测坏素材 / 网络盘时可能长时间不返回，而这条路径会被 UI 线程同步调用
        // （`existing_proxy` → `probe_height`），一旦挂住就是整窗卡死。超时与启动
        // 失败都按「测不出来」处理（`None`），与旧的 `.output().ok()?` 语义一致。
        let output = run_with_deadline(&mut cmd, SUBPROCESS_TIMEOUT).ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let height = parse_ffmpeg_height(&stderr);
        self.height_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_path_buf(), (signature, height));
        height
    }
}

/// 代理转码的**基准**预算：与源时长无关的下限（短素材用这个）。
///
/// 原先这里是固定的 600 秒，对长素材来说远远不够（见 `run_proxy` 的说明）。
const PROXY_BASE_BUDGET_SECS: u64 = 600;

/// 代理转码的**硬上限**：无论素材多长都不超过这个值。
///
/// 没有上限的话，一份 24 小时的素材会把转码线程占住好几小时——
/// 即便用户早就离开了。
const PROXY_MAX_BUDGET_SECS: u64 = 4 * 3600;

/// 代理转码预算 = 基准 + 源时长 × 倍率。
///
/// 倍率 3 的依据：720p / veryfast / CRF 28 的 libx264 编码速度实测约 3~6 倍实时
/// （低端 CPU 偏低端、高端偏高端），再叠加一次软解回退（再乘 2），3 倍是
/// 「够用且不太久」的折中。倍率**不随源时长变化**——素材越长，单位时间的编码
/// 开销越接近稳态，按比例放大不会失真。
pub fn proxy_timeout_for(duration_secs: Option<f64>) -> std::time::Duration {
    let secs = duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| d as u64)
        .unwrap_or(0);
    let scaled = PROXY_BASE_BUDGET_SECS.saturating_add(secs.saturating_mul(3));
    std::time::Duration::from_secs(scaled.min(PROXY_MAX_BUDGET_SECS))
}
pub fn parse_ffmpeg_height(ffmpeg_stderr: &str) -> Option<u32> {
    for token in ffmpeg_stderr.split_whitespace() {
        let token = token.trim_end_matches(',');
        if let Some((w, h)) = token.split_once('x') {
            if let (Ok(ww), Ok(hh)) = (w.parse::<u32>(), h.parse::<u32>()) {
                if (16..20000).contains(&ww) && (16..20000).contains(&hh) {
                    return Some(hh);
                }
            }
        }
    }
    None
}

pub fn apply_no_window(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

/// 代理转码专用启动参数：隐藏控制台 + 降低到 BELOW_NORMAL 优先级。
///
/// 代理生成是后台非紧急任务，而 ASR（Whisper / SenseVoice）才是整条链路里
/// 占 99% 耗时的关键路径。若代理以默认优先级抢占 CPU，用户点下「开始转写」
/// 后识别速度会被明显拖慢，因此这里主动让出调度权重。
pub fn apply_background_priority(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    let _ = cmd;
}

/// 进程级的「让路模式」总开关。
///
/// # 为什么用全局而不是逐个引擎传参
///
/// 这是**进程级策略**，不是某个引擎的属性：用户在设置页打开一次「让路」，
/// 期望的是「所有会吃 CPU/GPU 的子进程都降优先级」。而 `FFmpegEngine` 与
/// `LLMEngine` 在代码里有十多个构造点（预览抽帧、波形、代理、时长探测、
/// 翻译……），逐个加参数既啰嗦又必然会漏。
///
/// 默认 `true`，与 `config.toml` 的 `gpu.yield_to_desktop` 默认值一致；
/// 启动时由 `main.rs` 按配置覆盖一次。
static YIELD_TO_DESKTOP: AtomicBool = AtomicBool::new(true);

/// 设置让路模式（启动时由 `main.rs` 按 `config.toml` 调用一次）。
pub fn set_yield_to_desktop(on: bool) {
    YIELD_TO_DESKTOP.store(on, Ordering::Relaxed);
}

/// 当前是否处于让路模式。
pub fn yield_to_desktop() -> bool {
    YIELD_TO_DESKTOP.load(Ordering::Relaxed)
}

/// 按**全局让路设置**为子进程套用启动标志（隐藏窗口 + 可选降优先级）。
///
/// 绝大多数调用点都应该用这个，而不是自己写 `creation_flags(0x08000000)`——
/// 后者会静默忽略用户的让路开关。
pub fn apply_default_child_flags(cmd: &mut Command) {
    apply_child_flags(cmd, yield_to_desktop());
}

/// Python 子进程的环境清洗（只对 Python 用）。
///
/// 用户机器上残留的 `PYTHONHOME` / `PYTHONPATH` 会把解释器的库搜索路径带偏：
/// 典型症状就是客户机上报的 `Could not find platform independent libraries <prefix>`
/// + `Fatal Python error: Failed to import encodings module`——解释器本身没坏，
/// 是被环境变量指到了另一个（残缺的）安装。这里统一剥掉，并固定 UTF-8 输出。
pub fn apply_python_env(cmd: &mut Command) {
    for key in [
        "PYTHONHOME",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "PYTHONEXECUTABLE",
        "PYTHONCASEOK",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1");
}

/// 统一的子进程启动标志：始终隐藏控制台；`yield_to_desktop` 为真时额外降到
/// BELOW_NORMAL_PRIORITY_CLASS，把 CPU 与 GPU 调度权重让给桌面与前台程序。
///
/// 核显（Radeon 680M 这类）既要跑推理又要输出画面：Whisper Vulkan 会把 compute
/// 队列持续压在 65%~80%，此时整机拖窗口、切前台都会顿。实测 5 分钟音频 /
/// small-q5_0 / 16 线程：让路开 = 18.4s、GPU 均值 62.7%；让路关 = 19.7s、GPU 均值
/// 65.1%。速度基本无损，桌面跟手程度差别明显。
pub fn apply_child_flags(cmd: &mut Command, yield_to_desktop: bool) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
        let mut flags = CREATE_NO_WINDOW;
        if yield_to_desktop {
            flags |= BELOW_NORMAL_PRIORITY_CLASS;
        }
        cmd.creation_flags(flags);
    }
    let _ = (cmd, yield_to_desktop);
}

/// GPU 占空比限速器：按「跑 N% 时间、挂起 (100-N)% 时间」给 GPU 主动留出空窗。
///
/// 为什么需要它：进程优先级（BELOW_NORMAL）只影响 CPU 时间片分配，管不到已经提交到
/// GPU 命令队列里的工作量。核显（如 Radeon 680M）既要跑 Whisper Vulkan 推理、又要
/// 驱动显示器，compute 队列一旦排满，桌面合成器就只能排队等 GPU，表现为拖窗掉帧。
/// 挂起进程会让其命令队列在几十毫秒内排空，桌面拿到独占 GPU 的窗口；恢复后推理从
/// 断点继续，显存不释放、模型不重载，因此「有效计算时段」内的吞吐不变，只是把总
/// 耗时按比例拉长（上限 60% → 约 1.67 倍）。
///
/// 生命周期约定：析构时会自动停止限速线程（见 `Drop` 实现），因此提前返回不会
/// 留下野线程；但**必须在 `Child` 被 drop（进程句柄关闭）之前**完成停止——
/// 否则系统可能把该句柄值复用到别的进程上，工作线程就会误挂起无关进程。
/// 由于 `GpuThrottle` 总是声明在 `Child` 之后，Rust 的局部变量逆序析构天然满足这一点。
pub struct GpuThrottle {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl GpuThrottle {
    /// 空实现：不做任何限速。
    pub fn off() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
        }
    }

    /// 为正在运行的子进程启动限速。`limit_percent >= 100`、非 Windows 平台
    /// 或拿不到 ntdll 导出函数时自动退化为空实现（即保持原速）。
    pub fn for_child(child: &std::process::Child, limit_percent: u32) -> Self {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::AsRawHandle;
            Self::start(child.as_raw_handle() as isize, limit_percent)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (child, limit_percent);
            Self::off()
        }
    }

    #[cfg(target_os = "windows")]
    fn start(raw_handle: isize, limit_percent: u32) -> Self {
        if raw_handle == 0 || limit_percent >= 100 {
            return Self::off();
        }
        if !nt_process::available() {
            warn!("ntdll 未导出 NtSuspendProcess/NtResumeProcess，GPU 限速降级为不限速");
            return Self::off();
        }
        let limit = limit_percent.clamp(20, 95);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        info!(
            limit_percent = limit,
            "GPU 限速已开启：按占空比给桌面留出 GPU 空窗"
        );
        let worker = std::thread::Builder::new()
            .name("gpu-duty-throttle".into())
            .spawn(move || {
                // 100ms 周期在「桌面跟手」与「切换开销」之间折中：
                // 让出段至少 5ms，足够 60Hz 合成器补上几帧，同时每秒只切换 10 次。
                const PERIOD_MS: u64 = 100;
                let run_ms = (PERIOD_MS * limit as u64 / 100).max(5);
                let idle_ms = (PERIOD_MS - run_ms).max(5);
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(run_ms));
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    if !nt_process::set_suspended(raw_handle, true) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(idle_ms));
                    if !nt_process::set_suspended(raw_handle, false) {
                        break;
                    }
                }
                // 兜底：任何退出路径都必须恢复子进程，否则转写会永久卡死
                nt_process::set_suspended(raw_handle, false);
            })
            .ok();
        Self { stop, worker }
    }

    /// 停止限速并等待工作线程退出，确保返回后不会再有任何挂起动作。
    ///
    /// 等价于析构，保留这个方法是为了让调用点能显式表达「现在就要停」的意图
    /// （例如 `child.wait()` 之后、`Child` 句柄关闭之前）。
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// 停线程 + join。被 [`Self::stop`] 与 `Drop` 共用，保证两条路径行为一致。
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for GpuThrottle {
    fn drop(&mut self) {
        // 为什么必须补 Drop：限速线程握着子进程的原始句柄按 100ms 周期挂起/恢复它，
        // 一旦 `GpuThrottle` 在任意 `?` / 提前返回 / panic 路径上被直接丢弃而没有
        // `stop()`，线程就会永远转下去——句柄随 `Child` 关闭后被系统复用给别的进程时，
        // 它就会开始误挂起无关进程（例如桌面窗口）。把清理绑定到析构上，
        // 就一次性覆盖了所有退出路径，不必在每个 return 前手动补 stop()。
        self.shutdown();
    }
}

/// `NtSuspendProcess` / `NtResumeProcess` 没有出现在 Windows SDK 头文件里，
/// 只能运行时从 ntdll.dll 取函数地址（Process Explorer 等工具同款做法），
/// 因此不引入 ntdll.lib 依赖，缺符号时安全降级。
#[cfg(target_os = "windows")]
mod nt_process {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    type SuspendResumeFn = unsafe extern "system" fn(*mut c_void) -> i32;

    struct Api {
        suspend: SuspendResumeFn,
        resume: SuspendResumeFn,
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            let module = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            if module.is_null() {
                return None;
            }
            let suspend = GetProcAddress(module, c"NtSuspendProcess".as_ptr().cast());
            let resume = GetProcAddress(module, c"NtResumeProcess".as_ptr().cast());
            if suspend.is_null() || resume.is_null() {
                return None;
            }
            Some(Api {
                suspend: std::mem::transmute::<*mut c_void, SuspendResumeFn>(suspend),
                resume: std::mem::transmute::<*mut c_void, SuspendResumeFn>(resume),
            })
        })
        .as_ref()
    }

    pub fn available() -> bool {
        api().is_some()
    }

    pub fn set_suspended(raw_handle: isize, suspended: bool) -> bool {
        let Some(api) = api() else {
            return false;
        };
        let handle = raw_handle as *mut c_void;
        let status = unsafe {
            if suspended {
                (api.suspend)(handle)
            } else {
                (api.resume)(handle)
            }
        };
        status >= 0
    }
}

/// 代理转码的编码线程预算：按逻辑核心数的 1/4 计算，上限 4、下限 1。
///
/// 原先使用 `-threads 0`（FFmpeg 自动，实际约 1.5x 核心数），16 核机器上会
/// 起 20+ 个编码线程，与 8~16 线程的 ASR 进程直接争抢核心与内存带宽。
/// 代理只服务于预览流畅度，不需要满速，留出核心给识别才是正确取舍。
pub fn proxy_encode_threads() -> u32 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cores / 4).clamp(1, 4) as u32
}

/// Shared NV12 → RGB shader. Runs on a hardware adapter when one exists,
/// otherwise on wgpu's CPU/WARP software backend — same pipeline either way.
pub const NV12_WGSL_SHADER: &str = r#"
@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var uv_plane: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    var x = -1.0;
    var y = -1.0;
    if (i == 1u) { x = 3.0; y = -1.0; }
    else if (i == 2u) { x = -1.0; y = 3.0; }
    return vec4(x, y, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let coord = vec2<i32>(pos.xy);
    let y = textureLoad(y_plane, coord, 0).r - 0.0625;
    let c = textureLoad(uv_plane, coord / 2, 0).rg - vec2<f32>(0.5, 0.5);
    // BT.601 limited-range, column-major mat3 * vec3(y, u, v)
    let rgb = mat3x3<f32>(
        vec3<f32>(1.164, 1.164, 1.164),
        vec3<f32>(0.0, -0.392, 2.017),
        vec3<f32>(1.596, -0.813, 0.0)
    ) * vec3<f32>(y, c.x, c.y);
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

/// NV12 尺寸上界：既挡奇数尺寸（UV 平面下标越界），也挡会让
/// `width * height` / `padded_bytes_per_row(width * 4)` 这类 u32 运算回绕的
/// 天文数字。16K 远超任何真实素材与 wgpu 的纹理上限。
const NV12_MAX_DIM: u32 = 16_384;

/// NV12 尺寸是否可用：偶数、非零、不超过 [`NV12_MAX_DIM`]。
///
/// 纯判定，CPU 转换与 wgpu 渲染器共用同一条约束。
fn nv12_dims_ok(width: u32, height: u32) -> bool {
    width >= 2
        && height >= 2
        && width <= NV12_MAX_DIM
        && height <= NV12_MAX_DIM
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
}

/// NV12 平面布局：返回 `(Y 平面字节数, 缓冲区总需求字节数)`。
///
/// 尺寸非法或乘法溢出时返回 `None`——调用方据此给出可恢复的 `Err`，
/// 而不是让 `usize` 乘法在 debug 下 panic、在 release 下回绕后拿一个
/// 荒谬的下标去切片。
fn nv12_plane_layout(width: u32, height: u32) -> Option<(usize, usize)> {
    if !nv12_dims_ok(width, height) {
        return None;
    }
    let y_size = (width as usize).checked_mul(height as usize)?;
    let need = y_size.checked_add(y_size / 2)?;
    Some((y_size, need))
}

/// CPU fallback of the same BT.601 limited-range conversion (R,G,B,A).
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    // NV12 的 UV 平面是 2x2 下采样：宽（或高）为奇数时 `col & !1` / `row / 2` 会
    // 算出越过 UV 平面末尾的下标——例如 w=3, h=2 时 y_size=6、UV 平面只有 3 字节，
    // 而最后一列取到 `uv_plane[3]`，直接 index out of bounds panic。
    // GPU 路径（`Nv12Renderer::new`）与这里共用 [`nv12_dims_ok`] / [`nv12_plane_layout`]，
    // 让非法尺寸走可恢复的 Err 而不是在渲染线程里崩溃。
    anyhow::ensure!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "NV12 尺寸必须为偶数: {width}x{height}"
    );
    let (y_size, need) = nv12_plane_layout(width, height)
        .with_context(|| format!("NV12 尺寸超出安全范围: {width}x{height}"))?;
    let w = width as usize;
    let h = height as usize;
    if nv12.len() < need {
        anyhow::bail!("NV12 buffer too small: {} < {}", nv12.len(), need);
    }
    let y_plane = &nv12[..y_size];
    let uv_plane = &nv12[y_size..need];
    let mut out = vec![0u8; y_size * 4];
    for row in 0..h {
        for col in 0..w {
            let y = y_plane[row * w + col] as f32;
            let uv_index = (row / 2) * w + (col & !1);
            let u = uv_plane[uv_index] as f32;
            let v = uv_plane[uv_index + 1] as f32;
            let c = y - 16.0;
            let d = u - 128.0;
            let e = v - 128.0;
            let r = (1.164 * c + 1.596 * e).clamp(0.0, 255.0) as u8;
            let g = (1.164 * c - 0.392 * d - 0.813 * e).clamp(0.0, 255.0) as u8;
            let b = (1.164 * c + 2.017 * d).clamp(0.0, 255.0) as u8;
            let i = (row * w + col) * 4;
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            out[i + 3] = 255;
        }
    }
    Ok(out)
}

#[derive(Clone)]
pub struct DecodedFrame {
    pub pts: f64,
    pub rgba: Vec<u8>,
}

pub struct FrameRing {
    inner: VecDeque<DecodedFrame>,
    capacity: usize,
}

impl FrameRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, frame: DecodedFrame) {
        while self.inner.len() >= self.capacity {
            self.inner.pop_front();
        }
        self.inner.push_back(frame);
    }

    pub fn latest(&self) -> Option<&DecodedFrame> {
        self.inner.back()
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Drop frames older than `pts` and return the closest remaining frame.
    pub fn take_closest(&mut self, pts: f64) -> Option<DecodedFrame> {
        while self.inner.len() > 1 {
            let older_gap = (self.inner[0].pts - pts).abs();
            let newer_gap = (self.inner[1].pts - pts).abs();
            if older_gap >= newer_gap {
                self.inner.pop_front();
            } else {
                break;
            }
        }
        self.inner.pop_front()
    }
}

/// wgpu NV12 converter. Prefers a hardware adapter, falls back to a CPU adapter
/// so the same WGSL runs on machines without a GPU.
pub struct Nv12Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    y_tex: wgpu::Texture,
    uv_tex: wgpu::Texture,
    out_tex: wgpu::Texture,
    out_buf: wgpu::Buffer,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    padded_bpr: u32,
}

impl Nv12Renderer {
    pub fn new(prefer_gpu: bool, width: u32, height: u32) -> Result<Self> {
        anyhow::ensure!(
            nv12_dims_ok(width, height),
            "NV12 渲染器尺寸非法（需为 2..={NV12_MAX_DIM} 的偶数）: {width}x{height}"
        );
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let mut adapters = instance.enumerate_adapters(wgpu::Backends::all());
        adapters.sort_by_key(|a| {
            let info = a.get_info();
            let ty = match info.device_type {
                wgpu::DeviceType::DiscreteGpu => 0,
                wgpu::DeviceType::IntegratedGpu => 1,
                wgpu::DeviceType::VirtualGpu => 2,
                wgpu::DeviceType::Cpu => 4,
                wgpu::DeviceType::Other => 5,
            };
            if prefer_gpu {
                ty
            } else {
                4u8.saturating_sub(ty.min(4))
            }
        });
        let adapter = adapters
            .into_iter()
            .next()
            .context("wgpu 未枚举到任何 adapter（含软件后端）")?;
        let info = adapter.get_info();
        info!(
            adapter = %info.name,
            backend = ?info.backend,
            device_type = ?info.device_type,
            "NV12 renderer adapter selected"
        );

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("voice2word-nv12"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
            },
            None,
        ))
        .context("wgpu request_device 失败")?;

        let y_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-y"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-uv"),
            size: wgpu::Extent3d {
                width: width / 2,
                height: height / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let out_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nv12-out"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        // `width` 已被 `nv12_dims_ok` 限到 16K 以内，`width * 4` 不可能回绕
        let padded_bpr = padded_bytes_per_row(width * 4);
        let out_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12-readback"),
            size: padded_bpr as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12-shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_WGSL_SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let y_view = y_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12-bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12-pll"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12-rp"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs",
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs",
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        Ok(Self {
            device,
            queue,
            y_tex,
            uv_tex,
            out_tex,
            out_buf,
            pipeline,
            bind_group,
            width,
            height,
            padded_bpr,
        })
    }

    pub fn convert(&self, nv12: &[u8]) -> Result<Vec<u8>> {
        // 走与 CPU 转换同一条布局计算：`self.width * self.height` 直接相乘
        // 在 debug 下会因回绕 panic，release 下回绕出的小尺寸会让下面切出
        // 一段错误的平面。
        let (y_size, need) = nv12_plane_layout(self.width, self.height)
            .context("NV12 渲染器尺寸非法（构造时已校验）")?;
        if nv12.len() < need {
            anyhow::bail!("NV12 buffer too small");
        }
        let y_plane = &nv12[..y_size];
        let uv_plane = &nv12[y_size..need];

        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.y_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            y_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.width),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.uv_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            uv_plane,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(self.width),
                rows_per_image: Some(self.height / 2),
            },
            wgpu::Extent3d {
                width: self.width / 2,
                height: self.height / 2,
                depth_or_array_layers: 1,
            },
        );

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nv12-enc"),
            });
        {
            let view = self
                .out_tex
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &self.out_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &self.out_buf,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.padded_bpr),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = self.out_buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv()
            .context("nv12 readback channel closed")?
            .context("nv12 map_async failed")?;

        let mapped = slice.get_mapped_range();
        let row_rgba = (self.width * 4) as usize;
        let mut out = vec![0u8; row_rgba * self.height as usize];
        for row in 0..self.height as usize {
            let src = row * self.padded_bpr as usize;
            let dst = row * row_rgba;
            out[dst..dst + row_rgba].copy_from_slice(&mapped[src..src + row_rgba]);
        }
        drop(mapped);
        self.out_buf.unmap();
        Ok(out)
    }
}

fn padded_bytes_per_row(unpadded: u32) -> u32 {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    unpadded.div_ceil(align) * align
}

pub fn convert_nv12_frame(
    renderer: Option<&Nv12Renderer>,
    nv12: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>> {
    if let Some(gpu) = renderer {
        match gpu.convert(nv12) {
            Ok(rgba) => return Ok(rgba),
            Err(err) => warn!(error = %err, "wgpu NV12 convert failed; CPU fallback"),
        }
    }
    nv12_to_rgba(nv12, width, height)
}

#[cfg(test)]
mod tests {
    use super::proxy_timeout_for;
    use super::PROXY_BASE_BUDGET_SECS;
    use super::PROXY_MAX_BUDGET_SECS;

    /// 代理转码预算必须随源时长放大。
    ///
    /// 固定 600 秒的截止期对长素材是**必然失败**：2 小时 4K 转 720p 约需
    /// 20~40 分钟（1200~2400 秒），必然被 kill，回退链再跑一次软解（同样超时），
    /// 最终「长素材在无 GPU 机器上永远拿不到代理」。
    #[test]
    fn proxy_timeout_scales_with_source_duration() {
        let short = proxy_timeout_for(Some(60.0));
        let long = proxy_timeout_for(Some(7200.0));
        assert!(
            long > short,
            "长素材的转码预算必须大于短素材（否则必然被固定截止期杀掉）"
        );
        // 2 小时素材：600 + 7200*3 = 22200 秒 ≈ 6.2 小时，远大于实测所需的 ~40 分钟。
        assert!(long.as_secs() >= 60 * 60, "2 小时素材的预算至少要 1 小时");
        // 未知时长 → 退回基准预算。
        let unknown = proxy_timeout_for(None);
        assert_eq!(unknown.as_secs(), PROXY_BASE_BUDGET_SECS);
        // 非有限时长（ffprobe 字段损坏）不得把预算算成天文数字。
        let nan = proxy_timeout_for(Some(f64::NAN));
        assert_eq!(nan.as_secs(), PROXY_BASE_BUDGET_SECS);
        // 硬上限：再长的素材也不超过 4 小时，否则转码线程会占住机器好几小时。
        let huge = proxy_timeout_for(Some(100_000.0));
        assert!(huge.as_secs() <= PROXY_MAX_BUDGET_SECS);
    }
    use super::*;

    use std::time::{Duration, Instant};

    #[test]
    fn nv12_black_is_near_black() {
        let w = 2u32;
        let h = 2u32;
        let mut nv12 = vec![16u8; 2 * 2];
        nv12.extend_from_slice(&[128, 128]);
        let rgba = nv12_to_rgba(&nv12, w, h).unwrap();
        assert_eq!(rgba.len(), 16);
        for px in rgba.chunks(4) {
            assert!(px[0] < 8 && px[1] < 8 && px[2] < 8);
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn nv12_odd_dimensions_return_error_not_panic() {
        // 宽为奇数时 UV 平面比色度采样需要的短一行：过去这里会越界 panic
        let nv12 = vec![16u8; 3 * 2 + 3];
        let err = nv12_to_rgba(&nv12, 3, 2).unwrap_err();
        assert!(err.to_string().contains("偶数"), "{err}");
        // 高为奇数同理
        assert!(nv12_to_rgba(&nv12, 2, 3).is_err());
        // 合法偶数尺寸仍照常工作
        assert!(nv12_to_rgba(&[16u8; 6 + 3], 2, 2).is_ok());
    }

    #[test]
    fn parse_height_from_ffmpeg_banner() {
        let s = "Stream #0:0: Video: h264 (High), yuv420p, 3840x2160, 30 fps";
        assert_eq!(parse_ffmpeg_height(s), Some(2160));
    }

    #[test]
    fn ring_drops_old_frames() {
        let mut ring = FrameRing::new(3);
        for i in 0..5 {
            ring.push(DecodedFrame {
                pts: i as f64,
                rgba: vec![i as u8],
            });
        }
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.latest().unwrap().pts, 4.0);
        let closest = ring.take_closest(3.2).unwrap();
        assert_eq!(closest.pts, 3.0);
    }

    #[test]
    fn proxy_path_includes_height_and_is_mp4() {
        let mgr = ProxyManager::new("ffmpeg");
        let p = mgr.proxy_path(Path::new("C:/media/demo clip.mkv"), 720);
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.contains("720p"));
        assert!(name.ends_with(".mp4"));
        assert!(!name.to_lowercase().contains("hevc"));
    }

    #[test]
    fn detect_does_not_panic() {
        let _ = HardwareProfile::detect();
    }

    #[test]
    fn vendor_classifies_common_adapter_names() {
        assert_eq!(
            Vendor::from_adapter_name("AMD Radeon RX 7900 XTX"),
            Some(Vendor::Amd)
        );
        assert_eq!(
            Vendor::from_adapter_name("Radeon 680M Graphics"),
            Some(Vendor::Amd)
        );
        assert_eq!(
            Vendor::from_adapter_name("NVIDIA GeForce RTX 4090"),
            Some(Vendor::Nvidia)
        );
        assert_eq!(
            Vendor::from_adapter_name("Intel(R) Iris(R) Xe Graphics"),
            Some(Vendor::Intel)
        );
        assert_eq!(
            Vendor::from_adapter_name("Intel(R) Arc(TM) A770"),
            Some(Vendor::Intel)
        );
        assert_eq!(
            Vendor::from_adapter_name("Microsoft Basic Render Driver"),
            None
        );
        assert_eq!(Vendor::from_adapter_name(""), None);
    }

    #[test]
    fn vendor_classification_is_case_insensitive() {
        assert_eq!(Vendor::from_adapter_name("radeon"), Some(Vendor::Amd));
        assert_eq!(Vendor::from_adapter_name("NVIDIA"), Some(Vendor::Nvidia));
        assert_eq!(
            Vendor::from_adapter_name("INTEL UHD Graphics 770"),
            Some(Vendor::Intel)
        );
    }

    #[test]
    fn wgpu_shader_converts_limited_black() {
        let w = 16u32;
        let h = 16u32;
        let mut nv12 = vec![16u8; (w * h) as usize];
        nv12.extend(std::iter::repeat_n(128u8, (w * h / 2) as usize));
        let renderer = Nv12Renderer::new(false, w, h).expect("software wgpu adapter required");
        let rgba = renderer.convert(&nv12).expect("shader convert");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        for px in rgba.chunks(4) {
            assert!(px[0] < 12 && px[1] < 12 && px[2] < 12, "got {:?}", px);
            assert_eq!(px[3], 255);
        }
    }
    // ─────────── 预览 / 代理链路硬解（路线图 P0-A3） ───────────

    fn amd_profile(hw: bool) -> HardwareProfile {
        HardwareProfile {
            backend: RenderBackend::Gpu,
            adapter_name: "AMD Radeon 680M".into(),
            hardware_decode: hw,
            force_proxy: false,
            is_discrete: false,
            vendor: Some(Vendor::Amd),
        }
    }

    fn vendor_profile(vendor: Option<Vendor>, hw: bool) -> HardwareProfile {
        HardwareProfile {
            backend: RenderBackend::Gpu,
            adapter_name: "fixture adapter".into(),
            hardware_decode: hw,
            force_proxy: false,
            is_discrete: false,
            vendor,
        }
    }

    /// `-hwaccel` 是输入侧选项，必须整段落在 `-i` 之前，且只出现一次。
    fn assert_hwaccel_before_input(args: &[String], backend: &str) {
        let flag = args
            .iter()
            .position(|a| a == "-hwaccel")
            .unwrap_or_else(|| panic!("missing -hwaccel in {args:?}"));
        let input = args
            .iter()
            .position(|a| a == "-i")
            .unwrap_or_else(|| panic!("missing -i in {args:?}"));
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some(backend),
            "unexpected backend in {args:?}"
        );
        assert!(flag < input, "-hwaccel must precede -i: {args:?}");
        assert_eq!(
            args.iter().filter(|a| *a == "-hwaccel").count(),
            1,
            "duplicate -hwaccel in {args:?}"
        );
    }

    /// AMD / Intel / NVIDIA 三家都**首选** d3d12va；d3d11va 作为回退候选由
    /// [`hwaccel_attempt_chain`] 提供（顺序在候选链测试里单独断言）。
    #[test]
    fn hwaccel_backend_prefers_d3d12va_for_known_vendors() {
        for vendor in [Vendor::Amd, Vendor::Nvidia, Vendor::Intel] {
            assert_eq!(
                vendor_profile(Some(vendor), true).hwaccel_backend(),
                Some(HwAccel::D3d12va),
                "{vendor:?} 应首选 d3d12va"
            );
        }
    }

    /// 厂商未知 / `Vendor::Other` / 无 GPU / 用户关掉开关 → 不加 `-hwaccel`。
    #[test]
    fn hwaccel_backend_is_none_without_a_known_gpu() {
        assert_eq!(vendor_profile(None, true).hwaccel_backend(), None);
        assert_eq!(
            vendor_profile(Some(Vendor::Other), true).hwaccel_backend(),
            None
        );
        // `hardware_decode=false`（用户拨到「软解预览」）优先于厂商
        assert_eq!(amd_profile(false).hwaccel_backend(), None);
        assert_eq!(HardwareProfile::cpu_only().hwaccel_backend(), None);
        assert_eq!(
            HardwareProfile::detect().hwaccel_backend().is_some(),
            HardwareProfile::detect().hardware_decode
        );
    }

    /// 硬解开启时，代理参数里必须带上**所选**后端，且 `-hwaccel` 落在 `-i` 之前。
    /// 档案驱动的默认选择是 d3d12va；显式传入回退候选 d3d11va 时同样成立。
    #[test]
    fn proxy_args_emits_the_selected_hwaccel_backend() {
        for vendor in [Vendor::Amd, Vendor::Intel] {
            let profile = vendor_profile(Some(vendor), true);
            assert!(profile.decode_policy().try_hwaccel, "{vendor:?} 应开启硬解");
            let args = proxy_args(
                Path::new("C:/media/demo.mp4"),
                Path::new("C:/tmp/out.mp4"),
                720,
                profile.hwaccel_backend(),
                proxy_encode_threads(),
            );
            assert_hwaccel_before_input(&args, "d3d12va");
            assert!(args.iter().any(|a| a == "-c:v"));
            assert!(args.iter().any(|a| a == "libx264"));
        }

        // 回退候选（d3d11va）也要能原样拼进命令
        let fallback = proxy_args(
            Path::new("C:/media/demo.mp4"),
            Path::new("C:/tmp/out.mp4"),
            720,
            Some(HwAccel::D3d11va),
            proxy_encode_threads(),
        );
        assert_hwaccel_before_input(&fallback, "d3d11va");
    }

    /// 软解路径（无 GPU / 用户关开关）逐参数等价于历史版本：整条命令里没有 `-hwaccel`。
    #[test]
    fn proxy_args_never_mentions_hwaccel_on_software_paths() {
        let soft = proxy_args(
            Path::new("C:/media/demo.mkv"),
            Path::new("C:/tmp/out.mp4"),
            540,
            None,
            proxy_encode_threads(),
        );
        assert!(
            !soft.iter().any(|a| a.contains("hwaccel")),
            "软解参数不得出现 -hwaccel: {soft:?}"
        );
        assert_eq!(soft[0], "-hide_banner");
        assert_eq!(soft[3], "-y");
        assert_eq!(soft[4], "-i", "-i 必须紧跟通用开关之后");

        // 关掉硬件解码的档案同样产出软解参数
        assert_eq!(
            amd_profile(false).hwaccel_backend(),
            None,
            "hardware_decode=false 时后端必须是 None"
        );
        assert_eq!(
            proxy_args(
                Path::new("C:/media/demo.mkv"),
                Path::new("C:/tmp/out.mp4"),
                540,
                amd_profile(false).hwaccel_backend(),
                proxy_encode_threads(),
            ),
            soft,
        );
    }

    /// 硬解 / 软解两条路径只差 `-hwaccel <backend>` 两个 token，其余完全一致。
    /// 回退实现正是靠「把后端置 None 再跑一次」，这条断言就是回退正确性的证明。
    #[test]
    fn software_retry_differs_from_hwaccel_only_by_the_hwaccel_flag() {
        let src = Path::new("C:/media/demo.mkv");
        let out = Path::new("C:/tmp/out.mp4");
        let threads = proxy_encode_threads();
        let hw = proxy_args(src, out, 720, Some(HwAccel::D3d11va), threads);
        let sw = proxy_args(src, out, 720, None, threads);

        let mut stripped = hw.clone();
        let at = stripped
            .iter()
            .position(|a| a == "-hwaccel")
            .expect("hardware args contain -hwaccel");
        stripped.drain(at..at + 2);
        assert_eq!(stripped, sw, "去掉 -hwaccel 后必须与软解参数逐参数相同");
    }

    /// 硬解失败必须自动回退：先换另一个硬件后端，再兜底软解——本任务最重要的健壮性约束。
    ///
    /// 用假执行器覆盖「怎么跑」之外的全部决策逻辑；真实 ffmpeg 调用只在
    /// `real_ffmpeg_proxy_runs_with_and_without_hwaccel` 里端到端验证。
    #[test]
    fn hardware_failure_falls_back_through_the_candidate_chain() {
        // d3d12va 失败 → d3d11va 失败 → 软解成功：三次尝试，顺序即候选链
        let mut attempts: Vec<Option<HwAccel>> = Vec::new();
        let ok = transcode_with_fallback(
            Some(HwAccel::D3d12va),
            Box::new(|backend| {
                attempts.push(backend);
                Ok(backend.is_none())
            }),
        )
        .expect("fallback must not surface an error");
        assert!(ok, "回退后的软解尝试成功，整体应判成功");
        assert_eq!(
            attempts,
            vec![Some(HwAccel::D3d12va), Some(HwAccel::D3d11va), None],
            "必须按 d3d12va → d3d11va → 软解 的顺序各尝试一次"
        );
    }

    /// 第一候选失败、第二候选成功时**不得**再多跑一次软解（避免凭空多一次全片转码）。
    #[test]
    fn second_hardware_candidate_success_stops_before_software() {
        let mut attempts: Vec<Option<HwAccel>> = Vec::new();
        let ok = transcode_with_fallback(
            Some(HwAccel::D3d12va),
            Box::new(|backend| {
                attempts.push(backend);
                Ok(backend == Some(HwAccel::D3d11va))
            }),
        )
        .unwrap();
        assert!(ok);
        assert_eq!(
            attempts,
            vec![Some(HwAccel::D3d12va), Some(HwAccel::D3d11va)],
            "d3d11va 成功后不应再跑软解"
        );
    }

    /// 硬解一次成功时**不得**多跑一遍软解（避免凭空多一次全片转码）。
    #[test]
    fn successful_hardware_decode_does_not_retry() {
        let mut attempts: Vec<Option<HwAccel>> = Vec::new();
        let ok = transcode_with_fallback(
            Some(HwAccel::D3d11va),
            Box::new(|backend| {
                attempts.push(backend);
                Ok(true)
            }),
        )
        .unwrap();
        assert!(ok);
        assert_eq!(attempts, vec![Some(HwAccel::D3d11va)]);
    }

    /// 软解路径（后端本来就是 None）失败时不再重试——重试没有意义，直接报错。
    #[test]
    fn software_only_path_does_not_retry_and_reports_failure() {
        let mut attempts: Vec<Option<HwAccel>> = Vec::new();
        let ok = transcode_with_fallback(
            None,
            Box::new(|backend| {
                attempts.push(backend);
                Ok(false)
            }),
        )
        .unwrap();
        assert!(!ok, "软解也失败时整体判失败，交给调用方 bail!");
        assert_eq!(attempts, vec![None], "不得重试");
    }

    /// 全部候选（两个硬件 + 软解）都失败：跑完候选链就收手，不得无限循环。
    #[test]
    fn all_candidates_failing_stops_after_the_chain() {
        let mut attempts: Vec<Option<HwAccel>> = Vec::new();
        let ok = transcode_with_fallback(
            Some(HwAccel::D3d12va),
            Box::new(|backend| {
                attempts.push(backend);
                Ok(false)
            }),
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(
            attempts,
            vec![Some(HwAccel::D3d12va), Some(HwAccel::D3d11va), None],
            "候选链长度有界，跑完即停"
        );
    }

    /// 执行器自身报错（启动失败）时把错误原样上抛，不再吞掉去重试。
    #[test]
    fn attempt_error_is_propagated() {
        let mut calls = 0;
        let err = transcode_with_fallback(
            Some(HwAccel::D3d11va),
            Box::new(|_| {
                calls += 1;
                Err(anyhow::anyhow!("spawn failed"))
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("spawn failed"));
        assert_eq!(calls, 1);
    }

    /// 用**真实 ffmpeg** 跑一遍生产代码 `ensure_proxy`，硬解开 / 关两条路径都要
    /// 产出可播放的代理。前面几条测的是「参数构造与回退决策」，这条测的是
    /// 「参数真的能喂进 ffmpeg 并转出文件」——`-hwaccel` 放在错误位置、
    /// 或 `-vf scale` 与硬解不兼容，都只有真跑一次才会暴露。
    ///
    /// 本机（AMD Radeon 680M）能解析出 ffmpeg 时会实际执行；解析不到时静默跳过，
    /// 保证在没装 ffmpeg 的机器上测试套件依然全绿。
    #[test]
    fn real_ffmpeg_proxy_runs_with_and_without_hwaccel() {
        let ffmpeg = crate::utils::AppConfig::load_from_file("config.toml")
            .map(|c| crate::utils::AppConfig::resolve_path(&c.paths.ffmpeg))
            .unwrap_or_default();
        if !ffmpeg.is_file() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("v2w_hwaccel_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir e2e");
        let src = dir.join("clip.mp4");

        // 3 秒 640x360 的 H.264 素材，够 ensure_proxy 走完整条转码分支
        let gen = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc2=size=640x360:rate=15:duration=3")
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&src)
            .status();
        if !gen.map(|s| s.success()).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(&dir);
            return; // 生成素材失败（缺 lavfi 等）：跳过而不是误报
        }

        // 硬解实例（本机是 AMD 核显 → d3d11va）
        let hw_mgr = ProxyManager::with_hwaccel(&ffmpeg, Some(HwAccel::D3d11va));
        let hw = hw_mgr.ensure_proxy(&src, 240).expect("hardware proxy");
        assert!(
            hw.metadata().map(|m| m.len() > 1024).unwrap_or(false),
            "硬解代理未产出有效文件: {hw:?}"
        );

        // 纯软解实例——目标高度不同以绕开「已有代理」缓存
        let sw_mgr = ProxyManager::new(&ffmpeg);
        assert_eq!(sw_mgr.hwaccel(), None, "默认构造必须是纯软解");
        let sw = sw_mgr.ensure_proxy(&src, 238).expect("software proxy");
        assert!(
            sw.metadata().map(|m| m.len() > 1024).unwrap_or(false),
            "软解代理未产出有效文件: {sw:?}"
        );

        let _ = std::fs::remove_file(&hw);
        let _ = std::fs::remove_file(&sw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 硬解策略挂在 [`ProxyManager`] 实例上，且默认必须是纯软解——加硬解是
    /// 显式选择，不能让「没改过调用点的旧代码」突然开始用 GPU。
    #[test]
    fn proxy_manager_defaults_to_software_and_can_opt_into_hwaccel() {
        let off = ProxyManager::new("ffmpeg");
        assert_eq!(off.hwaccel(), None);

        let on = ProxyManager::with_hwaccel("ffmpeg", Some(HwAccel::D3d11va));
        assert_eq!(on.hwaccel(), Some(HwAccel::D3d11va));

        // 档案驱动的构造：AMD 核显 + hardware_decode=true → 首选 d3d12va
        let from_amd = ProxyManager::with_profile("ffmpeg", &amd_profile(true));
        assert_eq!(from_amd.hwaccel(), Some(HwAccel::D3d12va));
        // 用户关掉硬解（hardware_decode=false）→ 纯软解
        assert_eq!(
            ProxyManager::with_profile("ffmpeg", &amd_profile(false)).hwaccel(),
            None
        );
        // 无 GPU / 厂商未知 → 纯软解
        assert_eq!(
            ProxyManager::with_profile("ffmpeg", &vendor_profile(None, true)).hwaccel(),
            None
        );
        assert_eq!(
            ProxyManager::with_profile("ffmpeg", &HardwareProfile::cpu_only()).hwaccel(),
            None
        );

        // 实例策略要真的反映进命令：默认实例拼出的参数里没有 -hwaccel
        let soft_args = proxy_args(
            Path::new("C:/media/demo.mp4"),
            Path::new("C:/tmp/out.mp4"),
            720,
            off.hwaccel(),
            proxy_encode_threads(),
        );
        let hw_args = proxy_args(
            Path::new("C:/media/demo.mp4"),
            Path::new("C:/tmp/out.mp4"),
            720,
            on.hwaccel(),
            proxy_encode_threads(),
        );
        assert!(!soft_args.iter().any(|a| a.contains("hwaccel")));
        assert_hwaccel_before_input(&hw_args, "d3d11va");
    }

    // ─────────── 让路模式开关 ───────────

    /// 全局开关必须能读回写入的值（它决定所有子进程是否降优先级）。
    #[test]
    fn yield_to_desktop_flag_roundtrips() {
        // 保存现场：这个开关是进程级全局量，测试之间会互相影响
        let before = yield_to_desktop();

        set_yield_to_desktop(true);
        assert!(yield_to_desktop(), "写入 true 后应读回 true");
        set_yield_to_desktop(false);
        assert!(!yield_to_desktop(), "写入 false 后应读回 false");

        set_yield_to_desktop(before);
    }

    // ─────────── 硬解候选链（D3d12va 启用） ───────────

    /// 每个变体都要给出稳定的 FFmpeg 参数与人类可读标签。
    #[test]
    fn hwaccel_ffmpeg_args_and_labels_are_stable() {
        assert_eq!(HwAccel::D3d11va.as_ffmpeg_arg(), "d3d11va");
        assert_eq!(HwAccel::D3d12va.as_ffmpeg_arg(), "d3d12va");
        assert_eq!(HwAccel::D3d11va.flag(), HwAccel::D3d11va.as_ffmpeg_arg());
        assert_eq!(HwAccel::D3d12va.flag(), HwAccel::D3d12va.as_ffmpeg_arg());
        assert_eq!(HwAccel::D3d11va.label(), "D3D11VA");
        assert_eq!(HwAccel::D3d12va.label(), "D3D12VA");
        assert_ne!(
            HwAccel::D3d11va.as_ffmpeg_arg(),
            HwAccel::D3d12va.as_ffmpeg_arg(),
            "两个后端不能映射到同一个 FFmpeg 参数"
        );
    }

    /// 偏好表必须恰好覆盖全部变体，且 d3d12va 排在最前——这条同时是
    /// 「没有定义却不被使用的变体」的语义检查（对应 dead_code 关注的场景）。
    #[test]
    fn hwaccel_preference_covers_every_variant_with_d3d12va_first() {
        assert_eq!(HWACCEL_PREFERENCE.len(), 2);
        assert_eq!(HWACCEL_PREFERENCE[0], HwAccel::D3d12va);
        assert!(HWACCEL_PREFERENCE.contains(&HwAccel::D3d11va));
        assert!(HWACCEL_PREFERENCE.contains(&HwAccel::D3d12va));
    }

    /// 候选链顺序：首选 → 其余硬件候选 → 软解兜底；纯软解档案只有一步。
    #[test]
    fn hwaccel_attempt_chain_orders_fallbacks() {
        assert_eq!(
            hwaccel_attempt_chain(Some(HwAccel::D3d12va)),
            vec![Some(HwAccel::D3d12va), Some(HwAccel::D3d11va), None]
        );
        // 显式指定 d3d11va 时，d3d12va 作为第二候选补齐，末尾仍是软解
        assert_eq!(
            hwaccel_attempt_chain(Some(HwAccel::D3d11va)),
            vec![Some(HwAccel::D3d11va), Some(HwAccel::D3d12va), None]
        );
        // 无 GPU / 厂商未知 / 用户关开关：只有软解一步
        assert_eq!(hwaccel_attempt_chain(None), vec![None]);
    }

    /// 候选链不得出现重复项，硬件候选不超过偏好表大小，且软解恒在末尾。
    #[test]
    fn hwaccel_attempt_chain_is_deduplicated_with_software_last() {
        for preferred in [None, Some(HwAccel::D3d11va), Some(HwAccel::D3d12va)] {
            let chain = hwaccel_attempt_chain(preferred);
            assert_eq!(chain.last().copied(), Some(None), "软解必须是最后一步");
            let mut hardware: Vec<HwAccel> = chain.iter().flatten().copied().collect();
            let count = hardware.len();
            hardware.sort_by_key(|hw| hw.as_ffmpeg_arg());
            hardware.dedup();
            assert_eq!(hardware.len(), count, "候选链不得重复: {chain:?}");
            assert!(
                count <= HWACCEL_PREFERENCE.len(),
                "硬件候选数不得超过偏好表: {chain:?}"
            );
        }
    }

    /// 日志名：硬件后端用 FFmpeg 参数名，软解统一显示 `software`。
    #[test]
    fn hwaccel_name_labels_software_and_hardware() {
        assert_eq!(hwaccel_name(None), "software");
        assert_eq!(hwaccel_name(Some(HwAccel::D3d11va)), "d3d11va");
        assert_eq!(hwaccel_name(Some(HwAccel::D3d12va)), "d3d12va");
    }

    /// 假的长跑命令：`ping` / `sleep` 都是**直接子进程**，被 kill 后不会留下还在
    /// 往管道里写字的孙子进程，因此超时回收路径是干净的（与 `ffmpeg.rs` 同款手法）。
    #[cfg(target_os = "windows")]
    fn 长跑假命令() -> Command {
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"]);
        cmd
    }

    #[cfg(not(target_os = "windows"))]
    fn 长跑假命令() -> Command {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        cmd
    }

    /// 缺陷回归：代理链路的子进程调用必须带截止期，超时要在预算内杀干净并报错。
    ///
    /// `ensure_proxy` 的两处子进程调用（转码、分辨率探测）过去是裸的 `status()` /
    /// `output()`，没有任何上界：坏素材会让后台转码线程永远挂住、UI 一直转圈。
    /// 现在两处都复用 `ffmpeg.rs` 的 `run_with_deadline`，这条测试钉住该执行器在本
    /// 模块里的可用性与超时语义——1 秒预算内返回 `Err`，且错误信息说明是超时。
    #[test]
    fn media_pipeline_subprocess_is_killed_after_deadline() {
        let mut cmd = 长跑假命令();
        let started = Instant::now();
        let err = run_with_deadline(&mut cmd, Duration::from_millis(200))
            .expect_err("超过截止期的子进程必须报错，不能等它自然结束");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "超时路径必须在预算内返回，实际耗时 {elapsed:?}"
        );
        assert!(
            err.to_string().contains("超时"),
            "错误信息应说明超时: {err}"
        );
    }

    /// 锁粒度回归：转码闸门按输出路径拆分，且命中缓存的快路径不排队等锁。
    ///
    /// 老实现是一把罩住整段转码 + 回退链的单锁——A 的 4K 代理在转时，B 的请求
    /// （哪怕结果早就在缓存里）也只能排队。这条测试同时钉住两件事：
    /// 1) 两个不同输出拿到的是两把不同的闸门，同一输出复用同一把，且没人持有后
    ///    会被回收（表不会随预览过的文件无限增长）；
    /// 2) 主线程攥着某输出的闸门时，另一个线程里命中缓存的那次 `ensure_proxy`
    ///    仍必须立刻返回——它走的是无锁快路径，不进任何闸门。
    #[test]
    fn 代理转码闸门按输出路径拆分且缓存快路径不等锁() {
        // 假 ffmpeg 路径：探测会立刻失败（spawn 失败 → None），测试不需要真 ffmpeg。
        let mgr = Arc::new(ProxyManager::new("voice2word-test-ffmpeg-missing"));
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let src =
            std::env::temp_dir().join(format!("v2w_gate_{}_{}.mp4", std::process::id(), nanos));
        let out = mgr.proxy_path(&src, 720);
        let other = mgr.proxy_path(&src, 480);
        assert_ne!(out, other, "前提：不同目标高度对应不同输出文件");

        // 1) 闸门按输出拆分 + 没人持有时回收
        let gate_a = mgr.transcode_gate(&out);
        let gate_b = mgr.transcode_gate(&other);
        assert!(
            !Arc::ptr_eq(&gate_a, &gate_b),
            "不同输出不应共用同一把锁（那正是要被修掉的全局锁粒度）"
        );
        let gate_c = mgr.transcode_gate(&other);
        assert!(
            Arc::ptr_eq(&gate_b, &gate_c),
            "同一输出必须复用同一把闸门，否则两条请求会并发写同一个文件"
        );
        drop(gate_a);
        drop(gate_b);
        drop(gate_c);
        let gate_d = mgr.transcode_gate(&out);
        assert_eq!(
            mgr.in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len(),
            1,
            "无人持有的闸门应被回收，表不能随预览过的文件无限增长"
        );
        drop(gate_d);

        // 2) 缓存命中不等锁：先把 out 写成「完整代理」
        //
        // 判据要求文件头里有 `moov`（faststart 完成后 moov 会在文件开头）。
        // 只写全零字节的假文件会被判为半成品，缓存快路径根本不会命中。
        let mut fake = vec![0u8; 4096];
        fake[16..20].copy_from_slice(b"moov");
        std::fs::write(&out, &fake).expect("写入假代理文件");
        assert!(proxy_ready(&out), "前提：带 moov 的假代理应被判定为可用");

        // 先把闸门绑定到局部变量：`transcode_gate` 返回的 Arc 若是临时值，语句结束后即释放，
        // guard 借用它去 lock 会触发 E0716（temporary value dropped while borrowed）。
        let gate_held = mgr.transcode_gate(&out);
        let guard = gate_held.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let mgr = Arc::clone(&mgr);
            let src = src.clone();
            std::thread::spawn(move || {
                let _ = tx.send(mgr.ensure_proxy(&src, 720));
            })
        };
        let got = rx.recv_timeout(Duration::from_millis(500));
        drop(guard);
        let _ = worker.join();
        let got = got.expect("缓存命中的快路径必须立即返回，不能等待转码闸门");
        assert_eq!(
            got.expect("缓存存在时必须直接复用"),
            out,
            "命中缓存时返回的必须是现成的代理文件"
        );

        let _ = std::fs::remove_file(&out);
    }
}

#[cfg(test)]
mod media_pipeline_robustness_tests {
    use super::{nv12_dims_ok, nv12_plane_layout, proxy_ready, settle_proxy_attempt, NV12_MAX_DIM};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("v2w_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 尺寸/行距边界：NV12 的 Y 平面是 `w*h`、总需求是 1.5 倍；奇数尺寸、
    /// 零尺寸与天文数字尺寸都必须被判定为非法（旧代码在 GPU 路径直接用
    /// `width * height` 与 `width * 4`，debug 下会乘法回绕 panic）。
    #[test]
    fn nv12_layout_rejects_odd_zero_and_oversized_dimensions() {
        assert_eq!(nv12_plane_layout(2, 2), Some((4, 6)));
        assert_eq!(nv12_plane_layout(1920, 1080), Some((2_073_600, 3_110_400)));

        // 奇数尺寸：UV 平面比色度采样少一列/一行，下标会越界
        assert_eq!(nv12_plane_layout(3, 2), None);
        assert_eq!(nv12_plane_layout(2, 3), None);
        // 零尺寸没有意义，也要拒绝（旧写法会返回空图而不是报错）
        assert_eq!(nv12_plane_layout(0, 2), None);
        assert_eq!(nv12_plane_layout(2, 0), None);
        // 天文数字：挡在 u32 乘法回绕与 wgpu 超限纹理之前
        assert_eq!(nv12_plane_layout(u32::MAX - 1, u32::MAX - 1), None);
        assert_eq!(nv12_plane_layout(NV12_MAX_DIM + 2, 2), None);
        assert!(nv12_dims_ok(NV12_MAX_DIM, NV12_MAX_DIM));
        assert!(!nv12_dims_ok(NV12_MAX_DIM + 2, 2));
        assert!(!nv12_dims_ok(16, 15));
    }

    /// 缺陷回归：一次失败的代理尝试（退出码非零 **或** 执行器报错——超时被杀 /
    /// 启动失败）必须删掉已经写出的截断 mp4。`proxy_ready` 只判「存在且 >1KB」，
    /// 所以残留的半成品会被下一次 `ensure_proxy` 当成合格代理直接复用——预览对
    /// 同一个素材会一直坏到临时目录被清空。
    #[test]
    fn failed_proxy_attempt_removes_partial_output() {
        let dir = temp_dir("settle_proxy");
        let out = dir.join("proxy.mp4");

        // 前提：半成品（无 moov 的截断 mp4）**不得**被判为可用代理。
        //
        // 旧判据只看「>1KB」，于是转码被超时 kill 后留下的几十 MB 半截文件会被
        // 当成合格代理反复复用，预览一直坏到用户换素材——这是本节要防的核心问题。
        std::fs::write(&out, vec![0u8; 4096]).expect("写入半成品");
        assert!(
            !proxy_ready(&out),
            "无 moov 的半成品必须判为不可用（否则会被当成合格代理复用）"
        );

        // 退出码非零：清理
        let ok = settle_proxy_attempt(&out, Ok(false)).expect("非零退出码不上抛错误");
        assert!(!ok);
        assert!(!out.exists(), "退出码非零留下的截断 mp4 必须被清理");

        // 执行器报错（超时被杀 / 启动失败）：同样清理，且错误原样上抛
        std::fs::write(&out, vec![0u8; 4096]).expect("写入半成品");
        let err = settle_proxy_attempt(&out, Err(anyhow::anyhow!("子进程 ffmpeg 解码停滞超时")))
            .expect_err("执行器报错必须上抛");
        assert!(
            err.to_string().contains("超时"),
            "错误信息应原样保留: {err}"
        );
        assert!(
            !out.exists(),
            "超时残留的半成品必须被清理，否则会被当成合格代理"
        );

        // 成功时必须保留刚转好的代理（这里写成带 moov 的「完整」形态，顺带证明
        // 成功产物确实会被判为可用）。
        let mut complete = vec![0u8; 4096];
        complete[16..20].copy_from_slice(b"moov");
        std::fs::write(&out, &complete).expect("写入代理");
        assert!(settle_proxy_attempt(&out, Ok(true)).expect("成功路径不上抛"));
        assert!(out.exists(), "成功时不得删除代理文件");
        assert!(proxy_ready(&out), "成功产物必须被判为可用代理");

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
