//! Whisper 语音识别引擎封装 (基于 whisper-cli)

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{error, info};

use crate::subtitle::Segment;
use crate::utils::TempPathGuard;

pub struct WhisperEngine {
    cli_path: PathBuf,
    model_path: PathBuf,
    vad_model_path: Option<PathBuf>,
    threads: u32,
    processors: u32,
    use_gpu: bool,
    /// GPU 占用上限（百分比，100 = 不限速）。小于 100 时子进程按占空比被挂起/恢复，
    /// 给桌面合成器留出真正的 GPU 空窗；让路模式只降优先级，压不住 GPU 队列。
    gpu_limit_percent: u32,
    /// 禁用温度回退 (-nf)： turbo 等大模型同样跳过回退重解码以提速，
    /// 由此损失的少数低置信片段由管线的置信度二段重解码救回
    no_fallback: bool,
    /// 跨句自注意力上下文 token 上限 (-mc)
    max_context: u32,
    /// 是否收集 token 级概率：仅置信度救场开启时需要 -ojf 全量 JSON；
    /// 救场关闭时用轻量 -oj，避免为已禁用的功能支付 JSON 体积与解析成本
    need_token_probs: AtomicBool,
    vad_threshold_bits: AtomicU64,
    cancel: Arc<AtomicBool>,
    active_children: Arc<Mutex<Vec<ChildEntry>>>,
}

/// 子进程登记表里的一行：`(pid, 本次登记的单调序号)`。
///
/// 为什么除了 PID 还要存序号：PID 会被系统复用。若注销时只按 PID 匹配，
/// 一个刚退出的子进程的守卫可能把「复用了同一 PID 的新子进程」那一行顺手摘掉，
/// 新进程从此失去终止保护。带上序号后，每个守卫只会摘掉自己登记的那一行。
/// `pub(crate)` 是为了让 `sensevoice` 引擎复用同一份守卫，而不是各写一份。
pub(crate) type ChildEntry = (u32, u64);

/// 全局单调递增的登记序号，给每个 [`ChildPidGuard`] 发一个唯一标识。
static CHILD_REGISTRATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// 取出并清空登记表中**仍然有效**的子进程 PID，供 `cancel()` 逐个强杀。
///
/// 为什么单拎成函数：它是「误杀」这条风险链的收口点。子进程一旦被 `wait()`
/// 回收，[`ChildPidGuard::disarm`] 就会立刻把它那一行摘掉；因此这里返回的
/// 只可能是**尚未回收**的 PID。`cancel()` 只对着这份名单发信号，
/// 就不存在「拿着一个已归还系统、可能已被复用给无关进程的号码去 kill」的可能。
/// 抽出来也便于在单测里直接断言这份名单的内容，而不必真的去 kill 进程。
///
/// 持锁范围只有 `drain` 这一次：`kill_process_tree` 在锁释放之后才逐个执行，
/// 避免在持锁期间做可能阻塞的进程操作（否则会拖住同引擎的其它登记/注销）。
pub(crate) fn take_live_pids(registry: &Arc<Mutex<Vec<ChildEntry>>>) -> Vec<u32> {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .map(|(pid, _)| pid)
        .collect()
}

/// RAII 守卫：登记子进程 PID，并在守卫离开作用域时把这一行从登记表摘掉。
///
/// 为什么不能靠「在每个 return / `?` 前补一行 retain」：登记点与正常注销点之间
/// 夹着多条提前退出路径——`child.stdin.take()...?`、`child.wait()...?`、
/// 取消分支、panic 展开。手写补丁只要漏掉任意一条，PID 就永久留在表里；
/// 内存占用可以忽略，但等系统把这个 PID 复用给无关进程后，`cancel()` 里的
/// `kill_process_tree` 就会照着这个陈旧数字去误杀它。
/// 把注销绑到栈变量的生命周期上，任何退出方式都覆盖，且新增分支时不必再记。
/// （与 `src/utils/temp_guard.rs` 的 `TempPathGuard` 同一思路。）
///
/// 唯一的例外是「子进程已被 `wait()` 回收」这一刻，必须比 `Drop` 更早摘除——
/// 见 [`ChildPidGuard::disarm`]。
pub(crate) struct ChildPidGuard {
    registry: Arc<Mutex<Vec<ChildEntry>>>,
    /// 本次登记的唯一序号，`Drop` 只按它匹配，避免误摘同 PID 的他人条目
    token: u64,
}

impl ChildPidGuard {
    /// 把 `pid` 登记进 `registry`，并返回负责在离开作用域时摘除它的守卫。
    pub(crate) fn register(registry: Arc<Mutex<Vec<ChildEntry>>>, pid: u32) -> Self {
        let token = CHILD_REGISTRATION_SEQ.fetch_add(1, Ordering::Relaxed);
        registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((pid, token));
        Self { registry, token }
    }

    /// 子进程被 `wait()` 回收后**立刻**调用：把本行从登记表摘掉。
    ///
    /// 为什么不能等 `Drop` 兜底：`wait()` 一返回，这个进程的 PID 就归还给了
    /// 操作系统，随时可能被分配给无关进程。若这一行还留在表里，用户此刻点
    /// 「终止转写」触发的 `cancel()` 就会照着这个陈旧数字 `kill_process_tree`，
    /// 误杀无关进程——`Drop` 要到函数返回才执行，中间还隔着 join 读取线程、
    /// 解析 JSON 等大段逻辑，窗口很长。在 `wait()` 返回处立刻摘除，把这段危险
    /// 窗口压缩到紧随其后的两条指令。
    ///
    /// 在 Windows 上这道防线是**完全闭合**的：PID 只有在进程对象的最后一个句柄
    /// 关闭后才可能被复用，而进程句柄由 `child` 持有、要到函数返回（在守卫
    /// `Drop` 之后）才关闭；本方法让注销更早发生，因此登记表里绝不会出现
    /// 「PID 已可被复用」的条目。非 Windows 平台没有句柄这一层保护，
    /// 这里把窗口从「函数剩余全部逻辑」缩到最小。
    ///
    /// 只按自己的 `token` 匹配，因此不会误摘后来者复用同一 PID 值的条目
    /// （「误摘」方向的防线，见 [`ChildPidGuard::register`] 的序号说明）。
    pub(crate) fn disarm(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(_, token)| *token != self.token);
    }
}

impl Drop for ChildPidGuard {
    fn drop(&mut self) {
        // 与 disarm 同一套摘除逻辑，保证「提前退出 / panic 展开」也走同一条路径。
        self.disarm();
    }
}

#[derive(Debug, Deserialize)]
struct WhisperJsonOutput {
    result: Option<WhisperResultMeta>,
    transcription: Option<Vec<WhisperJsonSegment>>,
}

