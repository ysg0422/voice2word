//! 阿里 SenseVoice-Small 极速非自回归语音识别引擎封装
//! 通过 sherpa-onnx 驱动 INT8 模型，单次前向出字，自带标点与 ITN，速度是 Whisper 的 5~8 倍。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

use crate::subtitle::Segment;

use super::whisper::{join_or_log, take_live_pids, ChildEntry, ChildPidGuard};

/// SenseVoice 单会话线程数上限。
///
/// 依据：16 核机器实测单会话在 8 线程附近已饱和（8 线程 32.6s vs 16 线程 41.6s，
/// 线程越多反而越慢），故单会话与切块回退路径都夹到 8。公开给设置页推导滑条量程：
/// 滑条若能选到 9~16 而引擎静默夹回 8，就是「改了没效果」的观感来源——与 VAD 阈值、
/// 并行进程数滑条同型。
pub const MAX_THREADS: u32 = 8;

/// SenseVoice 单会话线程数下限（与设置页滑条最左档一致）。
pub const MIN_THREADS: u32 = 2;

/// 把线程数夹到引擎自述的有效区间 [`MIN_THREADS`, `MAX_THREADS`]。
///
/// 设置页滑条量程即由这两个常量推导，而 `whisper_threads` 是全局共享项
/// （16 核机器上可配到 16），CLI/GUI 都把它原样透传给本引擎。不夹的话，
/// 用户配 16 线程会真的以 16 线程起 runner——既违反本文件的实测结论
/// （8 线程已饱和、更多反而更慢），又造成「改了没效果 / 越改越慢」的观感。
fn clamp_threads(threads: u32) -> u32 {
    threads.clamp(MIN_THREADS, MAX_THREADS)
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

#[derive(Debug, Deserialize)]
struct SenseVoiceStreamLine {
    event: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    elapsed_sec: f64,
    #[serde(default)]
    segments: Vec<SenseVoiceOutputItem>,
    /// `event == "provider"` 时由 runner 回传：请求的 provider。
    #[serde(default)]
    requested: String,
    /// `event == "provider"` 时由 runner 回传：实际生效的 provider。
    #[serde(default)]
    actual: String,
}

#[derive(Debug, Deserialize)]
struct SenseVoiceOutputItem {
    index: usize,
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    polished: String,
}

/// ONNX 执行后端的实况：请求值 vs runner 回报的实际生效值。
///
/// 「静默回落」的可见化载体——引擎把它记在 `Arc<Mutex<..>>` 里，供上层读取
/// （日志 / 未来的 UI 横幅）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    /// 配置里请求的 provider（Rust 侧已归一：`dml` | `cpu`）。
    pub requested: String,
    /// runner 回报的实际生效 provider（如 `cpu`）。
    pub actual: String,
}

impl ProviderStatus {
    /// 是否发生了「请求非 cpu、实际却落到别的值」的回落。
    pub fn fell_back(&self) -> bool {
        provider_fallback_notice(&self.requested, &self.actual).is_some()
    }
}

/// 判定「请求了非 cpu 的 provider、实际却落到别的值」，命中则给出可操作告警文案。
///
/// 这是「静默回落」的唯一判定入口，抽成纯函数以便单测（不需要真跑子进程）。
/// 返回 `None` 表示请求就是 cpu（本就无需 GPU）、或实际值与请求一致（真吃到了）。
pub fn provider_fallback_notice(requested: &str, actual: &str) -> Option<String> {
    let req = requested.trim().to_ascii_lowercase();
    let act = actual.trim().to_ascii_lowercase();
    if req.is_empty() || act.is_empty() || req == "cpu" || req == act {
        return None;
    }
    Some(format!(
        "ONNX provider 回落：配置请求 `{req}`，实际生效 `{act}`。\
         这通常是 Python 环境里的 sherpa-onnx 绑定了 CPU-only 版 onnxruntime（未编译进 \
         DirectML）。修复：`pip install onnxruntime-directml` 并改用 DirectML 版 \
         sherpa-onnx；若暂时不升级，请在「性能设置」里把 provider 切回 `cpu` 以消除误导。"
    ))
}

/// 从子进程 stderr 文本里识别 sherpa-onnx 的 provider 回落特征串。
///
/// 作为 `provider` JSON 事件的兜底通道：旧版 runner 不回传该事件时仍能检出。
pub fn stderr_provider_fallback(stderr: &str) -> bool {
    stderr.contains("Fallback to cpu")
        || stderr.contains("Unsupported string:")
        || stderr.contains("Available providers:")
}

/// 一次 `record_provider` 回报的纯函数判定结果（不碰锁、不发日志，可直接单测）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderReportOutcome {
    /// `Some` 表示本次回报是 slot 的首次写入，携带该落盘状态；`None` 表示只读重复回报。
    stored: Option<ProviderStatus>,
    /// `Some` 表示本次回报应产生一条回落告警；`None` 表示本次不得告警。
    warns_on_this_call: Option<String>,
}

/// 判定「本次 provider 回报」是否应写状态、是否应告警。
///
/// 判定基准是 slot 的**实况** `seen`，而不是本次入参：
/// - `seen` 为空（本次才是首次回报）：状态写入本次入参，告警文案也由本次入参推出；
/// - `seen` 已有值：后到的重复/矛盾回报既不覆盖状态，也不产生告警——否则同一次运行里
///   会先报「已回落到 CPU」、再被一条「将使用 GPU」的矛盾值刷屏，且把一次性告警额度
///   提前吃掉，导致真正的首次回落反而静默。语义与 punc.rs 的 `record_provider` 一致。
fn provider_report_outcome(
    seen: Option<&ProviderStatus>,
    incoming: &ProviderStatus,
) -> ProviderReportOutcome {
    if seen.is_some() {
        return ProviderReportOutcome {
            stored: None,
            warns_on_this_call: None,
        };
    }
    ProviderReportOutcome {
        stored: Some(incoming.clone()),
        warns_on_this_call: provider_fallback_notice(&incoming.requested, &incoming.actual),
    }
}

#[derive(Clone)]
pub struct SenseVoiceEngine {
    runner_path: PathBuf,
    model_path: PathBuf,
    tokens_path: PathBuf,
    vad_model_path: PathBuf,
    threads: u32,
    /// 运行 runner 脚本的 Python 解释器（默认 "python"，可配置为绝对路径）
    python_path: PathBuf,
    /// ONNX 执行后端：`cpu` | `dml`。默认 `cpu`（零回归）；
    /// `dml` 只有在 Python 环境装了 DirectML 版 sherpa-onnx 时才真正生效，
    /// 否则 runner 会静默回落 cpu。收敛规则与 `GpuConfig::resolve_onnx_provider` 一致。
    provider: String,
    /// 回落可见化：runner 回报的 provider 实况（首个回报为准）。
    provider_status: Arc<Mutex<Option<ProviderStatus>>>,
    /// 回落告警只打一次：并行切块时每个 worker 进程都回报一条，不能刷屏。
    fallback_reported: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    active_children: Arc<Mutex<Vec<ChildEntry>>>,

