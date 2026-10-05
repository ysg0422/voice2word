//! Unified media pipeline policy (Jianying-style).
//!
//! One NV12 WGSL shader is used for both GPU adapters and wgpu's CPU/WARP
//! software backend. Preview always prefers an H.264 proxy on machines without
//! a discrete/integrated GPU; export / independent playback keeps the original.

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tracing::{info, warn};

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
}

#[derive(Debug, Clone, Copy)]
pub struct DecodePolicy {
    pub try_hwaccel: bool,
    pub software_threads: u32,
    /// 让路模式：预览解码进程降到低于正常优先级，避免与桌面合成器抢 GPU/CPU
    pub yield_to_desktop: bool,
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
                adapter_name: info.name,
                hardware_decode: true,
                force_proxy: false,
                is_discrete: matches!(info.device_type, wgpu::DeviceType::DiscreteGpu),
            };
        }

        warn!("No hardware GPU adapter found; using FFmpeg software decode + wgpu software rasterizer");
        Self {
            backend: RenderBackend::CpuSoftware,
            adapter_name: "software (wgpu WARP / lavapipe)".into(),
            hardware_decode: false,
            force_proxy: true,
            is_discrete: false,
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
        }
    }

    pub fn use_gpu_pipeline(&self) -> bool {
        self.backend == RenderBackend::Gpu
    }

    pub fn decode_policy(&self, yield_to_desktop: bool) -> DecodePolicy {
        DecodePolicy {
            try_hwaccel: self.hardware_decode,
            software_threads: 0, // ffmpeg 0 = auto
            yield_to_desktop,
        }
    }
}

/// H.264 proxy generator. Proxy files are never HEVC.
pub struct ProxyManager {
    ffmpeg_path: PathBuf,
    lock: Mutex<()>,
    height_cache: Mutex<HashMap<PathBuf, (FileSignature, Option<u32>)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified_secs: u64,
}

fn file_signature(path: &Path) -> FileSignature {
    match std::fs::metadata(path) {
        Ok(meta) => FileSignature {
            len: meta.len(),
            modified_secs: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        },
        Err(_) => FileSignature {
            len: 0,
            modified_secs: 0,
        },
    }
}

impl ProxyManager {
    pub fn new<P: AsRef<Path>>(ffmpeg_path: P) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            lock: Mutex::new(()),
            height_cache: Mutex::new(HashMap::new()),
        }
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
        std::env::temp_dir().join(format!(
            "voice2word_proxy_{}_{}_{}_{}p.mp4",
            safe, signature.len, signature.modified_secs, height
        ))
    }

    pub fn existing_proxy<P: AsRef<Path>>(&self, source: P, height: u32) -> Option<PathBuf> {
        let source = source.as_ref();
        if let Some(h) = self.probe_height(source) {
            if h <= height {
                return Some(source.to_path_buf());
            }
        }
        let out = self.proxy_path(source, height);
        if out.exists() && out.metadata().map(|m| m.len() > 1024).unwrap_or(false) {
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
                info!(height = h, "source already within proxy resolution; skip transcode");
                return Ok(source.to_path_buf());
            }
        }

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let out = self.proxy_path(source, height);
        if out.exists() && out.metadata().map(|m| m.len() > 1024).unwrap_or(false) {
            return Ok(out);
        }

        info!(
            source = %source.display(),
            height,
            out = %out.display(),
            "generating H.264 proxy (libx264, never HEVC)"
        );

        let vf = format!("scale=-2:{}", height);
        let mut cmd = Command::new(&self.ffmpeg_path);
        apply_background_priority(&mut cmd);
        let encode_threads = proxy_encode_threads();
        info!(
            threads = encode_threads,
            "proxy transcode runs at below-normal priority with a bounded thread budget"
        );
        let status = cmd
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(source)
            .args([
                "-vf",
                &vf,
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "28",
                "-pix_fmt",
                "yuv420p",
                "-profile:v",
                "baseline",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-movflags",
                "+faststart",
            ])
            .arg("-threads")
            .arg(encode_threads.to_string())
            .arg(&out)
            .status()
            .with_context(|| format!("启动代理生成失败: {:?}", self.ffmpeg_path))?;

        if !status.success() {
            let _ = std::fs::remove_file(&out);
            anyhow::bail!("FFmpeg 代理生成失败");
        }
        Ok(out)
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
        let output = cmd.arg("-i").arg(source).output().ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let height = parse_ffmpeg_height(&stderr);
        self.height_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source.to_path_buf(), (signature, height));
        height
    }
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
        info!(limit_percent = limit, "GPU 限速已开启：按占空比给桌面留出 GPU 空窗");
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
            let module = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
            if module.is_null() {
                return None;
            }
            let suspend = GetProcAddress(module, b"NtSuspendProcess\0".as_ptr());
            let resume = GetProcAddress(module, b"NtResumeProcess\0".as_ptr());
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

/// CPU fallback of the same BT.601 limited-range conversion (R,G,B,A).
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    let y_size = w.checked_mul(h).context("nv12 size overflow")?;
    let need = y_size + y_size / 2;
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
        anyhow::ensure!(width % 2 == 0 && height % 2 == 0, "NV12 size must be even");
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
            if prefer_gpu { ty } else { 4u8.saturating_sub(ty.min(4)) }
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
        let y_size = (self.width * self.height) as usize;
        let need = y_size + y_size / 2;
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
    ((unpadded + align - 1) / align) * align
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
    use super::*;

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
    fn wgpu_shader_converts_limited_black() {
        let w = 16u32;
        let h = 16u32;
        let mut nv12 = vec![16u8; (w * h) as usize];
        nv12.extend(std::iter::repeat(128u8).take((w * h / 2) as usize));
        let renderer = Nv12Renderer::new(false, w, h).expect("software wgpu adapter required");
        let rgba = renderer.convert(&nv12).expect("shader convert");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        for px in rgba.chunks(4) {
            assert!(px[0] < 12 && px[1] < 12 && px[2] < 12, "got {:?}", px);
            assert_eq!(px[3], 255);
        }
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
}