#[derive(Debug, Deserialize)]
struct WhisperResultMeta {
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhisperJsonSegment {
    offsets: Option<WhisperOffsets>,
    text: Option<String>,
    /// -ojf 完整输出的 token 级明细，每 token 带概率 p；据此自算 avg_logprob
    #[serde(default)]
    tokens: Vec<WhisperJsonToken>,
}

#[derive(Debug, Deserialize)]
struct WhisperJsonToken {
    text: Option<String>,
    #[serde(default)]
    p: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct WhisperOffsets {
    from: Option<i64>, // 毫秒
    to: Option<i64>,   // 毫秒
}

pub enum AudioInput<'a> {
    Path(&'a Path),
    Stream(Box<dyn std::io::Read + Send + 'static>),
}

impl<'a> From<&'a Path> for AudioInput<'a> {
    fn from(p: &'a Path) -> Self {
        AudioInput::Path(p)
    }
}

impl<'a> From<&'a PathBuf> for AudioInput<'a> {
    fn from(p: &'a PathBuf) -> Self {
        AudioInput::Path(p.as_path())
    }
}

impl WhisperEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        threads: u32,
        processors: u32,
    ) -> Self {
        let default_vad = PathBuf::from("models/whisper/ggml-silero-v6.2.0.bin");
        let vad = if default_vad.exists() {
            Some(default_vad)
        } else {
            None
        };
        Self::with_vad(cli_path, model_path, vad, threads, processors)
    }

    pub fn with_vad<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        vad_model_path: Option<PathBuf>,
        threads: u32,
        processors: u32,
    ) -> Self {
        Self::with_device(
            cli_path,
            model_path,
            vad_model_path,
            threads,
            processors,
            true,
            false,
            32,
            100,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_device<P1: AsRef<Path>, P2: AsRef<Path>>(
        cli_path: P1,
        model_path: P2,
        vad_model_path: Option<PathBuf>,
        threads: u32,
        processors: u32,
        use_gpu: bool,
        no_fallback: bool,
        max_context: u32,
        gpu_limit_percent: u32,
    ) -> Self {
        Self {
            cli_path: cli_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            vad_model_path,
            threads,
            processors,
            use_gpu,
            gpu_limit_percent,
            no_fallback,
            max_context: max_context.clamp(0, 448),
            need_token_probs: AtomicBool::new(false),
            vad_threshold_bits: AtomicU64::new(0.50f64.to_bits()),
            cancel: Arc::new(AtomicBool::new(false)),
            active_children: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 管线在每次任务开始前调用：救场开启时需要 -ojf 全量 JSON 收集 token 概率，
    /// 救场关闭时降级为轻量 -oj，省掉全量 JSON 体积与解析成本
    pub fn set_need_token_probs(&self, need: bool) {
        self.need_token_probs.store(need, Ordering::SeqCst);
    }

    pub fn set_vad_threshold(&self, threshold: f64) {
        let valid = if threshold.is_finite() { threshold.clamp(0.3, 0.8) } else { 0.5 };
        self.vad_threshold_bits.store(valid.to_bits(), Ordering::SeqCst);
    }

    pub fn uses_gpu(&self) -> bool {
        self.use_gpu
    }

    /// 用户请求终止：置位取消标志并强杀全部正在运行的 whisper-cli 子进程（含切块并行实例）
    ///
    /// 只杀 [`take_live_pids`] 交回的「尚未回收」PID：已被 `wait()` 回收的进程，
    /// 会在 `wait()` 返回处就被 [`ChildPidGuard::disarm`] 从登记表摘掉，因此这里
    /// 不可能对着一个已归还系统、可能被复用给无关进程的号码发信号。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for pid in take_live_pids(&self.active_children) {
            kill_process_tree(pid);
        }
    }

    /// 复位取消标志（每次新任务开始前由管线调用）
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// 登记子进程 PID，并回报「登记这一刻取消是否已经到达」。
    ///
    /// 收口的是 `cancel()` 与「刚 `spawn()`、尚未登记」之间的竞态：`cancel()` 先
    /// 置位取消标志，再 `take_live_pids()` 取名单逐个强杀。若它恰好插在 `spawn()`
    /// 成功与本次 `register()` 之间，登记表里还没有这一行，取到的名单是空的，
    /// 这一发信号就丢了——子进程会一直占着 GPU 跑到自己结束，用户看来就是
    /// 「点了终止转写却完全没生效」。
    ///
    /// 为什么在登记之后复查标志就能闭合：`cancel()` 的置位与本次复查都是 SeqCst，
    /// 二者有全序；而 `take_live_pids()` 的 drain 与本次 register 又在同一把 Mutex
    /// 下互斥。若 drain 没看到本行，只能是 drain 早于 register，那么置位也早于
    /// register，复查必然读到 `true`。于是「drain 发信号」与「本处补杀」两条路径
    /// 必有一条命中，不存在都漏。
    ///
    /// 返回值交给调用方去补 `kill_process_tree`，而不是在本函数里直接杀，是为了让
    /// 这条时序能在单测里直接断言，不必真的拉起子进程、发系统信号。
    fn register_child(&self, pid: u32) -> (ChildPidGuard, bool) {
        let guard = ChildPidGuard::register(self.active_children.clone(), pid);
        (guard, self.is_cancelled())
    }

    pub fn transcribe<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_with_model(
            audio_path,
            language,
            threads,
            total_duration,
            None,
            progress_cb,
        )
    }
    /// 转写音频文件，支持通过 `model_path_override` 在运行时动态切换模型档位（极速/均衡/精准）
    /// 返回 `(segments, vad_duration_sec)`
    pub fn transcribe_with_model<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input(
            AudioInput::Path(audio_path.as_ref()),
            language,
            threads,
            total_duration,
            model_path_override,
            false,
            progress_cb,
        )
    }

    /// Long-video chunk workers already provide process-level parallelism. Run
    /// each child with one internal processor so a user-supplied `-p 2` cannot
    /// multiply into an accidental 2x2 oversubscription.
    pub fn transcribe_with_model_single_processor<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input_with_processors(
            AudioInput::Path(audio_path.as_ref()),
            language,
            threads,
            total_duration,
            model_path_override,
            false,
            Some(1),
            progress_cb,
        )
    }

    /// 纯内存管道推流转写：直接从内存流（Read）中泵入 PCM WAV 字节，0 磁盘 I/O 往返
    pub fn transcribe_stream(
        &self,
        stream: Box<dyn std::io::Read + Send + 'static>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input(
            AudioInput::Stream(stream),
            language,
            threads,
            total_duration,
            model_path_override,
            false,
            progress_cb,
        )
    }

    /// 置信度救场专用：强制保留温度回退（无视引擎级 -nf）重解码低置信窗口，
    /// 质量与旧的回退路径等价，仅用于管线二段重解码的少量片段。
    pub fn transcribe_stream_with_fallback(
        &self,
        stream: Box<dyn std::io::Read + Send + 'static>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input(
            AudioInput::Stream(stream),
            language,
            threads,
            total_duration,
            model_path_override,
            true,
            progress_cb,
        )
    }

    /// 统一入口：根据 AudioInput 分发文件路径模式或纯内存管道推流模式。
    /// `force_fallback` 为 true 时不附加 -nf，即使引擎配置了禁用回退。
    pub fn transcribe_input<'a>(
        &self,
        audio_input: AudioInput<'a>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        force_fallback: bool,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        self.transcribe_input_with_processors(
            audio_input,
            language,
            threads,
            total_duration,
            model_path_override,
            force_fallback,
            None,
            progress_cb,
        )
    }

    fn transcribe_input_with_processors<'a>(
        &self,
        audio_input: AudioInput<'a>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        model_path_override: Option<&Path>,
        force_fallback: bool,
        processors_override: Option<u32>,
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        let temp_dir = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let prefix = temp_dir.join(format!("v2w_whisper_{}_{}", std::process::id(), unique));
        // whisper-cli 通过 `-of <prefix>` 输出 `<prefix>.json`。这个文件过去只在
        // 「跑完 + 解析完」的正常路径上被删：用户取消（上面的提前 return）与
        // 转写失败（下面的 bail!）都发生在删除之前，于是 JSON 永久留在 TEMP 里。
        // 用守卫把它绑到函数作用域上，任何出口（含 panic）都会删掉。
        let json_file = PathBuf::from(format!("{}.json", prefix.display()));
        let mut json_guard = TempPathGuard::file(&json_file);

        let lang = language.unwrap_or("auto");
        let th = threads.unwrap_or(self.threads);
        // 处理器数量保持可配置：不同 Vulkan 驱动对单实例/多实例调度差异很大，
        // 应通过同一素材基准测试选择最佳值，而不是硬编码 GPU 并发策略。
        let processors = processors_override.unwrap_or(self.processors).max(1) as usize;

        // 线程保护机制：多处理器并行时，单处理器线程数等额下调，
        // 保证所有处理器的总 CPU 线程受控在配额内，防止 CPU 线程超售抢占桌面资源。
        let effective_th = if processors > 1 {
            (th / (processors as u32)).max(2)
        } else {
            th
        };

        // 优先使用运行时档位覆盖的模型路径，否则使用配置中的默认模型
        let effective_model: PathBuf = match model_path_override {
            Some(p) => p.to_path_buf(),
            None => self.model_path.clone(),
        };

        let is_stream = matches!(audio_input, AudioInput::Stream(_));
        info!(
            "Whisper 开始转写: [模式: {}], 语言: {}, 线程数: {} (单实例 {}), 并行处理器: {}, 模型: {:?}, VAD: {:?}",
            if is_stream { "纯内存管道推流 (0 磁盘 I/O)" } else { "本地文件" },
            lang, th, effective_th, processors, effective_model, self.vad_model_path
        );

        if !effective_model.exists() {
            anyhow::bail!(
                "Whisper 模型未就位：{:?}。请到「性能设置 → 模型与组件」下载，或在 config.toml 的 paths.whisper_model 指向实际模型文件。",
                effective_model
            );
        }

        if let Some(ref cb) = progress_cb {
            cb(0.0, "Whisper 正在加载模型并开始逐句识别...", None);
        }

        let mut cmd = Command::new(&self.cli_path);
        cmd.arg("-m").arg(&effective_model);

        match &audio_input {
            AudioInput::Path(p) => {
                cmd.arg("-f").arg(p);
                cmd.stdin(std::process::Stdio::null());
            }
            AudioInput::Stream(_) => {
                cmd.arg("-f").arg("-");
                cmd.stdin(std::process::Stdio::piped());
            }
        }

        if let Some(ref vad_path) = self.vad_model_path {
            if vad_path.exists() {
                info!(
                    "Whisper 启用 Silero VAD 高密度语音无损压实与时间戳映射: {:?}",
                    vad_path
                );
                cmd.arg("--vad")
                    .arg("-vm")
                    .arg(vad_path)
                    .arg("-vt")
                    .arg(format!("{:.2}", f64::from_bits(self.vad_threshold_bits.load(Ordering::SeqCst))))
                    .arg("-vsd")
                    .arg("250"); // 最小静音间隔 250ms，剥离无效停顿，全自动时间戳映射还原
            }
        }

        cmd.arg("-l")
            .arg(lang)
            .arg("-t")
            .arg(effective_th.to_string())
            .arg("-p")
            .arg(processors.to_string())
            .arg("-bo")
            .arg("1")
            .arg("-bs")
            .arg("1")
            // `-mc`（跨句上下文上限，取值来自 config.toml 的 whisper_max_context）保留，
            // 不做 GPU/CPU 分流：2026-09-29 同场对照（生产参数去 --carry-initial-prompt、
            // 其余参数完全一致）显示它在两条后端上都不亏：
            //   CPU：-mc 32 = 65.67 s / CER 11.76%，-mc -1（whisper 默认不裁）= 75.45 s / CER 12.13%
            //        → 快 13%，且精度好 0.37 pp
            //   GPU：-mc 32 = 28.13 / 28.42 s / CER 12.03%，-mc -1 = 29.73 / 29.84 s / CER 12.03%
            //        → 两轮编辑距离均为 366，CER 逐字相同，墙钟略快 5%
            // 注：第一轮曾在「不带 --prompt」的 GPU 组合上测到 -mc 32 使 CER +2.07 pp；
            // 该组合已不在默认链路（prompt 是必留项），故不影响保留决策。
            .arg("-mc")
            .arg(self.max_context.to_string())
            .arg("-sns") // 抑制非语音标记(音乐/掌声/杂音)，杜绝自回归发散与幻读
            .arg("-of")
            .arg(&prefix);
        // token 概率只在救场开启时收集：-ojf 全量 JSON 带 token 级概率，用于自算 avg_logprob；
        // 救场关闭时用轻量 -oj，输出体积小一个数量级，解析也更快（默认路径）
        if self.need_token_probs.load(Ordering::SeqCst) {
            cmd.arg("-ojf");
        } else {
            cmd.arg("-oj");
        }

        // Turbo 保留温度回退的旧策略已升级为两段式：快速通场全程 -nf 提速，
        // 低置信片段由管线切窗带回退重解码救回，速度与质量兼得。
        let is_precise = effective_model
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.contains("large") || n.contains("turbo"))
            .unwrap_or(false);
        let suppress_fallback = if force_fallback {
            false
        } else {
            self.no_fallback || !is_precise
        };
        if suppress_fallback {
            cmd.arg("-nf");
        }

        if self.use_gpu {
            info!("Whisper 使用 GPU 推理 (启用 Flash Attention)");
            cmd.arg("-fa");
        } else {
            info!("Whisper 无 GPU，强制 CPU 推理 (-ng)");
            cmd.arg("-ng");
        }

        // whisper.cpp 的 zh 词表偏繁体，prompt 只作简体锚点（必留：不注入时贪心解码会整篇
        // 输出繁体，见 docs/Whisper人工标准字幕对比.md 第 4 节）。
        //
        // 这里曾追加 `--carry-initial-prompt`，注释声称「跨段落固化复用 Prefix Prompt 的
        // 静态 KV-Cache，消除窗口跳变处的重复前向冷启动」。2026-09-29 的同素材同场复跑
        // （05:00–15:00 裁片、ggml-small-q5_0、-t 12，脚本 bench_whisper_closeout_verify.ps1）
        // 否定了这个收益：
        //   GPU：27.67 s / CER 13.14%  →  28.13 s / CER 12.03%（去掉后慢 0.46 s，精度好 1.11 pp）
        //   CPU：66.10 s / CER 12.98%  →  65.67 s / CER 11.76%（去掉后快 0.43 s，精度好 1.22 pp）
        // 更早一轮复核里去掉它则反过来快 0.28 s（26.89 → 26.61 s）。两次符号相反，说明速度
        // 差异落在噪声内、无稳定收益，而精度损失稳定在 1.1～1.2 pp，是「花 1% 速度换 1.1 pp
        // 精度」的净亏，故移除该参数，只保留 prompt 锚点。
        if lang == "zh" || lang == "auto" {
            cmd.arg("--prompt").arg("以下是普通话录音。");
        }

        cmd.env("PYTHONIOENCODING", "utf-8");
        // 让路模式：whisper-cli 降到低于正常优先级，把 CPU 与 GPU 调度权重让给
        // 桌面/前台程序。核显上 Whisper Vulkan 会把 compute 队列压到 65%~80%，
        // 桌面合成器抢不到时间片就会拖窗卡顿；让路后实测转写耗时几乎不变。
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // spawn 前的取消早退：若取消在准备区（拼参数、探模型、挂 VAD、设优先级…）期间
        // 就已到达，就没必要真的把 whisper-cli 拉起来再靠 register_child 的复查把它杀掉——
        // 那几毫秒里进程创建与 GPU/Vulkan 上下文初始化已经白做一遍，用户看到的是
        // 「点了终止却还闪了一下子进程」。这里沿用与 `wait()` 之后取消分支完全相同的
        // 返回（空结果 + 0.0），由管线按取消收尾，不发明新的错误类型或文案。
        //
        // 此处分号以上已持有准备区的 `json_guard`（`TempPathGuard`），它仍在作用域内，
        // 提前 return 会触发 Drop 正常删除该 JSON 路径，不需要手工清理。
        //
        // 与 register_child 复查的分工：本处只覆盖「取消早于 spawn 请求」；取消落在本处
        // 与 register_child 之间时，仍由那处复查补杀。两者是串联的两道门，不会互相干扰。
        if self.cancel.load(Ordering::SeqCst) {
            return Ok((Vec::new(), 0.0));
        }

        // 推理程序缺失时提前给出可操作的中文提示：否则 `cmd.spawn()` 只会抛一句
        // 原始的「调用 whisper-cli 失败」，新用户看不出是「程序没下」还是「路径写错」。
        // 放在取消早退之后：取消语义优先，不应被缺失检查打断。
        if !self.cli_path.exists() {
            anyhow::bail!(
                "Whisper 推理程序未就位：{:?}。请到「性能设置 → 模型与组件」下载「whisper.cpp 识别程序」（会自动解压）；若你自行编译了 Vulkan 版，也可在 config.toml 的 paths.whisper_cli 指向它。",
                self.cli_path
            );
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("调用 whisper-cli 失败: {:?}", self.cli_path))?;
        crate::utils::child_registry::adopt(&child);

        // 登记子进程 PID，供用户「终止转写」时强杀。
        // 用 RAII 守卫而不是「函数末尾手动 retain」：后续任何 `?` / 提前 return /
        // panic 都会经由 Drop 把 PID 摘掉，不会留下会被 PID 复用误伤的陈旧条目。
        // 正常跑完时还要在 `wait()` 返回处主动 `disarm()`，见其文档。
        let (mut child_guard, cancelled_before_register) = self.register_child(child.id());
        // 竞态收口：cancel() 可能恰好插在 spawn() 成功与上面的登记之间——那会儿本行
        // 还没进表，cancel() 的 take_live_pids() 拿到空名单，强杀信号就丢了，子进程
        // 会一直占着 GPU 跑到自己结束。register_child 已用 SeqCst 全序保证「drain 没
        // 看到本行」必然意味着「本处复查命中」，所以这里补一发即可闭合窗口。
        // 此刻 child 仍持有进程句柄、尚未 wait()，PID 不可能被系统复用给无关进程。
        if cancelled_before_register {
            kill_process_tree(child.id());
        }

        // GPU 占空比限速：仅在走显卡推理且用户设了上限时生效。
        // 必须与 child.wait() 配对 stop()，保证句柄关闭前限速线程已退出。
        let gpu_throttle = if self.use_gpu {
            super::media_pipeline::GpuThrottle::for_child(&child, self.gpu_limit_percent)
        } else {
            super::media_pipeline::GpuThrottle::off()
        };

        // 若为内存流输入，启动高效流泵送线程 (128KB 环形缓冲，0 磁盘 I/O)
        let stream_pump_handle = match audio_input {
            AudioInput::Stream(stream) => {
                let stdin = child
                    .stdin
                    .take()
                    .context("获取 whisper-cli 标准输入管道失败")?;
                Some(std::thread::spawn(move || {
                    use std::io::{copy, BufReader, BufWriter, Write};
                    let mut reader = BufReader::with_capacity(128 * 1024, stream);
                    let mut writer = BufWriter::with_capacity(128 * 1024, stdin);
                    let _ = copy(&mut reader, &mut writer);
                    let _ = writer.flush();
                    // writer 与 stdin 离开作用域自动 drop，向 whisper-cli 发送 EOF
                }))
            }
            AudioInput::Path(_) => None,
        };

        // 异步读取 stdout 与 stderr
        // 关键点：长视频或开启 VAD 时，whisper-cli 会输出海量日志到 stderr，
        // 必须异步并行消费 stderr，防止 Windows 64KB 管道缓冲区打满导致子进程永久死锁挂起！
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cb_shared = std::sync::Arc::new(progress_cb);
        let cb_clone = cb_shared.clone();

        // stderr 流式解析：whisper-cli 在长视频 + VAD 下会输出海量日志（每个 VAD 切片一行），
        // 原先 read_to_end 全量读入再整体解码，会占用几十 MB 并产生一次大字符串分配。
        // 这里改为逐行处理：只累计 VAD 耗时、保留末尾若干行用于报错信息，其余直接丢弃。
        const STDERR_TAIL_LINES: usize = 40;
        let stderr_handle = std::thread::spawn(move || -> (f64, String) {
            let Some(err) = stderr else {
                return (0.0, String::new());
            };
            use std::io::BufRead;
            let mut vad_ms = 0.0f64;
            let mut tail: std::collections::VecDeque<String> =
                std::collections::VecDeque::with_capacity(STDERR_TAIL_LINES);
            let mut raw_line = Vec::new();
            let mut reader = std::io::BufReader::new(err);
            loop {
                raw_line.clear();
                match reader.read_until(b'\n', &mut raw_line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
                let line = decode_cli_bytes(&raw_line);
                if let Some(ms) = WhisperEngine::parse_vad_ms_line(&line) {
                    vad_ms += ms;
                }
                if tail.len() == STDERR_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            (vad_ms / 1000.0, tail.into_iter().collect::<Vec<_>>().join("\n"))
        });

        let proc_count = processors;
        let stdout_handle = std::thread::spawn(move || {
            let mut captured_lines = Vec::new();
            let mut last_emit = 0.0f64;
            let mut proc_progress = vec![0.0f64; proc_count];
            let mut streamed_count = 0usize;
            let dur = total_duration.unwrap_or(0.0);
            let chunk_dur = if dur > 0.0 && proc_count > 0 {
                dur / (proc_count as f64)
            } else {
                0.0
            };

            if let Some(out) = stdout {
                use std::io::BufRead;
                let mut reader = std::io::BufReader::new(out);
                let mut raw_line = Vec::new();
                loop {
                    raw_line.clear();
                    match reader.read_until(b'\n', &mut raw_line) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    let line = decode_cli_bytes(&raw_line);
                    let trimmed = line.trim();
                    if trimmed.contains("-->") {
                        if let Some((time_bracket, text_part)) = trimmed.split_once(']') {
                            let time_info = time_bracket.trim_start_matches('[').trim();
                            let (start_sec, end_sec) =
                                if let Some((t_start, t_end)) = time_info.split_once("-->") {
                                    (
                                        Self::parse_time_str(t_start.trim()),
                                        Self::parse_time_str(t_end.trim()),
                                    )
                                } else {
                                    (0.0, 0.0)
                                };
                            let ratio = if chunk_dur > 0.0 {
                                let p_idx = ((end_sec / chunk_dur) as usize).min(proc_count - 1);
                                let c_start = p_idx as f64 * chunk_dur;
                                let p = ((end_sec - c_start) / chunk_dur).clamp(0.0, 1.0);
                                if p > proc_progress[p_idx] {
                                    proc_progress[p_idx] = p;
                                }
                                proc_progress.iter().sum::<f64>() / (proc_count as f64)
                            } else if let Some(tot) = total_duration {
                                if tot > 0.0 {
                                    (end_sec / tot).clamp(0.0, 1.0)
                                } else {
                                    0.5
                                }
                            } else {
                                0.5
                            };

                            let clean_text = normalize_zh_text(text_part.trim());
                            let opt_seg = if !clean_text.is_empty() {
                                streamed_count += 1;
                                Some(Segment {
                                    index: streamed_count,
                                    start: start_sec,
                                    end: end_sec,
                                    text: clean_text,
                                    translation: None,
                                    translation_lang: None,
                                    polished: String::new(),
                                    language: None,
                                    confidence: None,
                                    speaker: None,
                                })
                            } else {
                                None
                            };

                            // 流式预览的粒度必须与最终结果一致：whisper.cpp 的 VAD 在密集讲话
                            // （静音边界缺失）下可能把整段吐成一条长达 30s+ 的片段，直接推进
                            // `streaming_segments` 会让界面显示「一句几秒钟说不完的话」，而它
                            // 结束后才会被 `optimize_segments` 按标点拆开。这里在推流出口先按同一
                            // 规则拆短，让实时字幕流与最终字幕表逐条对齐。
                            let preview_segs = opt_seg
                                .map(|seg| crate::subtitle::split_long_segments(vec![seg]));

                            let display_sec = if proc_count > 1 && dur > 0.0 {
                                ratio * dur
                            } else {
                                end_sec
                            };
                            let mm = (display_sec / 60.0) as u32;
                            let ss = (display_sec % 60.0) as u32;
                            let tot_mm = (dur / 60.0) as u32;
                            let tot_ss = (dur % 60.0) as u32;

                            let label = if dur > 0.0 {
                                format!(
                                    "已转写至 {mm:02}:{ss:02} / {tot_mm:02}:{tot_ss:02} ({:.1}%)",
                                    ratio * 100.0
                                )
                            } else {
                                format!("已转写至 {mm:02}:{ss:02} ({:.1}%)", ratio * 100.0)
                            };

                            // 有新句子时立即推送；无新句子时按进度推进节流推送
                            let has_seg = preview_segs.as_ref().is_some_and(|v| !v.is_empty());
                            let should_emit = has_seg
                                || ratio + 0.0001 >= last_emit + 0.01
                                || ratio >= 0.999;
                            if should_emit {
                                last_emit = ratio;
                                if let Some(cb) = cb_clone.as_ref() {
                                    match preview_segs {
                                        // 一条长段拆成多条时逐条推送，`opt_seg` 只用于
                                        // `should_emit` 的「有新句」判定与进度 label
                                        Some(segs) => {
                                            for seg in segs {
                                                cb(ratio, &label, Some(seg));
                                            }
                                        }
                                        None => cb(ratio, &label, None),
                                    }
                                }
                            }
                            // 回退解析（JSON 缺失时）只需要 [.. --> ..] 形式的行。
                            // 注意：必须在 `if trimmed.contains("-->")` **之内**——此前这行
                            // 的缩进与注释对齐、看起来像在循环体尾部，实际仍在 if 内（缩进是
                            // 24 空格，闭合花括号在 20）。不要把它挪到 if 外，否则长视频下
                            // 每一行进度日志都会被逐行 `String` 累积。
                            captured_lines.push(line);
                        }
                    }
                }
            }
            captured_lines.join("\n")
        });

        let wait_result = child.wait();
        // 进程已被 wait() 回收：立刻摘掉登记行。必须排在 gpu_throttle.stop() 之前——
        // 后者要 join 限速线程、可能阻塞，不能把这段等待夹在「PID 已归还系统」与
        // 「摘除登记行」之间，否则 cancel() 仍有机会对着陈旧 PID 发信号。
        child_guard.disarm();
        // 先停限速线程再处理结果：句柄在 child 被 drop 时关闭，
        // 限速线程若还活着可能对已被复用的句柄调用挂起，误伤无关进程。
        gpu_throttle.stop();
        // 注销交给 child_guard：这里不再手写 retain，避免「新增一条提前返回路径
        // 就得记得再补一次」的维护陷阱（PID 的登记与注销由同一个栈变量负责）。
        let output_status = wait_result.with_context(|| "等待 whisper-cli 进程结束失败")?;
        if let Some(h) = stream_pump_handle {
            let _ = h.join();
        }
        let stdout_str = stdout_handle.join().unwrap_or_default();
        let (vad_sec, stderr_str) = stderr_handle.join().unwrap_or((0.0, String::new()));

        // 用户主动终止：被杀进程的退出码不重要，按空结果返回，由管线走取消收尾
        if self.cancel.load(Ordering::SeqCst) {
            return Ok((Vec::new(), 0.0));
        }

        if !output_status.success() {
            error!(
                "whisper-cli 运行报错 (退出码 {:?}): {}",
                output_status.code(),
                stderr_str
            );
            anyhow::bail!("Whisper 转写失败: {}", stderr_str.trim());
        }

        let mut segments = Vec::new();

        if json_file.exists() {
            let json_raw = std::fs::read(&json_file).unwrap_or_default();
            let json_str = decode_cli_bytes(&json_raw);
            let parsed: Result<WhisperJsonOutput, _> = serde_json::from_str(&json_str);
            // 解析完立刻删除（守卫负责），不再依赖函数末尾的正常收尾路径
            json_guard.remove_now();

            if let Ok(data) = parsed {
                let detected_lang = data.result.and_then(|r| r.language);
                if let Some(trans) = data.transcription {
                    for (i, item) in trans.into_iter().enumerate() {
                        let text = normalize_zh_text(&item.text.unwrap_or_default());
                        if text.is_empty() {
                            continue;
                        }
                        let start =
                            item.offsets.as_ref().and_then(|o| o.from).unwrap_or(0) as f64 / 1000.0;
                        let end =
                            item.offsets.as_ref().and_then(|o| o.to).unwrap_or(0) as f64 / 1000.0;

                        segments.push(Segment {
                            index: i + 1,
                            start,
                            end,
                            text,
                            translation: None,
                            translation_lang: None,
                            polished: String::new(),
                            language: detected_lang.clone(),
                            confidence: Self::tokens_avg_logprob(&item.tokens),
                            speaker: None,
                        });
                    }
                }
            }
        }

        // 回退逻辑：如果 json 没产出，从捕获的 stdout 解析
        if segments.is_empty() && !stdout_str.is_empty() {
            segments = Self::parse_stdout_segments(&stdout_str);
        }

        // 智能优化时间轴：消除 100ms 重叠鬼影、消除时间重叠冲突、广播级短句延展平滑
        crate::subtitle::optimize_segments(&mut segments);

        if let Some(ref cb) = cb_shared.as_ref() {
            cb(
                1.0,
                &format!("转写完成，共 {} 个片段", segments.len()),
                None,
            );
        }

        info!(segments = segments.len(), vad_sec, "Whisper 转写完成");
        Ok((segments, vad_sec))
    }

    /// 从 -ojf 的 token 级概率自算 avg_logprob（与官方 avg_logprob 同语义）。
    /// 过滤 "[_BEG_]"/"[_SOT_]" 等特殊标记 token，只统计真实解码 token。
    fn tokens_avg_logprob(tokens: &[WhisperJsonToken]) -> Option<f64> {
        let logprobs: Vec<f64> = tokens
            .iter()
            .filter(|t| {
                t.p.is_some_and(|p| p > 0.0) && !t.text.as_deref().unwrap_or("").starts_with("[_")
            })
            .map(|t| t.p.unwrap().ln())
            .collect();
        if logprobs.is_empty() {
            None
        } else {
            Some(logprobs.iter().sum::<f64>() / logprobs.len() as f64)
        }
    }

    /// 从单行 whisper-cli 日志中提取 VAD 耗时（毫秒），非 VAD 行返回 None。
    ///
    /// 抽成行级函数是为了让 stderr 能在读取线程里流式累加 VAD 耗时，
    /// 而不必先把整份日志存进内存。
    fn parse_vad_ms_line(line: &str) -> Option<f64> {
        let pos = line.find("vad time =")?;
        let rest = &line[pos + 10..];
        let end = rest.find("ms")?;
        rest[..end].trim().parse::<f64>().ok()
    }

    /// 从完整 stderr 文本中提取 Silero VAD 总耗时 (秒)
    pub fn parse_vad_time(stderr: &str) -> f64 {
        stderr.lines().filter_map(Self::parse_vad_ms_line).sum::<f64>() / 1000.0
    }

    /// 解析形如 [00:00:00.000 --> 00:00:02.500]  文本 的标准输出
    fn parse_stdout_segments(output: &str) -> Vec<Segment> {
        let mut segments = Vec::new();
        let mut idx = 1;

        for line in output.lines() {
            let line = line.trim();
            if line.starts_with('[') && line.contains("-->") {
                if let Some(end_bracket) = line.find(']') {
                    let time_range = &line[1..end_bracket];
                    let text = normalize_zh_text(&line[end_bracket + 1..]);
                    if text.is_empty() {
                        continue;
                    }
                    let parts: Vec<&str> = time_range.split("-->").collect();
                    if parts.len() == 2 {
                        let start = Self::parse_time_str(parts[0].trim());
                        let end = Self::parse_time_str(parts[1].trim());
                        segments.push(Segment {
                            index: idx,
                            start,
                            end,
                            text,
                            translation: None,
                            translation_lang: None,
                            polished: String::new(),
                            language: None,
                            confidence: None,
                            speaker: None,
                        });
                        idx += 1;
                    }
                }
            }
        }
        segments
    }

    fn parse_time_str(s: &str) -> f64 {
        // HH:MM:SS.mmm
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() == 3 {
            let h: f64 = parts[0].parse().unwrap_or(0.0);
            let m: f64 = parts[1].parse().unwrap_or(0.0);
            let s: f64 = parts[2].parse().unwrap_or(0.0);
            h * 3600.0 + m * 60.0 + s
        } else {
            0.0
        }
    }
}