    /// 上一次 `transcribe*` 是否因 stdout 管道**读取失败**（而不是正常 EOF）提前结束：
    /// `true` 表示结果可能被截断（后面的句子根本没读出来）。
    ///
    /// 与 whisper 引擎同名的信号；用引擎级原子标志而不是改返回类型，因为
    /// `Result<(Vec<Segment>, f64)>` 是与 whisper 对齐的既有签名，改它会波及本文件
    /// 之外的调用点。Arc 是因为 `SenseVoiceEngine` 派生 Clone。
    output_truncated: Arc<AtomicBool>,
}

impl SenseVoiceEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>, P4: AsRef<Path>>(
        runner_path: P1,
        model_path: P2,
        tokens_path: P3,
        vad_model_path: P4,
        threads: u32,
    ) -> Self {
        Self::with_python(
            runner_path,
            model_path,
            tokens_path,
            vad_model_path,
            threads,
            PathBuf::from("python"),
        )
    }

    /// 指定 Python 解释器（用于 PATH 上有多个 Python、默认 `python` 缺依赖的场景）
    pub fn with_python<
        P1: AsRef<Path>,
        P2: AsRef<Path>,
        P3: AsRef<Path>,
        P4: AsRef<Path>,
        P5: AsRef<Path>,
    >(
        runner_path: P1,
        model_path: P2,
        tokens_path: P3,
        vad_model_path: P4,
        threads: u32,
        python_path: P5,
    ) -> Self {
        Self::with_python_and_provider(
            runner_path,
            model_path,
            tokens_path,
            vad_model_path,
            threads,
            python_path,
            "cpu",
        )
    }

    /// 完整指定：解释器 + ONNX 执行后端（`cpu` | `dml`）。
    pub fn with_python_and_provider<
        P1: AsRef<Path>,
        P2: AsRef<Path>,
        P3: AsRef<Path>,
        P4: AsRef<Path>,
        P5: AsRef<Path>,
    >(
        runner_path: P1,
        model_path: P2,
        tokens_path: P3,
        vad_model_path: P4,
        threads: u32,
        python_path: P5,
        provider: &str,
    ) -> Self {
        let provider = match provider.trim().to_ascii_lowercase().as_str() {
            "dml" | "directml" => "dml".to_string(),
            _ => "cpu".to_string(),
        };
        Self {
            runner_path: runner_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            tokens_path: tokens_path.as_ref().to_path_buf(),
            vad_model_path: vad_model_path.as_ref().to_path_buf(),
            threads: clamp_threads(threads),
            python_path: python_path.as_ref().to_path_buf(),
            provider,
            provider_status: Arc::new(Mutex::new(None)),
            fallback_reported: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            active_children: Arc::new(Mutex::new(Vec::new())),

            // 本轮结果是否被截断：见 `output_truncated`。
            output_truncated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 用户请求终止：置位取消标志并强杀正在运行的 runner 子进程
    ///
    /// 只杀 [`take_live_pids`] 交回的「尚未回收」PID：已被 `wait()` 回收的进程会在
    /// `wait()` 返回处就被 [`ChildPidGuard::disarm`] 摘掉，因此这里不会对着一个
    /// 已归还系统、可能被复用给无关进程的号码发信号。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for pid in take_live_pids(&self.active_children) {
            kill_process_tree(pid);
        }
    }

    /// 复位取消标志（每次新任务开始前由管线调用）
    ///
    /// 刻意不复位 `provider_status` / `fallback_reported`：provider 是否可用是
    /// **进程级事实**（Python 环境装了什么），不随任务切换而变；每任务重报一次
    /// 只会把同一条回落告警刷进日志。
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
        // 每轮任务开始都要清掉上一轮的截断标记：否则一次管道故障会一直挂在引擎上，
        // 把后续每一次正常转写都误报成「输出被截断」。
        self.output_truncated.store(false, Ordering::SeqCst);
    }

    /// 上一次转写的 stdout 是否因**读取失败**（非 EOF）提前结束：`true` = 结果可能被截断。
    ///
    /// `line_res.unwrap_or_default()` 的旧写法把「读到一半管道报错」变成一行空文本，
    /// 再被 `trimmed.is_empty()` 丢掉：与「runner 正常收尾」在结果上完全同形。
    /// 调用方（管线 / UI）读到 `true` 时应提示「字幕可能不完整」。
    pub fn output_truncated(&self) -> bool {
        self.output_truncated.load(Ordering::SeqCst)
    }

    /// 读取并**复位**截断标记，适合「一次转写只提示一次」的调用点。
    pub fn take_output_truncated(&self) -> bool {
        self.output_truncated.swap(false, Ordering::SeqCst)
    }
    /// 记录 runner 回报的 provider 实况；命中回落时打印一次可操作告警。
    ///
    /// 首次回报为准：并行切块时多个 worker 进程回报的是同一条事实，后到的只算重复。
    /// 是否就「本次回报」告警，必须按 slot 的**实况**（`slot.is_none()`）判定，而不是
    /// 看本次入参：同一次运行里，先到的回报已把实况写掉后，后到的矛盾值既不得覆盖
    /// 状态，更不能借着它去刷一条与实际状态相反的告警（例如先报「已回落 CPU」，
    /// 紧接着又报「将使用 GPU」）。口径与 punc.rs 的 `record_provider` 保持一致。
    fn record_provider(&self, requested: &str, actual: &str) {
        let status = ProviderStatus {
            requested: requested.trim().to_string(),
            actual: actual.trim().to_string(),
        };
        let mut slot = self
            .provider_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 实况判定：只有「本次才是首次回报」的那一次才允许写状态并告警。
        let first = slot.is_none();
        let outcome = provider_report_outcome(slot.as_ref(), &status);
        if let Some(stored) = outcome.stored.clone() {
            *slot = Some(stored);
        }
        drop(slot);
        // 告警文案取自实际落盘的状态（实况），而不是本次入参。
        if first {
            if let Some(notice) = provider_fallback_notice(&status.requested, &status.actual) {
                if !self.fallback_reported.swap(true, Ordering::SeqCst) {
                    warn!("{notice}");
                }
            }
        }
    }

    /// 读取 runner 回报的 provider 实况（尚未跑过推理时为 `None`）。
    ///
    /// 供上层把「配置了 dml 却回落 cpu」显示到界面上；引擎不感知 UI。
    pub fn provider_status(&self) -> Option<ProviderStatus> {
        self.provider_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 登记子进程 PID，并回报「登记这一刻取消是否已经到达」。
    ///
    /// 收口的是 `cancel()` 与「刚 `spawn()`、尚未登记」之间的竞态：`cancel()` 先
    /// 置位取消标志，再 `take_live_pids()` 取名单强杀。若它恰好插在 `spawn()` 成功
    /// 与本次 `register()` 之间，登记表里还没有这一行，取到的名单是空的，这一发
    /// 信号就丢了——runner 会一直占着 GPU 跑到自己结束，用户看来就是「点了终止
    /// 转写却完全没生效」。
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
        (guard, self.cancel.load(Ordering::SeqCst))
    }

    /// 检查 SenseVoice 所需脚本与全部模型权重是否就位
    pub fn is_available(&self) -> bool {
        self.runner_path.exists()
            && self.model_path.exists()
            && self.tokens_path.exists()
            && self.vad_model_path.exists()
    }

    /// 转写本地 WAV 音频文件
    pub fn transcribe<P: AsRef<Path>>(
        &self,
        audio_path: P,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<crate::engines::SegmentProgressCb>,
    ) -> Result<(Vec<Segment>, f64)> {
        let audio_path = audio_path.as_ref();
        info!(
            "SenseVoice 启动极速非自回归转写: {:?}, 语言: {:?}, 线程: {}",
            audio_path,
            language,
            clamp_threads(threads.unwrap_or(self.threads))
        );

        let mut cmd = Command::new(&self.python_path);
        // 让路模式必须在这里生效：SenseVoice 走 Python 子进程，长音频下
        // 多进程并行会吃满 CPU。只设 CREATE_NO_WINDOW 等于忽略用户的让路开关。
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        let th = clamp_threads(threads.unwrap_or(self.threads));
        cmd.arg(&self.runner_path)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--tokens")
            .arg(&self.tokens_path)
            .arg("--vad-model")
            .arg(&self.vad_model_path)
            .arg("--input")
            .arg(audio_path)
            .arg("--threads")
            .arg(th.to_string())
            .arg("--language")
            .arg(language.unwrap_or("auto"))
            .arg("--provider")
            .arg(&self.provider);

        if let Some(dur) = total_duration {
            if dur > 0.0 {
                cmd.arg("--total-duration").arg(dur.to_string());
            }
        }

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.run_process_and_parse(cmd, None, progress_cb)
    }

    /// 纯内存管道推流转写：直接从流（Read）中泵入 PCM WAV 字节，0 磁盘 I/O 往返
    pub fn transcribe_stream(
        &self,
        stream: Box<dyn std::io::Read + Send + 'static>,
        language: Option<&str>,
        threads: Option<u32>,
        total_duration: Option<f64>,
        progress_cb: Option<crate::engines::SegmentProgressCb>,
    ) -> Result<(Vec<Segment>, f64)> {
        info!("SenseVoice 启动纯内存管道非自回归推流转写 (0 磁盘 I/O)");

        let mut cmd = Command::new(&self.python_path);
        // 让路模式必须在这里生效：SenseVoice 走 Python 子进程，长音频下
        // 多进程并行会吃满 CPU。只设 CREATE_NO_WINDOW 等于忽略用户的让路开关。
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        let th = clamp_threads(threads.unwrap_or(self.threads));
        cmd.arg(&self.runner_path)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--tokens")
            .arg(&self.tokens_path)
            .arg("--vad-model")
            .arg(&self.vad_model_path)
            .arg("--input")
            .arg("-")
            .arg("--threads")
            .arg(th.to_string())
            .arg("--language")
            .arg(language.unwrap_or("auto"))
            .arg("--provider")
            .arg(&self.provider);

        if let Some(dur) = total_duration {
            if dur > 0.0 {
                cmd.arg("--total-duration").arg(dur.to_string());
            }
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.run_process_and_parse(cmd, Some(stream), progress_cb)
    }

    fn run_process_and_parse(
        &self,
        mut cmd: Command,
        input_stream: Option<Box<dyn std::io::Read + Send + 'static>>,
        progress_cb: Option<crate::engines::SegmentProgressCb>,
    ) -> Result<(Vec<Segment>, f64)> {
        // 本轮结果是否被截断：必须在任何读取线程启动前清零，保证读到的永远是
        // **本次运行**的事实（上一轮的失败不能污染本轮）。
        self.output_truncated.store(false, Ordering::SeqCst);
        // spawn 前的取消早退：若取消在准备区（拼参数、选解释器…）期间就已到达，就没必要
        // 真的把 runner 拉起来再靠 register_child 的复查把它杀掉——那几毫秒里进程创建与
        // Python 解释器启动已经白做一遍，用户看到的是「点了终止却还闪了一下子进程」。
        // 这里沿用与 `wait()` 之后取消分支完全相同的返回（空结果 + 0.0），由管线按取消
        // 收尾，不发明新的错误类型或文案。
        //
        // run_process_and_parse 之前的准备区（`transcribe` / `transcribe_stream`）只拼了 `cmd`
        // 并已把它 move 进来，不持有需要手工清理的临时文件/JSON 守卫（SenseVoice 靠子进程
        // stdout 逐行流式 JSON，不落盘中转文件），因此这里提前 return 没有额外的守卫回收负担。
        //
        // 与 register_child 复查的分工：本处只覆盖「取消早于 spawn 请求」；取消落在本处与
        // register_child 之间时，仍由那处复查补杀。两者是串联的两道门，不会互相干扰。
        if self.cancel.load(Ordering::SeqCst) {
            return Ok((Vec::new(), 0.0));
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("启动 SenseVoice 进程失败: {:?}", self.runner_path))?;
        crate::utils::child_registry::adopt(&child);

        // 登记子进程 PID，供用户「终止转写」时强杀。
        // 用 RAII 守卫（与 whisper 引擎复用同一份实现）而不是「函数末尾手动 retain」：
        // 后续任何 `?` / 提前 return / 取消分支 / panic 展开都会经由 Drop 把这一行摘掉，
        // 不会留下会被 PID 复用误伤的陈旧条目。正常跑完时还要在 `wait()` 返回处
        // 主动 `disarm()`，见其文档。
        let (mut child_guard, cancelled_before_register) = self.register_child(child.id());
        // 竞态收口：cancel() 可能恰好插在 spawn() 成功与上面的登记之间——那会儿本行
        // 还没进表，cancel() 的 take_live_pids() 拿到空名单，强杀信号就丢了，runner
        // 会一直占着 GPU 跑到自己结束。register_child 已用 SeqCst 全序保证「drain 没
        // 看到本行」必然意味着「本处复查命中」，所以这里补一发即可闭合窗口。
        // 此刻 child 仍持有进程句柄、尚未 wait()，PID 不可能被系统复用给无关进程。
        if cancelled_before_register {
            kill_process_tree(child.id());
        }

        // 若有内存音频流输入，启动泵送线程写入子进程 stdin
        let stream_handle = if let Some(stream) = input_stream {
            let stdin = child
                .stdin
                .take()
                .context("获取 SenseVoice 标准输入管道失败")?;
            Some(std::thread::spawn(move || {
                use std::io::{copy, BufReader, BufWriter, Write};
                let mut reader = BufReader::with_capacity(128 * 1024, stream);
                let mut writer = BufWriter::with_capacity(128 * 1024, stdin);
                // `let _ =` 把写失败（runner 提前退出、管道断开）吞得干干净净：
                // 上层只会看到「结果为空/偏短」，日志里没有任何线索。
                if let Err(err) = copy(&mut reader, &mut writer) {
                    warn!(
                        error = %err,
                        "SenseVoice 音频流泵送中断：runner 可能已提前结束，stdin 写入失败"
                    );
                }
                if let Err(err) = writer.flush() {
                    warn!(error = %err, "SenseVoice 音频流泵送收尾 flush 失败");
                }
                // writer 与 stdin 离开作用域自动 drop，向 runner 发送 EOF
            }))
        } else {
            None
        };

        let stdout = child
            .stdout
            .take()
            .context("获取 SenseVoice 标准输出管道失败")?;
        let stderr = child
            .stderr
            .take()
            .context("获取 SenseVoice 标准错误管道失败")?;

        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut err_str = String::new();
            // `let _ =` 把读失败吞成空串：失败分支随后会给出「无任何输出」的误导提示
            // （见 `stderr_str.trim().is_empty()`），而真正的事实是诊断通道断了或只读
            // 到一半。至少留下一条带字节数的日志。
            if let Err(err) = std::io::BufReader::new(stderr).read_to_string(&mut err_str) {
                warn!(
                    error = %err,
                    bytes = err_str.len(),
                    "读取 SenseVoice 标准错误失败：诊断信息可能被截断"
                );
            }
            err_str
        });

        let mut segments = Vec::new();
        let mut inference_elapsed = 0.0f64;

        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        // 读失败（非 EOF）时 `drain_stdout_lines` 返回 Err：这件事必须能被带出去，
        // 而不是像 `line_res.unwrap_or_default()` 那样变成一行空文本、再被
        // `trimmed.is_empty()` 丢掉。
        let mut unparsed_lines = 0usize;
        let stdout_read = drain_stdout_lines(reader.lines(), "SenseVoice runner", |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // 闭包里的 `return` 等价于原 for 循环里的 `continue`。
                return;
            }

            let parsed = serde_json::from_str::<SenseVoiceStreamLine>(trimmed);
            if let Err(err) = &parsed {
                // runner 协议是逐行 JSON：解不开的行以前被完全静默地丢掉，版本不匹配
                // 时用户只会看到「一句都没有」。只报首条（带截断预览）避免刷屏，
                // 收尾再汇总总数。
                if unparsed_lines == 0 {
                    let preview: String = trimmed.chars().take(160).collect();
                    warn!(
                        error = %err,
                        preview = %preview,
                        "SenseVoice runner 输出了无法解析的行：已忽略"
                    );
                }
                unparsed_lines += 1;
                return;
            }
            if let Ok(parsed) = parsed {
                match parsed.event.as_str() {
                    "segment" => {
                        let seg = Segment {
                            index: parsed.index,
                            start: parsed.start,
                            end: parsed.end,
                            text: parsed.text.clone(),
                            translation: None,
                            translation_lang: None,
                            polished: parsed.text.clone(),
                            language: None,
                            confidence: None,
                            speaker: None,
                            words: Vec::new(),
                        };
                        segments.push(seg.clone());
                        if let Some(ref cb) = progress_cb {
                            let label = format!("SenseVoice 极速转写中: 第 {} 句", parsed.index);
                            // 与 Whisper 推流出口同源：Python runner 已按标点细切，但若某段
                            // 仍超过 6s / 60 字（句内无可用切点），预览还得过一遍 Rust 拆分，
                            // 否则界面实时流会比最终字幕表粗一档（最终表由 `optimize_segments`
                            // 走同一个 `split_long_segments`）。
                            for piece in crate::subtitle::split_long_segments(vec![seg]) {
                                cb(parsed.progress, &label, Some(piece));
                            }
                        }
                    }
                    "finished" => {
                        inference_elapsed = parsed.elapsed_sec;
                        if segments.is_empty() && !parsed.segments.is_empty() {
                            for item in parsed.segments {
                                // runner 的 `polished` 与 `text` 目前同值，但字段是它主动
                                // 声明的（`all_segments` 里两者并列）：直接拿 `text` 顶上
                                // 等于把「已润色文本」这条通道静默掐掉——将来 runner 若
                                // 输出真正的标点恢复文本，这里会一声不响地丢弃。
                                let polished = if item.polished.trim().is_empty() {
                                    item.text.clone()
                                } else {
                                    item.polished
                                };
                                segments.push(Segment {
                                    index: item.index,
                                    start: item.start,
                                    end: item.end,
                                    text: item.text,
                                    translation: None,
                                    translation_lang: None,
                                    polished,
                                    language: None,
                                    confidence: None,
                                    speaker: None,
                                    words: Vec::new(),
                                });
                            }
                        }
                    }
                    "provider" => {
                        // runner 主动回报「请求 vs 实际」；比解析 stderr 文本稳。
                        self.record_provider(&parsed.requested, &parsed.actual);
                    }
                    // 未知事件一律忽略：协议只增不改，旧引擎遇到新事件必须安全跳过。
                    _ => {}
                }
            }
        });
        if unparsed_lines > 0 {
            warn!(
                unparsed_lines,
                "SenseVoice runner 输出中有无法解析的行（协议版本不匹配？详见前面那条日志）"
            );
        }
        if stdout_read.is_err() {
            // 「输出被截断」的正式信号：调用方（管线 / UI）据此提示用户，
            // 而不是把半份结果当成完整结果。
            self.output_truncated.store(true, Ordering::SeqCst);
        }

        let wait_result = child.wait();
        // 进程已被 wait() 回收：立刻摘掉登记行，杜绝 cancel() 误杀可能复用该 PID 的
        // 无关进程（详见 ChildPidGuard::disarm）。放在 wait() 紧后面，是为了不让
        // 下面的线程 join 把这段窗口拉长。
        child_guard.disarm();
        // 注销交给 child_guard：这里不再手写 retain，避免「新增一条提前返回路径
        // 就得记得再补一次」的维护陷阱（PID 的登记与注销由同一个栈变量负责）。
        let status = wait_result.with_context(|| "等待 SenseVoice 进程结束失败")?;
        if let Some(h) = stream_handle {
            // 泵送线程 panic 以前被 `let _ = h.join()` 静默吞掉：这里至少留一条日志
            // （`join_or_log` 内部 error!），具体失败原因仍由 runner 退出码/stderr 兜底呈现。
            let _ = join_or_log(h, "SenseVoice 音频流泵送线程");
        }
        // stderr 读取线程 panic 以前被 `unwrap_or_default()` 吞成空串：用户看到的是
        // 「退出码非 0 但无任何输出」的误导提示，真正原因（诊断通道坏了）完全不可见。
        // 这里显式记录状态：空串保留「无可用数据」的既有语义，由下面的进程失败分支
        // 给出带退出码的上下文报错；进程成功时仅降级诊断信息，不吃掉转写结果。
        let (stderr_str, stderr_thread_panicked) =
            match join_or_log(stderr_handle, "SenseVoice stderr 读取线程") {
                Some(s) => (s, false),
                None => (String::new(), true),
            };

        // 兜底通道：runner 未回传 `provider` 事件（旧版脚本）时，从 stderr 特征串识别
        // 回落。`record_provider` 首次为准，故与事件通道同时命中也不会重复告警。
        if stderr_provider_fallback(&stderr_str) {
            let requested = self.provider.clone();
            self.record_provider(&requested, "cpu");
        }

        // 用户主动终止：被杀进程的退出码不重要，按空结果返回，由管线走取消收尾
        if self.cancel.load(Ordering::SeqCst) {
            return Ok((Vec::new(), 0.0));
        }

        if !status.success() {
            // stderr 读取线程 panic 时 stderr_str 必为空，继续走下面「无输出＝Python 解释器或
            // 依赖未就位」的提示会把用户引向错误方向：真正的事实是诊断通道本身坏了。
            if stderr_thread_panicked {
                anyhow::bail!(
                    "SenseVoice 识别失败：runner 退出码 {:?}，且读取 stderr 的线程异常退出（panic），无法获取具体错误详情。请确认 python 可执行后重试；若可复现请保留 logs/ 中的日志反馈。",
                    status.code()
                );
            }
            warn!(
                "SenseVoice 退出状态非 0: {:?}, 错误日志: {}",
                status.code(),
                stderr_str
            );
            // stderr 为空时的裸报错等于让用户猜谜：python 解释器找不到、依赖缺失
            // 这类启动失败往往只留下退出码。按可操作的方向补一句提示。
            if stderr_str.trim().is_empty() {
                anyhow::bail!(
                    "SenseVoice 识别失败：runner 退出码 {:?} 且无任何输出。\
                     通常是 Python 解释器或依赖未就位——请确认「性能设置 → 模型与组件」里的 \
                     python 路径正确，且已安装 sherpa-onnx / numpy（见 tools/sensevoice_runner.py 头部说明）。",
                    status.code()
                );
            }
            anyhow::bail!("SenseVoice 识别失败: {}", stderr_str.trim());
        }

        // 智能优化时间轴：消除 100ms 重叠鬼影、消除时间重叠冲突、广播级短句延展平滑
        crate::subtitle::optimize_segments(&mut segments);

        if let Some(ref cb) = progress_cb {
            cb(
                1.0,
                &format!("SenseVoice 转写完成，共 {} 句", segments.len()),
                None,
            );
        }

        info!(
            segments = segments.len(),
            elapsed = inference_elapsed,
            "SenseVoice 转写完成"
        );
        Ok((segments, inference_elapsed))
    }
}

/// 逐行消费 runner 的 stdout（逐行 JSON 流），每行解码后交给 `on_line`。
///
/// 返回 `Ok(())` = 读到 EOF，**正常结束**；返回 `Err(..)` = **读失败**（非常 EOF）
/// 中止，也就是「输出被截断」。
///
/// 为什么要专门区分：旧写法 `let line = line_res.unwrap_or_default();` 把读错误变成
/// 一行空文本，紧接着被 `trimmed.is_empty()` 丢掉——「管道读到一半坏了」与「runner
/// 正常收尾」在结果上完全同形，用户只看到字幕少了一截，日志里毫无线索。
/// 迭代器由参数注入，单测可以直接喂一条「读一半就报错」的假 stdout，不必拉起 Python。
fn drain_stdout_lines<I>(lines: I, what: &str, mut on_line: impl FnMut(&str)) -> std::io::Result<()>
where
    I: IntoIterator<Item = std::io::Result<String>>,
{
    for line_res in lines {
        match line_res {
            Ok(line) => on_line(&line),
            Err(err) => {
                warn!(
                    error = %err,
                    stdout = what,
                    "读取标准输出失败：转写输出被截断（这不是正常结束）"
                );
                return Err(err);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ChildPidGuard：子进程 PID 登记的生命周期 ----
    //
    // 守卫实现与 whisper 引擎共用（`super::whisper::ChildPidGuard`），但登记表是
    // 每个引擎实例自己的一份，所以这里必须再验一遍：一旦 SenseVoice 侧漏掉登记或
    // 注销，`cancel()` 就会拿着陈旧 PID 去 `kill_process_tree`，误杀复用该 PID 的
    // 无关进程。前四条与 whisper 侧等价；后两条直接跑真实调用路径。

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
        // 这一行对应 run_process_and_parse 里 `child.stdout.take()...?`：
        // 它在登记之后提前返回，过去会把 PID 永久留在表里
        Err("simulated pipe failure")?;
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
        assert!(registered(&registry).is_empty(), "子进程结束后守卫负责注销");
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

    /// 用假路径造一个引擎：这些测试只关心 PID 表的生命周期，不需要真模型。
    fn engine_with_dummy_paths() -> SenseVoiceEngine {
        SenseVoiceEngine::new("runner.py", "model.onnx", "tokens.txt", "vad.onnx", 1)
    }

    /// 真实调用路径 1：`run_process_and_parse` 里登记之后、`wait()` 之前的
    /// `child.stdout.take().context(..)?` 提前返回。这里故意不 piped stdout，
    /// 让它走进那条 `?`，断言登记表被摘干净。
    #[cfg(target_os = "windows")]
    #[test]
    fn early_return_inside_run_process_and_parse_deregisters_pid() {
        let engine = engine_with_dummy_paths();
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stderr(Stdio::piped());
        // 注意：不设置 stdout 管道 → 触发「获取 SenseVoice 标准输出管道失败」

        let err = engine
            .run_process_and_parse(cmd, None, None)
            .expect_err("stdout 未接管时必须提前返回错误");
        assert!(
            err.to_string().contains("标准输出"),
            "应当是从 stdout 管道那条 ? 提前返回，实际: {err}"
        );
        assert!(
            registered(&engine.active_children).is_empty(),
            "登记之后提前返回，PID 必须被守卫摘掉，不能留下会被 cancel 误杀的陈旧条目"
        );
    }

    /// 真实调用路径 2：正常跑完（子进程立刻退出）后登记表也必须清空，
    /// 证明注销确实由守卫负责，而不是靠函数末尾手写的那一行。
    #[cfg(target_os = "windows")]
    #[test]
    fn normal_exit_inside_run_process_and_parse_deregisters_pid() {
        let engine = engine_with_dummy_paths();
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (segments, _) = engine
            .run_process_and_parse(cmd, None, None)
            .expect("空输出的子进程应当正常结束");
        assert!(segments.is_empty(), "没有 JSON 输出就不该有字幕片段");
        assert!(
            registered(&engine.active_children).is_empty(),
            "子进程结束后登记表必须为空"
        );
    }

    /// 取消路径：`cancel()` 取走并清空登记表，之后守卫析构不应再影响别的条目。
    #[test]
    fn cancel_drains_registry_and_guard_drop_is_afterwards_harmless() {
        let engine = engine_with_dummy_paths();
        // 登记一个不存在的 PID：cancel() 会去 kill_process_tree 一个空号，
        // 这在 Windows 上只是 taskkill 报「找不到进程」，不会误伤真实进程。
        let guard = ChildPidGuard::register(engine.active_children.clone(), u32::MAX);
        assert_eq!(registered(&engine.active_children).len(), 1);

        engine.cancel();
        assert!(
            registered(&engine.active_children).is_empty(),
            "cancel() 必须取走全部条目，防止同一 PID 被重复强杀"
        );

        drop(guard);
        assert!(
            registered(&engine.active_children).is_empty(),
            "守卫在 cancel 之后析构不应把表弄乱"
        );
    }

    /// 核心回归：`wait()` 回收 runner 后立刻 `disarm()`，`cancel()` 就不能再拿到这个 PID。
    ///
    /// `take_live_pids` 是 `cancel()` 唯一的强杀名单来源，断言它看不到该 PID，就等价于
    /// 断言 `cancel()` 不会对它发信号——且完全不依赖真实子进程的时序。
    #[test]
    fn cancel_never_sees_pid_disarmed_after_reap() {
        // 对照组：未回收的 PID 必须能被 cancel() 拿到，否则「终止转写」杀不掉正在跑的进程
        let control = empty_registry();
        let _live = ChildPidGuard::register(control.clone(), 4321);
        assert_eq!(take_live_pids(&control), vec![4321]);

        // 主用例：模拟 `child.wait()` 返回后立刻 disarm（PID 已归还系统）
        let engine = engine_with_dummy_paths();
        let mut guard = ChildPidGuard::register(engine.active_children.clone(), 4321);
        guard.disarm();
        assert!(
            registered(&engine.active_children).is_empty(),
            "disarm 必须立刻把登记行摘掉"
        );
        assert!(
            take_live_pids(&engine.active_children).is_empty(),
            "已回收的 PID 不得出现在 cancel() 的强杀名单里，否则会误杀复用该 PID 的无关进程"
        );
    }

    /// 回归：`cancel()` 恰好插在 `spawn()` 成功与 `register()` 之间——那会儿登记表
    /// 还是空的，`take_live_pids()` 取不到任何 PID，强杀名单为空。登记之后的复查必须
    /// 命中，等价于由转写线程补发一发强杀；否则这个刚起的 runner 会一直跑到自己结束。
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

        // 随后 runner 才 spawn 成功并登记：复查必须判定「要补杀」。
        let (_guard, needs_kill) = engine.register_child(4242);
        assert!(
            needs_kill,
            "登记后复查必须命中取消标志，否则取消会漏掉这个刚 spawn 的 runner"
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

    /// 端到端（真实子进程）：取消标志先置位，再让 `run_process_and_parse` 去拉一个
    /// 「本来要跑约 10 秒」的子进程。窗口没堵上时它会一直跑到自然结束；堵上后由
    /// spawn 前的取消早退直接返回（更早的关口），即便取消恰好落在早退与登记之间，
    /// 也会被登记后的复查立刻强杀，函数随即返回。
    ///
    /// 这里刻意用「10 秒 vs 5 秒」这种宽松边界来判，而不是卡毫秒：结论只由「取消有
    /// 没有生效」决定，不受调度抖动影响。Windows 才跑（taskkill / ping 都是系统自带）。
    /// 「到底有没有真的 spawn」由 `cancel_before_spawn_skips_process_creation` 精确判定，
    /// 本用例只负责「取消让长跑命令不拖满 10 秒」这一端到端行为。
    #[cfg(target_os = "windows")]
    #[test]
    fn cancelled_before_spawn_terminates_long_running_runner_quickly() {
        let engine = engine_with_dummy_paths();
        // 等价于「取消在 spawn 与 register 之间到达」：置位时表里还没有这一行。
        engine.cancel();

        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping", "-n", "11", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = std::time::Instant::now();
        let (segments, _) = engine
            .run_process_and_parse(cmd, None, None)
            .expect("被强杀的 runner 应当走取消路径正常返回");
        let elapsed = started.elapsed();

        assert!(segments.is_empty(), "取消后不应产出任何字幕片段");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "取消必须让长跑 runner 提前结束（实测 {elapsed:?}），而不是等它自然跑完约 10 秒"
        );
    }

    /// spawn 前早退的直接断言：取消已置位时，`run_process_and_parse` 必须立刻返回与既有
    /// 取消分支完全相同的 `Ok((空, 0.0))`，且**根本没有走到 `cmd.spawn()`**。
    ///
    /// 「没 spawn」怎么断言，而不是「spawn 了又杀」：这里注入一个**必然创建失败**的命令
    /// ——TEMP 下一个不存在的可执行文件。若代码仍会走到 `cmd.spawn()`，spawn 会直接返回
    /// 「启动 SenseVoice 进程失败」的 `Err`，函数不可能给出 `Ok`；既然实际拿到
    /// `Ok((空, 0.0))`，唯一的解释就是早退在 spawn 之前就短路了。对照用例
    /// `without_cancel_missing_runner_reports_spawn_failure` 证明这个哨兵命令确实会创建
    /// 失败，从而排除「spawn 成功后再被复查补杀」这条路——那条路仍需要一次成功的 spawn。
    #[test]
    fn cancel_before_spawn_skips_process_creation() {
        let engine = engine_with_dummy_paths();
        engine.cancel();

        let missing = std::env::temp_dir().join("v2w_probe_missing_runner_9f3a.exe");
        assert!(
            !missing.exists(),
            "哨兵命令路径必须不存在，否则本用例失去区分力"
        );
        let mut cmd = Command::new(&missing);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (segments, elapsed_sec) = engine
            .run_process_and_parse(cmd, None, None)
            .expect("取消早退必须走既有取消路径，返回 Ok(空结果) 而不是创建失败的错误");
        assert!(segments.is_empty(), "取消后不应产出任何字幕片段");
        assert_eq!(elapsed_sec, 0.0, "取消早退沿用既有取消分支的 0.0 时长");
        assert!(
            registered(&engine.active_children).is_empty(),
            "早退路径连登记都不该发生，登记表必须为空"
        );
    }

    /// 对照：不置位取消时，同一个不存在的 runner 会让 spawn 真的失败并报
    /// 「启动 SenseVoice 进程失败」。这证明上一个用例里的 `Ok(空, 0.0)` 成因只能是
    /// 「取消早退短路了 spawn」，而不是别的路径碰巧也返回了 Ok。
    #[test]
    fn without_cancel_missing_runner_reports_spawn_failure() {
        let engine = engine_with_dummy_paths();
        // 刻意不 cancel。

        let missing = std::env::temp_dir().join("v2w_probe_missing_runner_9f3a.exe");
        let mut cmd = Command::new(&missing);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let err = engine
            .run_process_and_parse(cmd, None, None)
            .expect_err("未取消时必须真的尝试 spawn，哨兵命令创建失败应报错");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("启动 SenseVoice 进程失败"),
            "错误应来自 spawn 失败这条路径，实际: {msg}"
        );
    }

    /// 痕迹法：取消已置位时，一个「一旦被启动就会写下痕迹文件」的真实命令不得留下痕迹。
    ///
    /// 与哨兵用例互补：哨兵用「spawn 必然失败」精确判定「没走到 spawn」；本用例用真实命令
    /// 确认「没有子进程被拉起来执行」。注意单看痕迹缺失并不足以完全排除「spawn 成功后被
    /// register 复查立刻 taskkill」的时序——那种情况下子进程可能还没来得及执行到写文件
    /// 那一句就被杀掉，痕迹同样可能缺失。因此**判定「真的没 spawn」以哨兵用例为准**，
    /// 本用例只作「无残留痕迹」的旁证。
    #[cfg(target_os = "windows")]
    #[test]
    fn cancel_before_spawn_leaves_no_trace_of_child() {
        let engine = engine_with_dummy_paths();
        engine.cancel();

        let marker = std::env::temp_dir().join(format!(
            "v2w_spawn_trace_{}_{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&marker);

        let mut cmd = Command::new("cmd");
        cmd.args(["/C", &format!("echo spawned> \"{}\"", marker.display())])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let (segments, _) = engine
            .run_process_and_parse(cmd, None, None)
            .expect("取消早退应正常返回空结果");
        let traced = marker.exists();
        let _ = std::fs::remove_file(&marker);

        assert!(segments.is_empty());
        assert!(
            !traced,
            "取消已置位时不得真的拉起子进程（否则会留下痕迹文件）"
        );
    }

    // ---- provider 回落可见化：判定纯函数 + 事件消费 ----

    #[test]
    fn fallback_notice_fires_when_dml_silently_falls_back() {
        let notice =
            provider_fallback_notice("dml", "cpu").expect("请求 dml 却落到 cpu，必须给出告警文案");
        assert!(notice.contains("dml"), "告警应点出请求值: {notice}");
        assert!(notice.contains("cpu"), "告警应点出实际值: {notice}");
        assert!(
            notice.contains("onnxruntime-directml"),
            "告警应给出可操作修复指引: {notice}"
        );
    }

    #[test]
    fn fallback_notice_is_silent_when_request_is_cpu() {
        assert!(
            provider_fallback_notice("cpu", "cpu").is_none(),
            "配置就是 cpu 时不该报警（本就无需 GPU）"
        );
    }

    #[test]
    fn fallback_notice_is_silent_when_provider_really_engaged() {
        assert!(
            provider_fallback_notice("dml", "dml").is_none(),
            "真吃到 DML 时不该报警"
        );
        assert!(
            provider_fallback_notice("directml", "directml").is_none(),
            "真吃到 DirectML 时不该报警"
        );
    }

    #[test]
    fn fallback_notice_tolerates_case_and_whitespace() {
        assert!(
            provider_fallback_notice(" DML ", "CPU").is_some(),
            "大小写/空白差异不应漏报回落"
        );
        assert!(
            provider_fallback_notice("", "cpu").is_none(),
            "空请求值视作未配置，不报警"
        );
    }

    #[test]
    fn stderr_scan_detects_sherpa_fallback_markers() {
        assert!(stderr_provider_fallback(
            "csrc/provider.cc:StringToProvider:35 Unsupported string: dml. Fallback to cpu"
        ));
        assert!(stderr_provider_fallback(
            "csrc/session.cc:GetSessionOptionsImpl:369 DirectML is for Windows only. Fallback to cpu!"
        ));
        assert!(stderr_provider_fallback(
            "session.cc:351 Available providers: CPUExecutionProvider, . Fallback to cpu!"
        ));
        assert!(
            !stderr_provider_fallback("Silero VAD 模型未找到"),
            "无关 stderr 不得误报回落"
        );
    }

    #[test]
    fn provider_status_fell_back_reflects_request_vs_actual() {
        let fell_back = ProviderStatus {
            requested: "dml".into(),
            actual: "cpu".into(),
        };
        assert!(fell_back.fell_back());

        let engaged = ProviderStatus {
            requested: "dml".into(),
            actual: "dml".into(),
        };
        assert!(!engaged.fell_back());

        let plain_cpu = ProviderStatus {
            requested: "cpu".into(),
            actual: "cpu".into(),
        };
        assert!(!plain_cpu.fell_back());
    }

    /// runner 回报的 `provider` 事件必须被消费成引擎状态；告警只打一次。
    #[test]
    fn provider_event_is_recorded_and_only_reported_once() {
        let engine = engine_with_dummy_paths();
        assert!(engine.provider_status().is_none(), "未回报前应为 None");

        engine.record_provider("dml", "cpu");
        let status = engine.provider_status().expect("回报后应有实况");
        assert_eq!(status.requested, "dml");
        assert_eq!(status.actual, "cpu");
        assert!(status.fell_back());

        // 首个回报为准：并行 worker 的后续重复回报不得改写已记录的实况。
        engine.record_provider("dml", "dml");
        assert_eq!(
            engine.provider_status().expect("实况仍在").actual,
            "cpu",
            "首个回报为准，后续重复不得覆盖"
        );
    }

    /// `provider` 事件与 stderr 兜底同时命中时，实况仍以首个为准、不重复告警。
    #[test]
    fn stderr_fallback_does_not_override_recorded_provider() {
        let engine = engine_with_dummy_paths();
        engine.record_provider("dml", "cpu");
        engine.record_provider("dml", "cpu");
        assert_eq!(engine.provider_status().unwrap().actual, "cpu");
    }

    /// 回落告警只在「slot 首次写入」那一次发出；后到的矛盾回报既不重复告警，
    /// 也不覆盖已有状态。
    ///
    /// 缺陷面：旧实现拿**本次入参**去算告警，于是同一次运行里可能先后打出互相
    /// 矛盾的两条：一条按「dml 回落 cpu」报「已回落到 CPU」，另一条按后到的
    /// 「dml 吃到 dml」报「将使用 GPU」；且 `fallback_reported` 在第一次就被后到的
    /// 非回落入参 swap 成 true，把本该只报一次的一次性额度吃掉，后续真正的首次回落
    /// 反而静默。
    ///
    /// 告警经 tracing 宏发出，测试里不便拦截，故把「是否应告警 + 告警文案」
    /// 抽成纯函数 `provider_report_outcome` 直接断言；同一入参序列也对引擎状态机
    /// 验一遍，保证纯函数与真实 `record_provider` 口径一致。
    #[test]
    fn fallback_warning_fires_once_per_slot_transition() {
        // 场景：slot 为空时首次回报 dml 回落 cpu，随后同一 slot 又来一条 dml 吃到 dml。
        let first = provider_report_outcome(
            None,
            &ProviderStatus {
                requested: "dml".into(),
                actual: "cpu".into(),
            },
        );
        assert_eq!(
            first.stored.as_ref().map(|s| s.actual.as_str()),
            Some("cpu"),
            "首次回报必须写入 slot（落盘的是实况）"
        );
        let notice = first
            .warns_on_this_call
            .as_deref()
            .expect("首次回报就是回落，必须产生告警记录");
        assert!(
            notice.contains("dml") && notice.contains("cpu"),
            "告警文案必须取自落盘实况（dml→cpu），而不是后到的入参: {notice}"
        );

        // 第二次回报：slot 已有实况，既不覆盖状态，也不得再产生告警。
        let seen = first.stored.as_ref();
        let second = provider_report_outcome(
            seen,
            &ProviderStatus {
                requested: "dml".into(),
                actual: "dml".into(),
            },
        );
        assert_eq!(
            second.stored, None,
            "slot 已有实况后，后到的矛盾回报不得覆盖状态"
        );
        assert_eq!(
            second.warns_on_this_call, None,
            "一次 slot 转换只告警一次：第二次回报不得重复告警"
        );

        // 同一入参序列走一遍真实 record_provider：状态机口径必须与纯函数一致。
        let engine = engine_with_dummy_paths();
        engine.record_provider("dml", "cpu");
        engine.record_provider("dml", "dml");
        assert_eq!(
            engine.provider_status().unwrap().actual,
            "cpu",
            "真实状态机同样以首次回报为准，矛盾值不得覆盖"
        );
    }

    // ---- 单会话线程数夹取：必须与文档/设置页量程一致 ----
    //
    // 文档（MAX_THREADS 注释）写明「单会话与切块回退路径都夹到 8」，设置页滑条
    // 量程也由 MIN_THREADS/MAX_THREADS 推导；但 `whisper_threads` 是全局共享项
    // （16 核机器可设到 16），CLI/GUI 原样透传。过去只有 `threads.max(1)`，
    // 于是配 9~16 会真的以 9~16 线程起 runner，越配越慢。

    #[test]
    fn clamp_threads_holds_documented_range() {
        assert_eq!(clamp_threads(0), MIN_THREADS, "0 必须抬到下限");
        assert_eq!(
            clamp_threads(1),
            MIN_THREADS,
            "1 低于下限，抬到 MIN_THREADS"
        );
        assert_eq!(clamp_threads(MIN_THREADS), MIN_THREADS, "下限本身不变");
        assert_eq!(clamp_threads(MAX_THREADS), MAX_THREADS, "上限本身不变");
        assert_eq!(clamp_threads(9), MAX_THREADS, "9 超上限，夹到 MAX_THREADS");
        assert_eq!(
            clamp_threads(16),
            MAX_THREADS,
            "16 超上限，夹到 MAX_THREADS"
        );
    }

    /// 构造器必须把入参夹到有效区间，而不是只夹下界（旧实现为 `threads.max(1)`）。
    #[test]
    fn constructor_clamps_threads_to_documented_range() {
        let hi = SenseVoiceEngine::new("r.py", "m.onnx", "t.txt", "v.onnx", 16);
        assert_eq!(
            hi.threads, MAX_THREADS,
            "构造器必须把 16 夹到 MAX_THREADS，否则真的会以 16 线程起 runner"
        );
        let lo = SenseVoiceEngine::new("r.py", "m.onnx", "t.txt", "v.onnx", 0);
        assert_eq!(lo.threads, MIN_THREADS, "构造器必须把 0 抬到 MIN_THREADS");
    }

    // ───────── 读失败 ≠ 正常结束：管道故障不再静默变成「少了几句话」 ─────────

    /// 回归：stdout **读失败**必须与正常结束（EOF）区分开。
    ///
    /// 旧写法 `line_res.unwrap_or_default()` 把读错误变成一行空文本、再被
    /// `trimmed.is_empty()` 丢掉，用户侧就是「字幕少了一截」；两件事必须能分开。
    /// 迭代器由参数注入，单测不必真的拉起 Python runner。
    #[test]
    fn drain_stdout_lines_separates_read_failure_from_eof() {
        // 1) 正常读完（EOF 由迭代器结束表达）：Ok(())，所有行都交付
        let mut got = Vec::new();
        let clean = vec![
            Ok("{\"event\":\"segment\"}".to_string()),
            Ok("x".to_string()),
        ];
        assert!(
            drain_stdout_lines(clean, "测试", |line| got.push(line.to_string())).is_ok(),
            "正常结束不是失败"
        );
        assert_eq!(
            got,
            vec!["{\"event\":\"segment\"}".to_string(), "x".to_string()]
        );

        // 2) 读到第二行时管道报错：必须 Err，且失败前读到的行要保留
        let mut got = Vec::new();
        let broken = vec![
            Ok("first".to_string()),
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "假管道：读失败",
            )),
        ];
        let outcome = drain_stdout_lines(broken, "测试", |line| got.push(line.to_string()));
        assert!(outcome.is_err(), "读失败必须与 EOF 分开表达");
        assert_eq!(got, vec!["first".to_string()], "失败前读到的行必须保留");
    }

    /// 「输出被截断」必须能从引擎读出来，并且是**一次取走即复位**的语义：
    /// 调用方（管线 / UI）据此提示用户「字幕可能不完整」。
    #[test]
    fn output_truncated_flag_is_readable_take_once_and_cleared_by_reset() {
        let engine = SenseVoiceEngine::new("r.py", "m.onnx", "t.txt", "v.onnx", 4);
        assert!(!engine.output_truncated(), "新引擎默认没有截断");

        engine.output_truncated.store(true, Ordering::SeqCst);
        assert!(engine.output_truncated(), "读失败后必须能读到截断信号");
        assert!(engine.take_output_truncated(), "取一次应为 true");
        assert!(!engine.output_truncated(), "取走即复位");

        engine.output_truncated.store(true, Ordering::SeqCst);
        engine.reset();
        assert!(
            !engine.output_truncated(),
            "reset（每轮任务开始调用）必须清掉上一轮的截断标记"
        );
    }
}