/// 跨平台强杀指定进程及其子进程树（用于用户「终止转写」即时生效）
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let _ = cmd.args(["/F", "/T", "/PID", &pid.to_string()]).output();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

/// whisper-cli 在 Windows 上可能吐 UTF-8 或本机 ANSI/GBK。
fn decode_cli_bytes(raw: &[u8]) -> String {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    if let Ok(s) = std::str::from_utf8(raw) {
        return trim_cli_line(s);
    }
    let (cow, _, had_errors) = encoding_rs::GBK.decode(raw);
    if !had_errors {
        return trim_cli_line(&cow);
    }
    trim_cli_line(&String::from_utf8_lossy(raw))
}

fn trim_cli_line(s: &str) -> String {
    s.trim_end_matches(['\r', '\n']).to_string()
}

/// Whisper 的 zh 词表偏繁体；落地前统一转简体。
fn normalize_zh_text(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    zhconv::zhconv(s, zhconv::Variant::ZhHans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traditional_becomes_simplified() {
        assert_eq!(
            normalize_zh_text("這是繁體中文語音識別"),
            "这是繁体中文语音识别"
        );
        assert_eq!(normalize_zh_text("概率不等式"), "概率不等式");
    }

    #[test]
    fn avg_logprob_from_token_probs() {
        let tok = |text: &str, p: f64| WhisperJsonToken {
            text: Some(text.to_string()),
            p: Some(p),
        };
        // [_BEG_] 应被过滤；两个真实 token: ln(0.9) 与 ln(0.5) 的均值
        let tokens = vec![tok("[_BEG_]", 0.913), tok("你", 0.9), tok("好", 0.5)];
        let got = WhisperEngine::tokens_avg_logprob(&tokens).unwrap();
        let expect = (0.9f64.ln() + 0.5f64.ln()) / 2.0;
        assert!((got - expect).abs() < 1e-12);
        // 全特殊 token / 空列表 / 零概率 → None
        assert!(WhisperEngine::tokens_avg_logprob(&[tok("[_SOT_]", 1.0)]).is_none());
        assert!(WhisperEngine::tokens_avg_logprob(&[]).is_none());
        assert!(WhisperEngine::tokens_avg_logprob(&[tok("词", 0.0)]).is_none());
    }

    #[test]
    fn gbk_bytes_decode_to_han() {
        let (bytes, _, _) = encoding_rs::GBK.encode("切比雪夫不等式");
        assert_eq!(decode_cli_bytes(&bytes), "切比雪夫不等式");
    }

    #[test]
    fn test_in_memory_stream_transcribe() {
        let cli_path =
            PathBuf::from("tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe");
        let model_path = PathBuf::from("models/whisper/ggml-base.bin");
        let ffmpeg_path = PathBuf::from("A:\\cppsoft\\ffmpeg-6.9\\bin\\ffmpeg.exe");
        let sample_media = PathBuf::from("resources/sample/sample.mp4");

        if cli_path.exists() && model_path.exists() && ffmpeg_path.exists() && sample_media.exists()
        {
            let ffmpeg = crate::engines::FFmpegEngine::new(ffmpeg_path);
            let engine =
                WhisperEngine::with_device(cli_path, model_path, None, 4, 1, true, true, 32, 100);

            let mut child = ffmpeg
                .spawn_audio_stream(&sample_media)
                .expect("FFmpeg 内存流启动失败");
            let stdout = child.stdout.take().expect("获取 stdout 失败");

            let (segs, _) = engine
                .transcribe_stream(Box::new(stdout), Some("zh"), Some(4), Some(6.0), None, None)
                .expect("纯内存推流转写应成功");

            let _ = child.wait();
            assert!(!segs.is_empty(), "内存推流转写应产出有效字幕");
            println!("纯内存管道推流测试成功，生成 {} 条字幕", segs.len());
        }
    }

    #[test]
    #[ignore]
    fn test_real_audio_transcribe() {
        let cli_path =
            PathBuf::from("tools/whisper-vulkan/whisper-1.8.4-windows-x64/whisper-cli.exe");
        let model_path = PathBuf::from("models/whisper/ggml-large-v3-turbo-q5_0.bin");
        let vad_path = Some(PathBuf::from("models/whisper/ggml-silero-v6.2.0.bin"));
        let engine =
            WhisperEngine::with_device(cli_path, model_path, vad_path, 8, 2, true, false, 32, 100);
        let mut audio = PathBuf::from("target/test_30s.wav");
        if !audio.exists() {
            audio = PathBuf::from("target/test_2min.wav");
        }
        if audio.exists() {
            let start = std::time::Instant::now();
            let streamed_segs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let streamed_clone = streamed_segs.clone();

            let (segs, vad_sec) = engine
                .transcribe_with_model(
                    &audio,
                    Some("zh"),
                    Some(8),
                    Some(30.0),
                    None,
                    Some(Box::new(move |_ratio, label, opt_seg| {
                        if let Some(s) = opt_seg {
                            println!(
                                "  [流式捕获] [{} -> {}] {} ({})",
                                s.start, s.end, s.text, label
                            );
                            streamed_clone.lock().unwrap().push(s);
                        }
                    })),
                )
                .expect("转写应成功");
            let elapsed = start.elapsed().as_secs_f64();
            let captured_count = streamed_segs.lock().unwrap().len();
            println!(
                "转写完成! 耗时: {:.2}s, VAD: {:.2}s, 最终片段数: {}, 流式逐句推送数: {}",
                elapsed,
                vad_sec,
                segs.len(),
                captured_count
            );
            assert!(!segs.is_empty(), "应解析出有效字幕片段");
            assert!(captured_count > 0, "应通过流式回调逐句推送到字幕视窗");
            println!(
                "第一句: [{} -> {}] {}",
                segs[0].start, segs[0].end, segs[0].text
            );
        }
    }

    // ---- ChildPidGuard：子进程 PID 登记的生命周期 ----
    //
    // 这些测试直接针对「陈旧 PID 会被 kill_process_tree 误杀」这条风险链的源头：
    // 只要守卫在任何退出方式下都把 PID 摘干净，`cancel()` 就不可能拿到旧数字。

    fn empty_registry() -> Arc<Mutex<Vec<ChildEntry>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn registered(registry: &Arc<Mutex<Vec<ChildEntry>>>) -> Vec<ChildEntry> {
        registry.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 复刻真实调用路径：登记 PID 之后遇到 `?` 提前返回。
    fn early_return_after_register(
        registry: &Arc<Mutex<Vec<ChildEntry>>>,
        pid: u32,
    ) -> Result<(), &'static str> {
        let _guard = ChildPidGuard::register(registry.clone(), pid);
        // 这一行对应 transcribe_with_model 里 `child.stdin.take()...?`：
        // 它在登记之后提前返回，过去会把 PID 永久留在表里
        Err("simulated spawn failure")?;
        #[allow(unreachable_code)]
        Ok(())
    }

    #[test]
    fn guard_deregisters_pid_on_early_return() {
        let registry = empty_registry();
        assert!(early_return_after_register(&registry, 4242).is_err());
        assert!(
            registered(&registry).is_empty(),
            "提前返回（?）时守卫必须把 PID 从登记表摘掉，否则 PID 复用后会误杀无关进程"
        );
    }

    #[test]
    fn guard_keeps_pid_while_child_is_alive() {
        let registry = empty_registry();
        {
            let _guard = ChildPidGuard::register(registry.clone(), 777);
            let entries = registered(&registry);
            assert_eq!(
                entries.len(),
                1,
                "守卫存活期间 PID 必须在表里，否则「终止转写」杀不掉正在跑的进程"
            );
            // 只断言 PID，不断言序号：序号取自进程级全局计数器，而 cargo test
            // 默认并行跑用例，别的用例可能先领走 0——断言具体数值会让这条测试随机翻车。
            assert_eq!(entries[0].0, 777, "登记的应是该子进程 PID");
        }
        assert!(
            registered(&registry).is_empty(),
            "子进程结束后守卫负责注销"
        );
    }

    #[test]
    fn guard_deregisters_pid_on_panic_unwind() {
        let registry = empty_registry();
        let reg = registry.clone();
        // 静默掉 panic 打印，避免测试输出里出现像失败的堆栈
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = ChildPidGuard::register(reg, 999);
            panic!("模拟转写线程 panic");
        }));
        std::panic::set_hook(prev_hook);

        assert!(outcome.is_err(), "闭包应当 panic");
        assert!(
            registered(&registry).is_empty(),
            "panic 展开也必须摘掉 PID，不能留下陈旧条目"
        );
    }

    #[test]
    fn guard_only_removes_its_own_entry() {
        // PID 被系统复用：两个守卫登记同一个 PID 值，但序号不同。
        // 先退出的那个不能把后登记（仍存活）的那一行摘掉。
        let registry = empty_registry();
        let first = ChildPidGuard::register(registry.clone(), 1234);
        let _second = ChildPidGuard::register(registry.clone(), 1234);
        assert_eq!(registered(&registry).len(), 2);

        drop(first);
        let left = registered(&registry);
        assert_eq!(left.len(), 1, "只应摘掉自己那一行");
        assert_eq!(left[0].0, 1234, "留下的仍应是同一 PID，供 cancel 强杀");
    }

    /// 核心回归：`wait()` 回收子进程后立刻 `disarm()`，`cancel()` 就不能再拿到这个 PID。
    ///
    /// 这是「已回收的 PID 不会被 `kill_process_tree` 误杀」的直接断言，且完全不依赖
    /// 真实子进程的时序——`take_live_pids` 正是 `cancel()` 唯一的强杀名单来源，
    /// 断言它看不到该 PID，就等价于断言 `cancel()` 不会对它发信号。
    #[test]
    fn cancel_never_sees_pid_disarmed_after_reap() {
        // 对照组：尚未回收时必须在名单里，否则「终止转写」杀不掉正在跑的进程。
        let live = empty_registry();
        let _live_guard = ChildPidGuard::register(live.clone(), 4321);
        assert_eq!(
            take_live_pids(&live),
            vec![4321],
            "未回收的 PID 必须能被 cancel() 取走去强杀"
        );

        // 主用例：模拟 `child.wait()` 返回后立刻 disarm（PID 已归还系统）。
        let registry = empty_registry();
        let mut guard = ChildPidGuard::register(registry.clone(), 4321);
        guard.disarm();
        assert!(
            registered(&registry).is_empty(),
            "disarm 必须立刻把登记行摘掉"
        );
        assert!(
            take_live_pids(&registry).is_empty(),
            "已回收的 PID 不得出现在 cancel() 的强杀名单里，否则会误杀复用该 PID 的无关进程"
        );
    }

    /// `disarm` 之后 `Drop` 再次摘除必须是幂等的：既不能 panic，也不能误伤别人的条目。
    #[test]
    fn disarm_then_drop_is_idempotent_and_scoped() {
        let registry = empty_registry();
        let mut reaped = ChildPidGuard::register(registry.clone(), 1001);
        // 另一个仍在跑的守卫复用同一 PID 值（序号不同），disarm/Drop 都不得碰它。
        let _still_running = ChildPidGuard::register(registry.clone(), 1001);

        reaped.disarm();
        drop(reaped);

        let left = registered(&registry);
        assert_eq!(left.len(), 1, "只应摘掉自己那一行，不能牵连同 PID 的他人条目");
        assert_eq!(take_live_pids(&registry), vec![1001]);
    }

    /// 只用于 PID 表用例的引擎：`with_device` 只是存下路径，不读模型、不起进程。
    fn engine_with_dummy_paths() -> WhisperEngine {
        WhisperEngine::with_device(
            "whisper-cli.exe",
            "ggml-base.bin",
            None,
            1,
            1,
            false,
            true,
            32,
            100,
        )
    }

    /// 回归：`cancel()` 恰好插在 `spawn()` 成功与 `register()` 之间——那会儿登记表
    /// 还是空的，`take_live_pids()` 取不到任何 PID，强杀名单为空。登记之后的复查必须
    /// 命中，等价于由转写线程补发一发强杀；否则这个刚起的子进程会一直跑到自己结束。
    ///
    /// 不依赖真实子进程时序：直接把「取消先到、登记后到」这个次序摆出来断言结果。
    #[test]
    fn cancel_between_spawn_and_register_still_requests_kill() {
        let engine = engine_with_dummy_paths();
        // 模拟「取消早于本次登记到达」：此刻表里没有条目，cancel() 只能空跑。
        engine.cancel();
        assert!(
            take_live_pids(&engine.active_children).is_empty(),
            "取消到达时登记表为空，正是本用例要覆盖的窗口"
        );

        // 随后子进程才 spawn 成功并登记：复查必须判定「要补杀」。
        let (_guard, needs_kill) = engine.register_child(4242);
        assert!(
            needs_kill,
            "登记后复查必须命中取消标志，否则取消会漏掉这个刚 spawn 的子进程"
        );
    }

    /// 反向断言：未取消时复查不得命中，避免正常转写启动平白多杀一发。
    #[test]
    fn register_without_cancel_does_not_request_kill() {
        let engine = engine_with_dummy_paths();
        let (_guard, needs_kill) = engine.register_child(4243);
        assert!(!needs_kill, "未取消时不得触发补杀");
        assert_eq!(
            registered(&engine.active_children).len(),
            1,
            "守卫仍必须在表里，正常取消路径要靠它拿到 PID"
        );
    }

    /// 造一个 cli 路径必然不存在的引擎：spawn 若被走到就必然失败，用作「有没有真的
    /// 尝试创建子进程」的哨兵。`with_device` 只是存下路径，不读模型、不起进程。
    fn engine_with_missing_cli() -> WhisperEngine {
        let missing_cli = std::env::temp_dir().join("v2w_probe_missing_whisper_cli_9f3a.exe");
        WhisperEngine::with_device(
            missing_cli,
            "ggml-base.bin",
            None,
            1,
            1,
            false,
            true,
            32,
            100,
        )
    }

    /// 造一个确实存在的临时模型文件（落在系统 TEMP，不入仓库），用于越过
    /// `transcribe_input_with_processors` 里「模型文件未找到」的提前 `bail!`，
    /// 从而把执行推进到 spawn 前的取消早退点。
    fn temp_model_file(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "v2w_probe_model_{}_{}_{}.bin",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&p, b"stub").expect("写临时模型文件应成功");
        p
    }

    /// spawn 前早退的直接断言：取消已置位时，`transcribe_input_with_processors` 必须在
    /// 走到 `cmd.spawn()` 之前就返回与既有取消分支相同的 `Ok((空, 0.0))`，且**根本没有
    /// 尝试创建子进程**。
    ///
    /// 「没 spawn」怎么断言，而不是「spawn 了又杀」：cli 路径被设成一个**必然不存在**的
    /// 可执行文件。代码若仍走到 spawn，会直接得到「调用 whisper-cli 失败」的 `Err`；
    /// 既然实际是 `Ok((空, 0.0))`，唯一的解释就是早退短路了 spawn。对照用例
    /// `without_cancel_missing_cli_reports_spawn_failure` 证明这个哨兵确实会创建失败，
    /// 因而这里的 `Ok` 不可能来自「spawn 成功后再被复查补杀」——那条路仍需一次成功的 spawn。
    #[test]
    fn cancel_before_spawn_skips_process_launch() {
        let engine = engine_with_missing_cli();
        engine.cancel();
        let model = temp_model_file("cancel");
        let audio = std::env::temp_dir().join("v2w_probe_dummy_audio.wav");

        let (segments, elapsed) = engine
            .transcribe_input_with_processors(
                AudioInput::Path(audio.as_path()),
                None,
                None,
                None,
                Some(model.as_path()),
                false,
                None,
                None,
            )
            .expect("取消早退必须走既有取消路径，返回 Ok(空结果) 而不是创建失败的错误");

        let _ = std::fs::remove_file(&model);

        assert!(segments.is_empty(), "取消后不应产出任何字幕片段");
        assert_eq!(elapsed, 0.0, "取消早退沿用既有取消分支的 0.0 时长");
        assert!(
            registered(&engine.active_children).is_empty(),
            "早退路径连登记都不该发生，登记表必须为空"
        );
    }

    /// 对照：不置位取消时，同一个不存在的 cli 会让 spawn 真的失败并报「调用 whisper-cli
    /// 失败」。这证明上一个用例里的 `Ok(空, 0.0)` 成因只能是「取消早退短路了 spawn」，
    /// 而不是别的路径碰巧也返回了 Ok。
    #[test]
    fn without_cancel_missing_cli_reports_spawn_failure() {
        let engine = engine_with_missing_cli();
        // 刻意不 cancel。
        let model = temp_model_file("spawnfail");
        let audio = std::env::temp_dir().join("v2w_probe_dummy_audio.wav");

        let err = engine
            .transcribe_input_with_processors(
                AudioInput::Path(audio.as_path()),
                None,
                None,
                None,
                Some(model.as_path()),
                false,
                None,
                None,
            )
            .expect_err("未取消时必须真的尝试 spawn，不存在的 cli 应报错");

        let _ = std::fs::remove_file(&model);

        let msg = format!("{err:#}");
        assert!(
            msg.contains("Whisper 推理程序未就位"),
            "错误应来自 spawn 失败这条路径，实际: {msg}"
        );
    }
}
