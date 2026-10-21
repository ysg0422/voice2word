//! 阿里 SenseVoice-Small 极速非自回归语音识别引擎封装
//! 通过 sherpa-onnx 驱动 INT8 模型，单次前向出字，自带标点与 ITN，速度是 Whisper 的 5~8 倍。

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::subtitle::Segment;

use super::whisper::{join_or_log, live_child_pids, ChildEntry, ChildPidGuard};

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
///
/// `taskkill` 的路径加固此前只做在 whisper / punc 两处，本文件是裸
/// `Command::new("taskkill")`：Windows 创建进程时**当前目录排在系统目录之前**，
/// 于是应用目录里放一个同名 `taskkill.exe`，就会在用户点「终止转写」时被执行。
/// 三处口径不一致等于加固等于没有——现在统一取 whisper 那份。
///
/// 失败时留一条 warn：此前是 `let _ = ...`，终止信号没送到时用户看到的是
/// 「点了终止但转写照跑」，日志里没有任何线索可查。
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new(super::whisper::taskkill_program());
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        match cmd.args(["/F", "/T", "/PID", &pid.to_string()]).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => warn!(
                pid,
                code = ?out.status.code(),
                "taskkill 未能终止 SenseVoice 子进程树，转写可能仍在继续"
            ),
            Err(err) => warn!(
                pid,
                error = %err,
                "taskkill 启动失败，SenseVoice 子进程树可能仍在运行"
            ),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

// ───────────────────────── 有界等待、取消与输出收口 ─────────────────────────

/// 【停滞看门狗】进程仍活着、但**连续没有任何输出**达到该时长 ⇒ 判定卡死。
///
/// 为什么要有这一层：`wait_budget_for` 是**总时长**上限（按时长缩放），它拦得住
/// 「跑太久」，但拦不住「早就卡住了却还在等预算耗尽」——比如 onnxruntime 死锁、
/// 驱动挂起、管道互等。此时界面会一直停在「正在识别」，用户唯一出口是关掉应用。
///
/// 判据只能是「停滞」，绝不能是「总耗时」：长音频跑十几分钟是正常的。
///
/// 与 `whisper.rs::WHISPER_STALL_BUDGET` 同口径（300s）——三个引擎此前只有
/// whisper 有这道防线，sensevoice / punc 没有：同一个故障在不同引擎上的表现不一致。
pub(crate) const SV_STALL_BUDGET: Duration = Duration::from_secs(300);
/// 有界等待的轮询间隔：50ms 是「取消 / 超时的响应延迟」与「空转开销」的折中
/// （与 `punc.rs` 的有界等待取同一个值）。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 有界等待的固定开销预算（秒）：Python 冷启动 + onnxruntime / sherpa-onnx 动态库加载 +
/// INT8 模型与 VAD 权重装载。实测「进程创建 → 能读输入」约 2~5s，首轮磁盘冷缓存与低端机
/// 可达十几秒；120s 是宽松上限。
const WAIT_COLD_START_SEC: f64 = 120.0;

/// 每 1 秒音频容许的墙钟上限（秒）。
///
/// SenseVoice 是**非自回归单次前向**，官方口径比 Whisper 快 5~8 倍；即便按最保守的
/// 「略快于实时」估计，1 秒音频也远用不到 3 秒墙钟。取 3.0 是给低端 CPU、线程数夹取、
/// VAD 融合与磁盘抖动留的余量——量级对了，长音频才不会被误杀成「超时」。
const WAIT_SECONDS_PER_AUDIO_SEC: f64 = 3.0;

/// 时长未知（调用方拿不到媒体时长）时**假设**的音频长度（秒）。
///
/// 取 600s：SenseVoice 单会话推流路径的典型输入量级（切块阈值的同量级）。假设值只影响
/// 等待预算、不影响结果；它保证「时长缺失」时仍有一个明确、可解释的上限，而不是退化成
/// 「不设上限」。
const WAIT_ASSUMED_AUDIO_SEC: f64 = 600.0;

/// 有界等待的硬上限：无论音频多长、假设值多大，都不能让调用方无限等。
///
/// 1 小时。为什么不干脆不设上限：本项加固的目的就是把「runner 卡死 → 应用永久卡住、
/// 只能杀进程」降级成「有限时间后失败并给出可读原因」。为什么不再小：SenseVoice 单会话
/// 在 GPU 上可能一口吃掉整段长音频（推流路径不切块），2 小时素材按 5~8 倍实时也要十几到
/// 二十分钟。
const WAIT_BUDGET_MAX: Duration = Duration::from_secs(3600);

/// 按音频时长推导有界等待预算：`冷启动 + 每秒音频 3 秒墙钟`，夹在 [`WAIT_BUDGET_MAX`] 内。
///
/// 为什么不能只用一个固定常量：同一个引擎既要跑十秒级短视频，也要跑几小时长音频（GPU
/// 推流路径不切块，整个文件就是一次会话）。固定小预算会把长音频的正常推理误判成超时，
/// 固定大预算又等于对小任务不设防。按时长缩放 + 硬上限同时解决两头；它是个纯函数，
/// 取值理由可以在单测里直接钉住。
fn wait_budget_for(duration_secs: Option<f64>) -> Duration {
    let audio_sec = match duration_secs {
        Some(d) if d.is_finite() && d > 0.0 => d,
        _ => WAIT_ASSUMED_AUDIO_SEC,
    };
    let budget_sec = (WAIT_COLD_START_SEC + WAIT_SECONDS_PER_AUDIO_SEC * audio_sec)
        .min(WAIT_BUDGET_MAX.as_secs_f64());
    Duration::from_secs_f64(budget_sec)
}

/// 有界等待的出口（外加「等待本身失败」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// 子进程自行退出（退出码另行判断）。
    Exited,
    /// 用户点了「终止」。
    Cancelled,
    /// 到点仍未退出。
    TimedOut,
    /// `try_wait` 本身报错（句柄异常），不能死循环。
    WaitFailed,
    /// 进程仍存活但**连续无输出**达到停滞预算 ⇒ 判定卡死，已强杀进程树。
    Stalled,
}

/// 本次轮询是否该退出等待；`None` = 继续等。
///
/// 优先级：**取消 > 超时**。两者同时成立时按「已取消」处理——用户主动终止是更强的语义，
/// 文案也该显示「已取消」，而不是让用户以为自己撞上了超时。抽成纯函数是为了让这条优先级
/// 能在单测里直接钉住，不必真的起子进程、等满预算。
fn wait_outcome(cancelled: bool, past_deadline: bool) -> Option<WaitOutcome> {
    if cancelled {
        Some(WaitOutcome::Cancelled)
    } else if past_deadline {
        Some(WaitOutcome::TimedOut)
    } else {
        None
    }
}

/// 有界等待子进程退出：`budget` 到点判超时，`cancelled()` 为真判取消。
///
/// 返回「出口 + `try_wait` 拿到的退出状态」（非正常退出时为 `None`）。抽成独立函数是为了
/// 让三条出口都能在单测里用真实子进程 + 极小预算跑一遍，不必等满真实预算，也不依赖
/// python 与模型文件。本函数**不**负责杀进程与收尸：那是调用方的事（先 kill 再 wait），
/// 这样「谁在什么时候强杀」在调用点一眼可见。
#[cfg_attr(not(test), allow(dead_code))]
fn wait_for_child<F>(
    child: &mut Child,
    budget: Duration,
    poll: Duration,
    cancelled: F,
) -> (WaitOutcome, Option<std::process::ExitStatus>)
where
    F: Fn() -> bool,
{
    wait_for_child_with_stall(child, budget, poll, cancelled, None)
}

/// 带**停滞看门狗**的有界等待。
///
/// `stall_tracker` 给出「最后一次收到输出」的时间戳；进程仍活着、但距那次输出已超过
/// [`SV_STALL_BUDGET`] ⇒ 判定卡死（`WaitOutcome::Stalled`），由调用方强杀进程树。
///
/// 为什么不能只靠总时长预算（`wait_budget_for`）：它拦得住「跑太久」，拦不住
/// 「早就卡住了却还在等预算耗尽」——onnxruntime 死锁、驱动挂起、管道互等都属于后者。
/// 此时界面会一直停在「正在识别」，用户唯一出口是关掉应用。
///
/// 判据是「停滞」不是「总耗时」：长音频跑十几分钟是正常的。
fn wait_for_child_with_stall<F>(
    child: &mut Child,
    budget: Duration,
    poll: Duration,
    cancelled: F,
    stall_tracker: Option<&Arc<std::sync::Mutex<Instant>>>,
) -> (WaitOutcome, Option<std::process::ExitStatus>)
where
    F: Fn() -> bool,
{
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (WaitOutcome::Exited, Some(status)),
            Ok(None) => {}
            Err(err) => {
                // 等待本身出错（句柄异常）时不能死循环：按失败处理，由调用方强杀回收。
                warn!(error = %err, "等待 SenseVoice 子进程失败，终止该进程");
                return (WaitOutcome::WaitFailed, None);
            }
        }
        // 停滞看门狗：进程还活着，但已经很久没有任何输出了。
        if let Some(tracker) = stall_tracker {
            let last = *tracker.lock().unwrap_or_else(|e| e.into_inner());
            if Instant::now().saturating_duration_since(last) >= SV_STALL_BUDGET {
                warn!(
                    pid = child.id(),
                    budget_secs = SV_STALL_BUDGET.as_secs(),
                    "SenseVoice 连续无输出达到停滞预算，判定卡死，强制终止进程树"
                );
                return (WaitOutcome::Stalled, None);
            }
        }
        // 判定抽成纯函数：让「取消优先于超时」这条语义能被单测直接钉住。
        if let Some(outcome) = wait_outcome(cancelled(), Instant::now() >= deadline) {
            return (outcome, None);
        }
        std::thread::sleep(poll);
    }
}

/// SenseVoice 阶段的可辨识失败。
///
/// 与 `punc::PuncStageError` 同款：调用方可以 `downcast_ref` 出具体原因，不必靠错误文案
/// 里的关键字判断（一改文案就失效）。只有超时一个变体，是因为**取消不走错误通道**：管线
/// 在转写返回后用自己那份 `cancelled` 标志做静默收尾，本引擎若把取消改成 `Err`，用户点
/// 「终止」就会看到一条「转写失败」。取消仍可由 `SenseVoiceEngine::is_cancelled()` 读出，
/// `Ok((空, 0.0))` 这个既有契约也由既有单测钉住。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenseVoiceStageError {
    /// 超过等待上限仍未结束，已强杀进程树。
    TimedOut,
    /// 进程仍存活但连续无输出达到停滞预算，已强杀进程树。
    Stalled,
}

impl std::fmt::Display for SenseVoiceStageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => write!(
                f,
                "SenseVoice 识别超时，已终止本次转写；未产出字幕，请重试\
                 （若反复超时，可换更小的模型或降低并行进程数）"
            ),
            Self::Stalled => write!(
                f,
                "SenseVoice 长时间无输出，已判定卡死并终止本次转写；未产出字幕，请重试\
                 （若反复卡死，可换更小的模型、降低并行进程数，或检查磁盘与内存）"
            ),
        }
    }
}

impl std::error::Error for SenseVoiceStageError {}
#[derive(Debug, Deserialize)]
struct SenseVoiceStreamLine {
    #[serde(default)]
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

/// 识别模型族：决定 runner 的 `--model-type`，以及是否需要额外的 decoder 文件。
///
/// 为什么需要它：sherpa-onnx 这一个运行时能承载多个模型族（Dolphin / Paraformer /
/// FireRedASR / NeMo…），它们的 onnx 结构不同，runner 必须知道按哪种方式建会话。
/// **缺省必须是 SenseVoice**：现有配置与调用点都按它工作，加这个枚举不能改变任何
/// 既有行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelFamily {
    /// 阿里 SenseVoice（现状；也是 `--model-type` 的缺省值）
    #[default]
    SenseVoice,
    /// 阿里 Paraformer（非自回归，中文工业档）
    Paraformer,
    /// 小红书 FireRedASR（encoder + decoder 两个 onnx）
    FireRedAsr,
    /// DataoceanAI Dolphin（多语种，单 onnx）
    DolphinCtc,
    /// NVIDIA NeMo CTC（单 onnx）
    NemoCtc,
    /// NVIDIA NeMo Canary（encoder + decoder 两个 onnx）
    NemoCanary,
}

impl ModelFamily {
    /// runner 认的 `--model-type` 取值。
    pub fn runner_model_type(self) -> &'static str {
        match self {
            Self::SenseVoice => "sensevoice",
            Self::Paraformer => "paraformer",
            Self::FireRedAsr => "fire-red-asr",
            Self::DolphinCtc => "dolphin-ctc",
            Self::NemoCtc => "nemo-ctc",
            Self::NemoCanary => "nemo-canary",
        }
    }

    /// 该族是否必须额外给一个 decoder 文件。
    pub fn needs_decoder(self) -> bool {
        matches!(self, Self::FireRedAsr | Self::NemoCanary)
    }

    /// 追加到 runner 命令行的额外参数。
    ///
    /// **SenseVoice 恒为空**：runner 的 `--model-type` 缺省就是它。显式再传一遍虽然
    /// 等价，但会让「改造前后命令行逐字节一致」这条回归断言失去意义——保持空表，
    /// 老命令行原样不变。
    pub fn runner_extra_args(self, decoder: Option<&Path>) -> Vec<String> {
        if self == Self::SenseVoice {
            return Vec::new();
        }
        let mut args = vec![
            "--model-type".to_string(),
            self.runner_model_type().to_string(),
        ];
        if let Some(path) = decoder {
            args.push("--decoder".to_string());
            args.push(path.display().to_string());
        }
        args
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
    /// 识别模型族（缺省 SenseVoice，与改造前完全一致）。
    family: ModelFamily,
    /// 少数模型族（FireRedASR / Canary）需要的 decoder 文件。
    decoder_path: Option<std::path::PathBuf>,
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
            family: ModelFamily::default(),
            decoder_path: None,
            provider,
            provider_status: Arc::new(Mutex::new(None)),
            fallback_reported: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            active_children: Arc::new(Mutex::new(Vec::new())),

            // 本轮结果是否被截断：见 `output_truncated`。
            output_truncated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 用户是否已请求终止（与 whisper / punc 同名的只读入口）。
    ///
    /// 取消本身不走错误通道（管线按自己那份 `cancelled` 标志静默收尾，见 `cancel` 的说明），
    /// 所以调用方要区分「已取消」与「超时 / 普通失败」时读这个，而不是去匹配错误文案。
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// 用户请求终止：置位取消标志并强杀正在运行的 runner 子进程
    ///
    /// 只杀「尚未回收」的 PID：已被 `wait()` 回收的进程会在 `wait()` 返回处就被
    /// [`ChildPidGuard::disarm`] 摘掉，因此这里不会对着一个已归还系统、可能被复用
    /// 给无关进程的号码发信号。
    ///
    /// 这里取的是**快照**（`live_child_pids`）而不是 drain（`take_live_pids`）：
    /// drain 让「取消」变成一次性动作——用户连点两次「终止转写」，第二次取到的名单
    /// 是空的，如果第一次的 `taskkill` 因为竞态/权限没把子进程杀掉，用户就再也没有
    /// 第二次机会，界面表现就是「点了终止，转写照跑」。
    /// whisper 引擎此前已改成快照，sensevoice / punc 仍停在 drain——三处口径不一致
    /// 意味着「终止」在哪个引擎上生效取决于选了哪个模型。现在统一为快照。
    ///
    /// 快照不削弱「不得杀到陈旧 PID」：登记行仍然只在 `child.wait()` 返回处摘除，
    /// 而 Windows 上 PID 只有在进程对象最后一个句柄关闭后才可能被复用。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for pid in live_child_pids(&self.active_children) {
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
    /// 置位点只有一个：`consume_stdout` 遇到**非 EOF** 的读失败（旧写法
    /// `line_res.unwrap_or_default()` 把这种失败变成一行空文本、再被 `trimmed.is_empty()`
    /// 丢掉，与「runner 正常收尾」在结果上完全同形）。
    ///
    /// 与错误**同向**：同一次转写里 `run_process_and_parse` 会返回 Err（绝不把半截结果写回），
    /// 所以读到 `true` 只可能是「刚失败的那一轮」留下的信号，调用方（管线 / UI）据此给出
    /// 「本次结果不可用 / 字幕可能不完整」的提示，`reset()` 会在下一轮开头清掉它。
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
    /// 置位取消标志，再取**快照**强杀。若它恰好插在 `spawn()` 成功
    /// 与本次 `register()` 之间，登记表里还没有这一行，取到的名单是空的，这一发
    /// 信号就丢了——runner 会一直占着 GPU 跑到自己结束，用户看来就是「点了终止
    /// 转写却完全没生效」。
    ///
    /// 为什么在登记之后复查标志就能闭合：`cancel()` 的置位与本次复查都是 SeqCst，
    /// 二者有全序；而快照读取与本次 register 又在同一把 Mutex
    /// 下互斥。若 drain 没看到本行，只能是 drain 早于 register，那么置位也早于
    /// register，复查必然读到 `true`。于是「drain 发信号」与「本处补杀」两条路径
    /// 必有一条命中，不存在都漏。
    ///
    /// 返回值交给调用方去补 `kill_process_tree`，而不是在本函数里直接杀，是为了让
    /// 这条时序能在单测里直接断言，不必真的拉起子进程、发系统信号。
    fn register_child(&self, pid: u32) -> (ChildPidGuard, bool) {
        let guard = ChildPidGuard::register(self.active_children.clone(), pid);
        // 与 whisper / punc 的 register_child 同款：复查走同名只读入口，SeqCst 收口。
        (guard, self.is_cancelled())
    }

    /// 探测是否存在原生无需 Python 的 sherpa-onnx 独立执行体
    pub fn find_native_cli() -> Option<PathBuf> {
        let candidates = [
            crate::utils::AppConfig::resolve_path("tools/sherpa-onnx-offline.exe"),
            crate::utils::AppConfig::resolve_path("tools/sherpa-onnx.exe"),
            crate::utils::AppConfig::resolve_path("tools/sherpa-onnx/sherpa-onnx-offline.exe"),
            crate::utils::AppConfig::resolve_path("tools/sherpa-onnx/sherpa-onnx.exe"),
        ];
        candidates.into_iter().find(|p| p.is_file())
    }

    /// 检查 SenseVoice 所需权重与执行体是否就位（支持纯原生 exe 或 Python runner）
    /// 指定模型族（缺省 SenseVoice）。构造式接口：不改现有构造函数的签名，
    /// 老调用点一行都不用动。
    pub fn with_family(mut self, family: ModelFamily, decoder: Option<std::path::PathBuf>) -> Self {
        self.family = family;
        self.decoder_path = decoder;
        self
    }

    /// 当前模型族（供 UI/日志判断走的是哪条识别链路）。
    pub fn family(&self) -> ModelFamily {
        self.family
    }

    /// 该引擎要追加给 runner 的参数（SenseVoice 为空表，命令行与改造前逐字节一致）。
    fn family_args(&self) -> Vec<String> {
        self.family.runner_extra_args(self.decoder_path.as_deref())
    }

    pub fn is_available(&self) -> bool {
        // 判据不能只看 `exists()`：一次失败的复制/半截下载会留下**存在但内容不全**
        // 的模型，闸门放过去之后，真正的失败发生在 sherpa/onnxruntime 里，报的是一句
        // 与「文件不完整」毫不相干的话。这里按体积兜一道——ONNX 权重最小也有几十 MB，
        // 取 1 MB 作为下限，既能挡住 0 字节/被截断，又不会误伤任何真实模型。
        const MIN_MODEL_BYTES: u64 = 1024 * 1024;
        fn big_enough(path: &std::path::Path, min: u64) -> bool {
            std::fs::metadata(path)
                .map(|m| m.len() >= min)
                .unwrap_or(false)
        }
        big_enough(&self.model_path, MIN_MODEL_BYTES)
            && big_enough(&self.tokens_path, 1)
            && (Self::find_native_cli().is_some()
                || (big_enough(&self.runner_path, 1) && big_enough(&self.vad_model_path, 1)))
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
        let th = clamp_threads(threads.unwrap_or(self.threads));
        info!(
            "SenseVoice 启动极速非自回归转写: {:?}, 语言: {:?}, 线程: {}",
            audio_path, language, th
        );

        let mut cmd = if let Some(native_cli) = Self::find_native_cli() {
            info!("SenseVoice 检测到官方原生 sherpa-onnx 独立程序: {:?}，直接以纯 C++ 模式运行（0 Python 依赖）", native_cli);
            let mut c = Command::new(&native_cli);
            super::media_pipeline::apply_default_child_flags(&mut c);
            c.arg(format!("--sense-voice-model={}", self.model_path.display()))
                .arg(format!("--tokens={}", self.tokens_path.display()))
                .arg(format!("--num-threads={}", th));
            if self.vad_model_path.exists() {
                c.arg(format!(
                    "--silero-vad-model-config-model={}",
                    self.vad_model_path.display()
                ))
                .arg("--silero-vad-model-config-threshold=0.5");
            }
            c.arg(audio_path);
            c
        } else {
            let mut c = Command::new(&self.python_path);
            super::media_pipeline::apply_default_child_flags(&mut c);
            super::media_pipeline::apply_python_env(&mut c);
            c.arg(&self.runner_path)
                .args(self.family_args())
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
                    c.arg("--total-duration").arg(dur.to_string());
                }
            }
            c
        };

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        self.run_process_and_parse(cmd, None, total_duration, progress_cb)
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
        super::media_pipeline::apply_python_env(&mut cmd);
        // 让路模式必须在这里生效：SenseVoice 走 Python 子进程，长音频下
        // 多进程并行会吃满 CPU。只设 CREATE_NO_WINDOW 等于忽略用户的让路开关。
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        let th = clamp_threads(threads.unwrap_or(self.threads));
        cmd.arg(&self.runner_path)
            .args(self.family_args())
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

        self.run_process_and_parse(cmd, Some(stream), total_duration, progress_cb)
    }

    /// 跑一次 runner 子进程：抽干 stdout / stderr，解析逐行 JSON 事件流。
    ///
    /// 加固后的结构（与 `punc.rs` 同款）：
    /// 1. stdin 泵送、stdout 抽干、stderr 抽干各有独立线程，**主线程只做有界等待**；
    /// 2. 等待有明确上限（`wait_budget_for`：按时长缩放 + 硬上限），到点强杀进程树并收尸；
    /// 3. 出口可区分：取消（既有 `Ok((空, 0.0))` 契约，管线按自己的 cancelled 标志静默收尾）、
    ///    超时（`Err(SenseVoiceStageError::TimedOut)`，可 downcast）、普通失败（退出码非 0 /
    ///    输出不可用）；
    /// 4. **绝不把半截输出当结果写回**：stdout 读失败一律返回错误，而不是「少几句话的成功
    ///    结果」。
    fn run_process_and_parse(
        &self,
        mut cmd: Command,
        input_stream: Option<Box<dyn std::io::Read + Send + 'static>>,
        total_duration: Option<f64>,
        progress_cb: Option<crate::engines::SegmentProgressCb>,
    ) -> Result<(Vec<Segment>, f64)> {
        // 本轮结果是否被截断：必须在任何读取线程启动前清零，保证读到的永远是
        // **本次运行**的事实（上一轮的失败不能污染本轮）。
        self.output_truncated.store(false, Ordering::SeqCst);
        // spawn 前的取消早退：若取消在准备区（拼参数、选解释器…）期间就已到达，就没必要
        // 真的把 runner 拉起来再靠 register_child 的复查把它杀掉——那几毫秒里进程创建与
        // Python 解释器启动已经白做一遍，用户看到的是「点了终止却还闪了一下子进程」。
        // 返回形态与 wait 之后的取消出口完全一致（空结果 + 0.0），由管线按取消收尾，
        // 不发明新的错误类型或文案。
        //
        // run_process_and_parse 之前的准备区（`transcribe` / `transcribe_stream`）只拼了 `cmd`
        // 并已把它 move 进来，不持有需要手工清理的临时文件/JSON 守卫（SenseVoice 靠子进程
        // stdout 逐行流式 JSON，不落盘中转文件），因此这里提前 return 没有额外的守卫回收负担。
        //
        // 与 register_child 复查的分工：本处只覆盖「取消早于 spawn 请求」；取消落在本处与
        // register_child 之间时，仍由那处复查补杀。两者是串联的两道门，不会互相干扰。
        if self.is_cancelled() {
            return Ok((Vec::new(), 0.0));
        }

        // 找不到解释器是最常见的一种失败：报错原文（Windows 上是「系统找不到指定的文件。」）
        // 完全没提 Python，用户只会以为程序坏了。这里翻成可操作的中文指引——
        // 与 ffmpeg 那条「找不到就告诉用户放哪」的经验同源。
        let program = cmd.get_program().to_os_string();
        // 只有待启动的确实是本引擎配置的解释器时，才给「装 Python / 改 [paths] python」
        // 这条指引；否则如实报那条命令的路径（见 `spawn_failure_message` 的说明）。
        let python_hint = (std::path::Path::new(&program) == self.python_path.as_path())
            .then_some(self.python_path.as_path());
        let mut child = cmd.spawn().map_err(|err| {
            super::spawn_failure_message(&err, &program, " SenseVoice 进程", python_hint)
        })?;
        crate::utils::child_registry::adopt(&child);

        // 登记子进程 PID，供用户「终止转写」时强杀。
        // 用 RAII 守卫（与 whisper 引擎复用同一份实现）而不是「函数末尾手动 retain」：
        // 后续任何 `?` / 提前 return / 取消分支 / panic 展开都会经由 Drop 把这一行摘掉，
        // 不会留下会被 PID 复用误伤的陈旧条目。正常跑完时还要在 `wait()` 返回处
        // 主动 `disarm()`，见其文档。
        //
        // 声明次序是有意的：`child` 先声明、`child_guard` 后声明 ⇒ 逆序析构 ⇒ 守卫先摘登记行，
        // `child` 的进程句柄后关闭。与 whisper / punc 的次序一致，也是「Windows 上不存在
        // 陈旧 PID 被 cancel() 误杀」这条结论的前提：PID 只有在进程对象的最后一个句柄关闭后
        // 才可能被复用，而那一刻登记行早已摘掉。
        let (mut child_guard, cancelled_before_register) = self.register_child(child.id());
        // 竞态收口：cancel() 可能恰好插在 spawn() 成功与上面的登记之间——那会儿本行
        // 还没进表，cancel() 的快照拿到空名单，强杀信号就丢了，runner
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

        // stderr：独立线程抽干成**原始字节**再解码，主线程不碰管道。
        //
        // 为什么读字节而不是 `read_to_string`：sherpa-onnx 的 C++ 日志直写 fd 2，编码随系统
        // 区域设置（中文 Windows 上常见 GBK），也可能在日志中途被截断成半个多字节序列。
        // 严格解码一失败，旧代码就把整个 stderr 变成空串，于是「引擎没起来（C++ 侧日志不是
        // UTF-8）」被报成「请检查 python / 依赖」——错误归因完全反了。这里先收全字节，再交给
        // `decode_stderr_bytes` 做多编码兜底。
        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut raw: Vec<u8> = Vec::new();
            // `let _ =` 把读失败吞成空串：失败分支随后会给出「无任何输出」的误导提示，
            // 而真正的事实是诊断通道断了或只读到一半。至少留下一条带字节数的日志。
            if let Err(err) = std::io::BufReader::new(stderr).read_to_end(&mut raw) {
                warn!(
                    error = %err,
                    bytes = raw.len(),
                    "读取 SenseVoice 标准错误失败：诊断信息可能被截断"
                );
            }
            decode_stderr_bytes(&raw)
        });

        // stdout：独立线程抽干 + 逐行解析。**必须独立**：管道缓冲区只有几十 KB，主线程若
        // 自己边读边等，一旦读取循环因任何原因停了（读失败、提前 return），就没人再消费
        // 管道，runner 写满 64KB 后阻塞在 write 上，主线程的等待永不返回——这正是本项要修的
        // 死锁。抽干线程即使掉线，主线程的有界等待也会兜底强杀。
        //
        // 进度回调要同时给抽干线程（流式预览）与收尾（1.0 完成）用：`Box<dyn Fn>` 不能克隆，
        // 包一层 `Arc` 让两边共享。
        let progress_cb = progress_cb.map(Arc::new);
        let stdout_cb = progress_cb.clone();
        // 停滞看门狗的活动时间戳：抽干线程每读到一行就刷新，主线程据此判「卡死」。
        let last_activity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let stdout_activity = last_activity.clone();
        let stdout_handle = std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            consume_stdout(&mut reader, stdout_cb.as_deref(), Some(&stdout_activity))
        });

        // 有界等待：正常退出 / 取消 / 超时三个出口，出口之后一律 kill + wait 回收。
        // 旧写法直接 `child.wait()`：runner 卡死（onnxruntime 挂住、管道互等）时永不返回，
        // 界面永远停在「正在识别」，唯一出口是关掉应用。
        let child_pid = child.id();
        let budget = wait_budget_for(total_duration);
        let (outcome, status) = wait_for_child_with_stall(
            &mut child,
            budget,
            WAIT_POLL_INTERVAL,
            || self.is_cancelled(),
            Some(&last_activity),
        );
        if !matches!(outcome, WaitOutcome::Exited) {
            // 先按进程树强杀（与 whisper / punc 的取消路径同一套：除 Python 之外，
            // onnxruntime 在部分后端会拉起辅助进程），再 kill 句柄兜底。句柄是我们自己
            // spawn 的，TerminateProcess 必然成功，因此前面的 wait 不会变成第二次永久阻塞。
            kill_process_tree(child_pid);
            let _ = child.kill();
        }
        // 收尸：kill 之后必须 wait，否则句柄不释放；已自行退出的进程由 try_wait 收过尸，
        // 这里拿到的是缓存状态，不会二次阻塞。
        let _ = child.wait();
        // 进程已被 wait() 回收：立刻摘掉登记行，杜绝 cancel() 误杀可能复用该 PID 的
        // 无关进程（详见 ChildPidGuard::disarm）。放在 wait() 紧后面，是为了不让下面的
        // 线程 join 把这段窗口拉长。
        child_guard.disarm();
        // 注销交给 child_guard：这里不再手写 retain，避免「新增一条提前返回路径
        // 就得记得再补一次」的维护陷阱（PID 的登记与注销由同一个栈变量负责）。

        // 进程已死 ⇒ 所有管道写端断开 ⇒ 三个抽干/泵送线程必然很快结束（超时强杀也一样），
        // 这里的 join 不会挂。先收子线程再定性，避免半截数据参与判定。
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
        // stdout 抽干线程掉线（panic）只可能是本文件的 bug：按「输出不可用」降级成截断信号，
        // 绝不能当成「空结果」——那会把已经读到的片段悄无声息地吞掉。
        let mut summary =
            join_or_log(stdout_handle, "SenseVoice stdout 读取线程").unwrap_or_else(|| {
                StdoutSummary {
                    read_failed: true,
                    ..StdoutSummary::default()
                }
            });

        // runner 逐行回报的 provider 实况在主线程落库：`record_provider` 需要 `&self`，抽干
        // 线程只带得回只读数据。首个回报为准的语义不变（`record_provider` 自己保证）。
        for (requested, actual) in std::mem::take(&mut summary.provider_reports) {
            self.record_provider(&requested, &actual);
        }
        // 兜底通道：runner 未回传 `provider` 事件（旧版脚本）时，从 stderr 特征串识别
        // 回落。判定用完整 stderr；`record_provider` 首次为准，故与事件通道同时命中也不会
        // 重复告警。
        if stderr_provider_fallback(&stderr_str) {
            let requested = self.provider.clone();
            self.record_provider(&requested, "cpu");
        }

        // 用户主动终止：被杀进程的退出码不重要，按空结果返回，由管线走取消收尾。
        // （这里刻意**不**返回 Err，理由见 `SenseVoiceStageError` 的说明；这也是既有契约。）
        if self.is_cancelled() {
            return Ok((Vec::new(), 0.0));
        }

        // 停滞（卡死）：与超时同性质——runner 自己卡住了，必须明确告警，而不是静默
        // （取消）或按退出码归因成「依赖没装好」（误导）。
        if outcome == WaitOutcome::Stalled {
            warn!(
                seconds = SV_STALL_BUDGET.as_secs(),
                stderr = %stderr_str,
                "SenseVoice 连续无输出达到停滞预算，已强杀进程树"
            );
            return Err(anyhow::Error::new(SenseVoiceStageError::Stalled));
        }

        // 超时：与「用户取消」「普通失败」都不同——runner 自己卡住了，必须明确告警，而不是
        // 静默（取消）或按退出码归因成「依赖没装好」（误导）。返回值可
        // `downcast_ref::<SenseVoiceStageError>()` 出具体原因，调用方不必匹配文案。
        if outcome == WaitOutcome::TimedOut {
            warn!(
                seconds = budget.as_secs(),
                stderr = %stderr_str,
                "SenseVoice 超过等待上限仍未结束，已强杀进程树"
            );
            return Err(anyhow::Error::new(SenseVoiceStageError::TimedOut));
        }

        // 等待本身失败（句柄异常）：`try_wait` 的 io 详情已在 `wait_for_child` 里 warn 过，
        // 这里给上层一句可读结论；进程已按上面的强杀分支回收。
        if outcome == WaitOutcome::WaitFailed {
            anyhow::bail!("等待 SenseVoice 进程结束失败");
        }

        // Exited 出口必然带退出码；拿不到只可能是状态被上面的分支吃掉，保守报内部错误，
        // 而不是继续用半截数据组装结果。
        let status = match status {
            Some(status) => status,
            None => anyhow::bail!("SenseVoice 进程状态缺失：无法判断转写是否完成"),
        };

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
            // 针对 Windows 应用商店 0 字节别名桩 (9009) 或依赖缺失 (10) 提供精准保姆级指引
            let stderr_trimmed = stderr_str.trim();
            if status.code() == Some(9009) || stderr_trimmed.contains("Python was not found") {
                anyhow::bail!(
                    "SenseVoice 启动失败：系统未找到真正的 Python 3 解释器（触发了 Windows 商店别名桩）。\n\
                     请从 python.org 下载安装 Python 3 并勾选「Add Python to PATH」，或在「性能设置」手动指定 python.exe 路径。"
                );
            }
            if status.code() == Some(10)
                || stderr_trimmed.contains("缺少必需的 Python 依赖库")
                || stderr_trimmed.contains("No module named")
            {
                anyhow::bail!(
                    "SenseVoice 运行环境依赖缺失：\n{}\n\
                     若出现 Failed to import encodings module / Could not find platform independent libraries：当前 Python 不是完整安装（客户机上常见：只剩一个残缺的 python）。请改用完整安装包（自带便携 Python），或在 config.local.toml 里把 paths.python 指向完整的 python.exe。
             若提示缺少 numpy / sherpa-onnx：在「性能设置 → 模型与组件」一键补齐，或对该 Python 执行 pip install numpy sherpa-onnx。",
                    stderr_trimmed
                );
            }
            if stderr_trimmed.is_empty() {
                anyhow::bail!(
                    "SenseVoice 识别失败：runner 退出码 {:?} 且无任何输出。\
                     通常是 Python 解释器或依赖未就位——请确认「性能设置」里的 \
                     python 路径正确，且已安装 sherpa-onnx 与 numpy。",
                    status.code()
                );
            }
            anyhow::bail!("SenseVoice 识别失败: {}", stderr_trimmed);
        }

        // 结果定性：截断 / 整段不可解析 / 可用。判定是纯函数，三条方向都能直接单测。
        let verdict = stdout_verdict(
            summary.read_failed,
            summary.parsed_lines,
            summary.malformed_lines,
        );

        // 截断**优先于退出码**：读到一半报错是比「退出码非 0」更具体的信号。runner 自己崩掉时
        // 管道写端关闭，我们拿到的是 EOF（`read_failed == false`），自然走下面的退出码分支拿到
        // 真正的 stderr；反过来，读失败后读端关闭，runner 往往是被 EPIPE 打死的，退出码只会把
        // 归因带偏。两条路都返回 Err、都不写回半截结果，区别只在文案方向。
        if verdict == StdoutVerdict::Truncated {
            // 「输出被截断」的正式信号：调用方（管线 / UI）据此提示用户。
            // 与错误**同向**：这里返回 Err，绝不把已经读到的那半截当结果写回。
            self.output_truncated.store(true, Ordering::SeqCst);
            warn!(
                code = ?status.code(),
                stderr = %stderr_str,
                "SenseVoice 输出被截断：读取 runner 标准输出失败（stderr 见上，供排查）"
            );
            anyhow::bail!(
                "SenseVoice 输出被截断（runner 退出码 {:?}）：读取 runner 标准输出失败后中止。\
                 已读到的内容不完整，本次结果不可用，请重试；若反复出现请保留 logs/ 中的日志。",
                status.code()
            );
        }

        // 有输出却一行都解不开：只在进程**正常退出**后才下这个结论——退出码非 0 时上面的
        // 分支已经拿 stderr 给出了真正的失败原因，那时候报「协议不匹配」会把用户带偏。
        if verdict == StdoutVerdict::Unparsable {
            anyhow::bail!(
                "SenseVoice runner 有输出但没有任何一行能解析成协议事件（{} 行无法解析）：\
                 通常是 runner 脚本与引擎版本不匹配，或输出被非 UTF-8 文本污染。\
                 请确认 tools/sensevoice_runner.py 与本程序版本一致。",
                summary.malformed_lines
            );
        }

        let mut segments = std::mem::take(&mut summary.segments);

        // 智能优化时间轴：消除 100ms 重叠鬼影、消除时间重叠冲突、广播级短句延展平滑
        crate::subtitle::optimize_segments(&mut segments);

        if let Some(cb) = progress_cb.as_deref() {
            cb(
                1.0,
                &format!("SenseVoice 转写完成，共 {} 句", segments.len()),
                None,
            );
        }

        // 推理时长规整：`finished` 事件缺失（老 runner，或进程被杀只吐了半截协议）时它是
        // 0.0，而调用方拿它算耗时占比会得到 0 或 inf。这里落到明确的下限常量，保证
        // 「成功返回的时长恒 > 0」这条契约（取消出口的 0.0 是既有契约，见常量说明）。
        let inference_elapsed = clamp_inference_elapsed(summary.inference_elapsed);

        info!(
            segments = segments.len(),
            elapsed = inference_elapsed,
            "SenseVoice 转写完成"
        );
        Ok((segments, inference_elapsed))
    }
}

/// `inference_elapsed` 的下限（秒）。
///
/// `finished` 事件缺失（老版 runner，或进程被杀只吐了半截协议）时引擎记下的时长是 0.0，
/// 而调用方（如 `core/metrics.rs` 的耗时占比）会拿它做除法：0 会让占比变成 0 或 inf。
/// 用一个明确的小正数兜底，语义是「不足 1ms 记作 1ms」——比在调用方到处写判零清楚，且
/// 「成功返回的 `inference_elapsed` 恒 `> 0`」这条契约可以在单测里直接钉住。
///
/// 唯一的例外是**取消**：那条出口按既有契约返回 `0.0`（管线只按自己的 cancelled 标志收尾，
/// 不会用到这个时长；既有单测也把 0.0 钉死了）。
const INFERENCE_ELAPSED_FLOOR_SEC: f64 = 0.001;

/// 把 runner 回报的推理时长规整成「调用方可以直接除」的值：非有限 / 非正 / 缺失一律落到
/// [`INFERENCE_ELAPSED_FLOOR_SEC`]。
fn clamp_inference_elapsed(raw: f64) -> f64 {
    if raw.is_finite() && raw > 0.0 {
        raw
    } else {
        INFERENCE_ELAPSED_FLOOR_SEC
    }
}

/// runner stdout 抽干线程的汇总结果。
#[derive(Debug, Default)]
struct StdoutSummary {
    /// 已解析出的字幕片段（`segment` 事件 + `finished.all_segments` 兜底）。
    segments: Vec<Segment>,
    /// `finished` 事件回报的推理时长（秒）；缺失时保持 0.0，返回前由
    /// [`clamp_inference_elapsed`] 规整。
    inference_elapsed: f64,
    /// 成功解析成协议事件的行数。
    parsed_lines: usize,
    /// 非空但解不开的行数（半截 JSON / 非 UTF-8 乱码 / 协议不匹配）。
    malformed_lines: usize,
    /// `provider` 事件带回的「请求 vs 实际」，在主线程落库（见 `record_provider`）。
    provider_reports: Vec<(String, String)>,
    /// 是否遇到**非 EOF** 的读失败（= 输出被截断）。
    read_failed: bool,
}

/// 一次 stdout 汇总的定性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdoutVerdict {
    /// 可用：解出了协议事件，或「完全空输出」（runner 静默退出，不是故障）。
    Usable,
    /// 读失败：结果被截断，绝不能当结果写回。
    Truncated,
    /// 有输出但一行都没解开：协议版本不匹配 / 非 UTF-8 乱码。
    Unparsable,
}

/// 判定一次 stdout 汇总应当如何定性（纯函数，单测可直接钉住三个方向）。
///
/// 为什么「空输出」不算故障：runner 对无语音音频本来就可能一行都不吐（`segment` 事件为零），
/// 此时退出码为 0，与「协议不匹配」必须区分开——后者一行都解析不出但**有输出**，那才是需要
/// 用户介入的错误方向。截断（读失败）优先级最高：它意味着连「读到的是不是全部」都不成立。
fn stdout_verdict(read_failed: bool, parsed_lines: usize, malformed_lines: usize) -> StdoutVerdict {
    if read_failed {
        return StdoutVerdict::Truncated;
    }
    if parsed_lines == 0 && malformed_lines > 0 {
        return StdoutVerdict::Unparsable;
    }
    StdoutVerdict::Usable
}

/// 把 runner stdout 的一行原始字节解成文本（去首尾空白）。
///
/// 用 lossy 而不是严格 UTF-8：sherpa-onnx 的 C++ 日志与 python 的异常文本可能带 GBK 或半个
/// 多字节序列，严格解码会让 `BufRead::lines()` 直接报 `InvalidData`，把「一行乱码」升级成
/// 「管道读失败 → 输出被截断」的误判。协议行本身是 UTF-8 JSON（引擎侧日志不会混进 JSON 行），
/// 去掉 BOM 后按 lossy 解码即可：坏字节只变成替换字符，解析失败被计入 `malformed_lines`，
/// 归因方向正确。
fn decode_stdout_line(raw: &[u8]) -> String {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    String::from_utf8_lossy(raw).trim().to_string()
}

/// 清理 SenseVoice 输出的前置与后置特殊标记，如 `<|zh|><|NEUTRAL|><|Speech|><|withitn|>`
pub fn clean_sensevoice_tags_rs(text: &str) -> String {
    let re = regex::Regex::new(r"<\|.*?\|>").unwrap();
    re.replace_all(text, "").trim().to_string()
}

/// 把 runner stderr 的原始字节解成文本（多编码兜底，永不失败）。
///
/// sherpa-onnx 的 C++ 日志直接 `write(2)` 到 fd 2，编码随系统区域设置（中文 Windows 上常见
/// GBK），也可能在日志中途被截断成半个多字节序列。以前这里用严格 UTF-8 解码：失败即返回
/// 空串，于是「引擎根本没起来（C++ 侧日志不是 UTF-8）」被报成「请检查 python / 依赖」——
/// 错误归因完全反了。顺序：严格 UTF-8 → GBK（`had_errors` 为假才算命中）→ lossy 兜底。
fn decode_stderr_bytes(raw: &[u8]) -> String {
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    if let Ok(text) = std::str::from_utf8(raw) {
        return text.to_string();
    }
    let (decoded, _, had_errors) = encoding_rs::GBK.decode(raw);
    if !had_errors {
        return decoded.into_owned();
    }
    String::from_utf8_lossy(raw).into_owned()
}

/// 把一条已解析的协议事件并入汇总（片段 / 进度回调 / provider 回报）。
///
/// 进度回调由 UI 层提供，这里用 `catch_unwind` 单独兜住：回调在 stdout 抽干线程里执行，
/// 一旦 panic 展开，线程会直接死掉，读端随之关闭，**剩下的片段全部丢失**（用户看到「字幕
/// 缺一半」且毫无线索），而子进程的 kill / wait 都在主线程，救不回来。代价是吞掉 UI 侧的
/// panic——只记一条 warn 后继续消费输出；这比「一次 UI 抖动毁掉整次转写的后半段」划算。
fn apply_stream_event(
    summary: &mut StdoutSummary,
    parsed: SenseVoiceStreamLine,
    progress_cb: Option<&crate::engines::SegmentProgressCb>,
) {
    match parsed.event.as_str() {
        "segment" => {
            let seg = Segment {
                locked: false,
                index: parsed.index,
                start: parsed.start,
                end: parsed.end,
                text: parsed.text.clone(),
                translation: None,
                translation_lang: None,
                translations: Default::default(),
                polished: parsed.text.clone(),
                language: None,
                confidence: None,
                speaker: None,
                speaker_name: None,
                words: Vec::new(),
            };
            summary.segments.push(seg.clone());
            if let Some(cb) = progress_cb {
                let label = format!("SenseVoice 极速转写中: 第 {} 句", parsed.index);
                // 与 Whisper 推流出口同源：Python runner 已按标点细切，但若某段仍超过
                // 6s / 60 字（句内无可用切点），预览还得过一遍 Rust 拆分，否则界面实时流
                // 会比最终字幕表粗一档（最终表由 `optimize_segments` 走同一个
                // `split_long_segments`）。
                for piece in crate::subtitle::split_long_segments(vec![seg]) {
                    let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        cb(parsed.progress, &label, Some(piece));
                    }));
                    if called.is_err() {
                        // 回调炸了不该拖垮识别：记一条日志，继续消费后面的输出。
                        warn!("SenseVoice 进度回调 panic：已忽略，继续消费 runner 输出");
                        break;
                    }
                }
            }
        }
        "finished" => {
            summary.inference_elapsed = parsed.elapsed_sec;
            if summary.segments.is_empty() && !parsed.segments.is_empty() {
                for item in parsed.segments {
                    // runner 的 `polished` 与 `text` 目前同值，但字段是它主动声明的
                    // （`all_segments` 里两者并列）：直接拿 `text` 顶上等于把「已润色文本」
                    // 这条通道静默掐掉——将来 runner 若输出真正的标点恢复文本，这里会
                    // 一声不响地丢弃。
                    let polished = if item.polished.trim().is_empty() {
                        item.text.clone()
                    } else {
                        item.polished
                    };
                    summary.segments.push(Segment {
                        locked: false,
                        index: item.index,
                        start: item.start,
                        end: item.end,
                        text: item.text,
                        translation: None,
                        translation_lang: None,
                        translations: Default::default(),
                        polished,
                        language: None,
                        confidence: None,
                        speaker: None,
                        speaker_name: None,
                        words: Vec::new(),
                    });
                }
            }
        }
        "provider" => summary
            .provider_reports
            .push((parsed.requested.clone(), parsed.actual.clone())),
        // 兼容原生 sherpa-onnx 独立程序输出（无 event 字段，但直接带有 text）
        "" if !parsed.text.is_empty() => {
            let clean = clean_sensevoice_tags_rs(&parsed.text);
            if !clean.is_empty() {
                summary.segments.push(Segment {
                    locked: false,
                    index: summary.segments.len() + 1,
                    start: parsed.start,
                    end: parsed.end,
                    text: clean.clone(),
                    translation: None,
                    translation_lang: None,
                    translations: Default::default(),
                    polished: clean,
                    language: None,
                    confidence: None,
                    speaker: None,
                    speaker_name: None,
                    words: Vec::new(),
                });
            }
        }
        // 未知事件一律忽略：协议只增不改，旧引擎遇到新事件必须安全跳过。
        _ => {}
    }
}

/// 抽干并解析 runner 的 stdout（逐行 JSON 事件流），返回一次性汇总。
///
/// - 逐行按**原始字节**读入（`read_until(b'\n')`）再 lossy 解码：用 `BufRead::lines()` 的话，
///   非 UTF-8 行会直接报 `InvalidData`，把「一行乱码」误判成「管道读失败」。
/// - 读失败（非 EOF）只置 `read_failed` 后停止消费，不再把半截结果丢掉——出口由主线程的
///   有界等待 + [`stdout_verdict`] 统一定性。
/// - 泛型 `R: BufRead` 让单测能注入「空输出 / 半截 JSON / 非 UTF-8 字节 / 读到一半报错」的
///   假管道，完全不必拉起 python 与真模型。
fn consume_stdout<R: std::io::BufRead>(
    reader: &mut R,
    progress_cb: Option<&crate::engines::SegmentProgressCb>,
    // 停滞看门狗的活动时间戳：每读到一行就刷新。None = 不跟踪。
    activity: Option<&Arc<std::sync::Mutex<Instant>>>,
) -> StdoutSummary {
    let mut summary = StdoutSummary::default();
    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            // 正常结束（EOF）。
            Ok(0) => break,
            Ok(_) => {
                // 每收到一行就刷新活动时间：看门狗据此判断「还在正常吐字」还是「卡住了」。
                if let Some(tracker) = activity {
                    if let Ok(mut t) = tracker.lock() {
                        *t = Instant::now();
                    }
                }
                let line = decode_stdout_line(&raw);
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<SenseVoiceStreamLine>(&line) {
                    Ok(parsed) => {
                        summary.parsed_lines += 1;
                        apply_stream_event(&mut summary, parsed, progress_cb);
                    }
                    Err(err) => {
                        // 兜底尝试原生 sherpa-onnx 独立执行体输出（如 {"text": "..."}）
                        if let Ok(native_val) = serde_json::from_str::<serde_json::Value>(&line) {
                            if let Some(txt) = native_val.get("text").and_then(|t| t.as_str()) {
                                let clean = clean_sensevoice_tags_rs(txt);
                                if !clean.is_empty() {
                                    summary.parsed_lines += 1;
                                    summary.segments.push(Segment::new(
                                        summary.segments.len() + 1,
                                        0.0,
                                        0.0,
                                        clean,
                                    ));
                                    continue;
                                }
                            }
                        }
                        // runner 协议是逐行 JSON：解不开的行以前被完全静默地丢掉，版本不匹配
                        // 时用户只会看到「一句都没有」。只报首条（带截断预览）避免刷屏，
                        // 收尾按 `stdout_verdict` 定性。
                        if summary.malformed_lines == 0 {
                            let preview: String = line.chars().take(160).collect();
                            warn!(
                                error = %err,
                                preview = %preview,
                                "SenseVoice runner 输出了无法解析的行：已忽略"
                            );
                        }
                        summary.malformed_lines += 1;
                    }
                }
            }
            Err(err) => {
                // 读失败（非 EOF）：记下信号后停止消费。reader 随本函数返回被丢弃 ⇒ 管道读端
                // 关闭 ⇒ 仍在写 stdout 的 runner 会立刻拿到写失败而不是堵在 64KB 缓冲区上。
                warn!(
                    error = %err,
                    "读取 SenseVoice 标准输出失败：转写输出被截断（这不是正常结束）"
                );
                summary.read_failed = true;
                break;
            }
        }
    }
    summary
}
#[cfg(test)]
mod tests {

    /// SenseVoice 是缺省族，**不能**往命令行里加任何参数——
    /// 否则「改造前后命令行逐字节一致」这条回归就失去意义。
    #[test]
    fn sensevoice_family_adds_no_runner_args() {
        assert!(super::ModelFamily::SenseVoice
            .runner_extra_args(None)
            .is_empty());
        assert!(super::ModelFamily::SenseVoice
            .runner_extra_args(Some(std::path::Path::new("dec.onnx")))
            .is_empty());
    }

    /// 其它族必须显式声明 --model-type；需要 decoder 的族还要带上 --decoder。
    #[test]
    fn other_families_pass_model_type_and_decoder() {
        use super::ModelFamily;
        assert_eq!(
            ModelFamily::DolphinCtc.runner_extra_args(None),
            vec!["--model-type", "dolphin-ctc"]
        );
        assert_eq!(
            ModelFamily::Paraformer.runner_extra_args(None),
            vec!["--model-type", "paraformer"]
        );
        assert_eq!(
            ModelFamily::FireRedAsr.runner_extra_args(Some(std::path::Path::new("dec.onnx"))),
            vec!["--model-type", "fire-red-asr", "--decoder", "dec.onnx"]
        );
        assert!(ModelFamily::FireRedAsr.needs_decoder());
        assert!(ModelFamily::NemoCanary.needs_decoder());
        assert!(!ModelFamily::NemoCtc.needs_decoder());
    }

    use super::*;
    use crate::engines::whisper::take_live_pids;

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
            .run_process_and_parse(cmd, None, None, None)
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

        let (segments, elapsed_sec) = engine
            .run_process_and_parse(cmd, None, None, None)
            .expect("空输出的子进程应当正常结束");
        assert!(segments.is_empty(), "没有 JSON 输出就不该有字幕片段");
        // 没有 `finished` 事件 ⇒ 时长字段缺失。成功出口必须给出非 0 值：调用方拿它做除法
        // （core/metrics.rs 的耗时占比），0 会变成 0 / inf。
        assert!(
            elapsed_sec > 0.0,
            "成功返回的推理时长不得为 0（缺失时必须落到下限常量），实得 {elapsed_sec}"
        );
        assert!(
            registered(&engine.active_children).is_empty(),
            "子进程结束后登记表必须为空"
        );
    }

    /// 取消路径：`cancel()` 取**快照**并强杀，登记表里的行由守卫摘除。
    ///
    /// 旧语义是 drain（取走并清空），已改为快照：drain 让「取消」变成一次性动作，
    /// 用户连点两次「终止转写」时第二次拿到空名单，第一次的 taskkill 若因竞态/权限
    /// 没杀掉子进程，用户就再没有第二次机会（界面表现是「点了终止，转写照跑」）。
    /// whisper 引擎此前已改成快照，现在三个引擎口径一致。
    #[test]
    fn cancel_snapshots_registry_and_guard_drop_is_afterwards_harmless() {
        let engine = engine_with_dummy_paths();
        // 登记一个不存在的 PID：cancel() 会去 kill_process_tree 一个空号，
        // 这在 Windows 上只是 taskkill 报「找不到进程」，不会误伤真实进程。
        let guard = ChildPidGuard::register(engine.active_children.clone(), u32::MAX);
        assert_eq!(registered(&engine.active_children).len(), 1);

        engine.cancel();
        assert!(
            !registered(&engine.active_children).is_empty(),
            "cancel() 只取快照、不清表：行留给 ChildPidGuard 摘除（取消可重复）"
        );

        drop(guard);
        assert!(
            registered(&engine.active_children).is_empty(),
            "守卫析构后表必须为空"
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
            .run_process_and_parse(cmd, None, None, None)
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
            .run_process_and_parse(cmd, None, None, None)
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
            .run_process_and_parse(cmd, None, None, None)
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
            .run_process_and_parse(cmd, None, None, None)
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

    // ───────── 有界等待：三条出口都用真实子进程跑一遍 ─────────
    //
    // 这些用例用 cmd/ping 验证等待行为，只在 Windows 上跑（项目目标平台）。

    /// ping 要跑约 30 秒，足够验证「超时 / 取消时真的强杀」。
    #[cfg(target_os = "windows")]
    fn spawn_long_lived() -> Child {
        Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    /// 立刻以 0 退出，用来验证「正常退出」出口。
    #[cfg(target_os = "windows")]
    fn spawn_short_lived() -> Child {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    /// 判定语义：都没命中就继续等；命中就退出；两者同时成立时「取消」优先于「超时」
    /// （用户主动终止是更强的语义）。
    #[test]
    fn wait_outcome_prioritises_cancel_over_timeout() {
        assert_eq!(wait_outcome(false, false), None, "都没到点必须继续等");
        assert_eq!(wait_outcome(true, false), Some(WaitOutcome::Cancelled));
        assert_eq!(wait_outcome(false, true), Some(WaitOutcome::TimedOut));
        assert_eq!(
            wait_outcome(true, true),
            Some(WaitOutcome::Cancelled),
            "同时超时时按「已取消」上报"
        );
    }

    /// 等待预算必须**随时长增长**、且始终有**硬上限**：
    /// - 固定小预算会把长音频的正常推理误杀成「超时」；
    /// - 没有上限就等于没把「runner 卡死 → 应用永久卡住」降级成有限失败。
    #[test]
    fn wait_budget_scales_with_audio_duration_and_stays_bounded() {
        let assumed = Duration::from_secs_f64(
            WAIT_COLD_START_SEC + WAIT_SECONDS_PER_AUDIO_SEC * WAIT_ASSUMED_AUDIO_SEC,
        );
        assert_eq!(wait_budget_for(None), assumed, "时长未知时用假设值");
        // 短视频的预算必须能覆盖冷启动（模型加载比推理本身还慢）。
        assert!(
            wait_budget_for(Some(10.0)) >= Duration::from_secs_f64(WAIT_COLD_START_SEC),
            "冷启动余量不得被时长缩放吃掉"
        );
        // 长音频：预算必须随时长单调增长，否则长素材会被误杀。
        assert!(
            wait_budget_for(Some(3600.0)) > wait_budget_for(Some(600.0)),
            "预算必须随时长增长"
        );
        // 硬上限：再长、再离谱的时长都不能让调用方无限等。
        assert_eq!(wait_budget_for(Some(1.0e9)), WAIT_BUDGET_MAX);
        assert_eq!(wait_budget_for(Some(f64::MAX)), WAIT_BUDGET_MAX);
        assert!(
            WAIT_BUDGET_MAX <= Duration::from_secs(6 * 3600),
            "上限不得失控"
        );
        // 损坏的时长字段（0 / 负数 / NaN / inf）按「未知」处理，不能让预算变成 0 或 NaN。
        for bad in [Some(0.0), Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(
                wait_budget_for(bad),
                assumed,
                "非法时长 {bad:?} 必须回落到假设值"
            );
        }
        assert!(WAIT_POLL_INTERVAL > Duration::ZERO, "轮询间隔不能为 0");
        assert!(
            WAIT_POLL_INTERVAL <= Duration::from_millis(500),
            "轮询间隔过长会让取消 / 超时的响应明显迟滞"
        );
    }

    /// 有界等待的三条出口都要能用**真实子进程**跑通：正常退出、超时（预算极小 + 长跑进程）、
    /// 取消。预算与轮询间隔是参数，因此不必等满真实预算，也不依赖 python / 模型。
    #[cfg(target_os = "windows")]
    #[test]
    fn bounded_wait_reports_exit_timeout_and_cancel() {
        let short_poll = Duration::from_millis(20);

        // 正常退出：立刻拿到退出码，且不判超时。
        let mut child = spawn_short_lived();
        let (outcome, status) =
            wait_for_child(&mut child, Duration::from_secs(30), short_poll, || false);
        assert_eq!(outcome, WaitOutcome::Exited);
        assert!(status.expect("正常退出必须带退出码").success());

        // 超时：ping 要跑 30 秒，预算只给 300ms，必须在预算附近返回「超时」而不是死等。
        let mut child = spawn_long_lived();
        let started = Instant::now();
        let (outcome, status) =
            wait_for_child(&mut child, Duration::from_millis(300), short_poll, || false);
        assert_eq!(outcome, WaitOutcome::TimedOut);
        assert!(status.is_none(), "未退出的进程不得给出退出码");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "有界等待必须在预算附近返回，绝不无限等"
        );
        let _ = child.kill();
        let _ = child.wait();

        // 取消：预算给到 30 秒，但取消已置位 → 立刻返回「取消」，不必等到超时。
        let mut child = spawn_long_lived();
        let started = Instant::now();
        let (outcome, _) = wait_for_child(&mut child, Duration::from_secs(30), short_poll, || true);
        assert_eq!(outcome, WaitOutcome::Cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "取消必须立刻生效，而不是等到超时"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// 超时必须能与「普通失败」区分开：调用方可以 downcast 出具体原因，而不是靠错误文案里的
    /// 关键字（改文案就失效）。
    #[test]
    fn timeout_error_is_downcastable_and_never_claims_partial_success() {
        let err = anyhow::Error::new(SenseVoiceStageError::TimedOut);
        assert_eq!(
            err.downcast_ref::<SenseVoiceStageError>(),
            Some(&SenseVoiceStageError::TimedOut)
        );
        assert!(err.to_string().contains("超时"), "文案要讲清是超时: {err}");
    }

    // ───────── stdout 解析：空输出 / 半截 JSON / 非 UTF-8 都不能崩，方向要对 ─────────

    /// 定性纯函数的三条方向：截断（读失败）、整段不可解析（有输出但一行都解不开）、可用。
    /// 空输出必须归到「可用」——runner 对无语音音频本来就可能一行不吐，误报成错误方向会把
    /// 用户引去查 python 依赖。
    #[test]
    fn stdout_verdict_separates_truncated_unparsable_and_usable() {
        assert_eq!(
            stdout_verdict(true, 3, 0),
            StdoutVerdict::Truncated,
            "读失败一律按截断"
        );
        assert_eq!(
            stdout_verdict(true, 0, 0),
            StdoutVerdict::Truncated,
            "截断优先于其它判定"
        );
        assert_eq!(
            stdout_verdict(false, 0, 0),
            StdoutVerdict::Usable,
            "空输出不是故障"
        );
        assert_eq!(
            stdout_verdict(false, 0, 2),
            StdoutVerdict::Unparsable,
            "有输出却一行都解不开"
        );
        assert_eq!(
            stdout_verdict(false, 5, 2),
            StdoutVerdict::Usable,
            "解出事件即可用"
        );
    }

    /// 逐行解析对三种畸形输入都不能 panic，且计数方向正确：
    /// - 空输出 → 无事件、无坏行（可用）；
    /// - 半截 JSON → 记坏行、不产出片段；
    /// - 非 UTF-8 字节（GBK「错」= B4 ED）→ 按 lossy 解码后记坏行，**不是**读失败。
    #[test]
    fn consume_stdout_tolerates_empty_truncated_json_and_non_utf8() {
        // 1) 空输出
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let summary = consume_stdout(&mut empty, None, None);
        assert_eq!(summary.parsed_lines, 0);
        assert_eq!(summary.malformed_lines, 0);
        assert!(summary.segments.is_empty());
        assert!(!summary.read_failed, "空输出是 EOF，不是读失败");
        assert_eq!(
            stdout_verdict(
                summary.read_failed,
                summary.parsed_lines,
                summary.malformed_lines
            ),
            StdoutVerdict::Usable
        );

        // 2) 半截 JSON + 非 UTF-8 行 + 一条合法行
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"event\":\"seg\n");
        bytes.extend_from_slice(&[0xB4, 0xED, b'\n']);
        bytes.extend_from_slice(b"{\"event\":\"finished\",\"elapsed_sec\":2.5}\n");
        let mut broken = std::io::Cursor::new(bytes);
        let summary = consume_stdout(&mut broken, None, None);
        assert_eq!(
            summary.malformed_lines, 2,
            "半截 JSON 与非 UTF-8 行都必须计入坏行"
        );
        assert_eq!(summary.parsed_lines, 1, "后面的合法行仍要解析出来");
        assert_eq!(summary.inference_elapsed, 2.5);
        assert!(!summary.read_failed, "乱码不是管道故障，归因方向要正确");
        assert_eq!(
            stdout_verdict(
                summary.read_failed,
                summary.parsed_lines,
                summary.malformed_lines
            ),
            StdoutVerdict::Usable
        );

        // 3) 整段都是坏行 ⇒ 判为不可解析（调用方据此报「runner 版本不匹配」，而不是「空结果」）
        let mut junk = std::io::Cursor::new(vec![0xB4, 0xED, b'\n', b'{', b'\n']);
        let summary = consume_stdout(&mut junk, None, None);
        assert_eq!(summary.parsed_lines, 0);
        assert_eq!(summary.malformed_lines, 2);
        assert_eq!(
            stdout_verdict(
                summary.read_failed,
                summary.parsed_lines,
                summary.malformed_lines
            ),
            StdoutVerdict::Unparsable
        );
    }

    /// 读到一半报错必须与 EOF 分开：`read_failed` 置位 ⇒ 定性为截断，调用方据此报错，绝不把
    /// 半截输出当结果（钉住的正是原 `drain_stdout_lines` 用例的同一条契约）。
    #[test]
    fn consume_stdout_separates_read_failure_from_eof() {
        /// 假管道：先交付一行合法 JSON，之后每次读都报 BrokenPipe。
        struct FailingReader {
            queued: Vec<u8>,
            pos: usize,
        }
        impl std::io::Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.pos < self.queued.len() {
                    let n = (self.queued.len() - self.pos).min(buf.len());
                    buf[..n].copy_from_slice(&self.queued[self.pos..self.pos + n]);
                    self.pos += n;
                    return Ok(n);
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "假管道：读失败",
                ))
            }
        }

        let line =
            b"{\"event\":\"segment\",\"index\":1,\"start\":0.0,\"end\":1.0,\"text\":\"a\"}\n";
        // 对照组：同样的字节读完后正常 EOF ⇒ 不是读失败。
        let mut clean = std::io::Cursor::new(line.to_vec());
        let summary = consume_stdout(&mut clean, None, None);
        assert_eq!(summary.segments.len(), 1);
        assert!(!summary.read_failed, "正常 EOF 不是读失败");
        assert_eq!(
            stdout_verdict(
                summary.read_failed,
                summary.parsed_lines,
                summary.malformed_lines
            ),
            StdoutVerdict::Usable
        );

        // 主用例：读完这一行后管道报错 ⇒ 必须定性为截断。
        let mut reader = std::io::BufReader::new(FailingReader {
            queued: line.to_vec(),
            pos: 0,
        });
        let summary = consume_stdout(&mut reader, None, None);
        assert_eq!(summary.segments.len(), 1, "失败前读到的行仍要保留");
        assert!(summary.read_failed, "读失败必须与 EOF 分开表达");
        assert_eq!(
            stdout_verdict(
                summary.read_failed,
                summary.parsed_lines,
                summary.malformed_lines
            ),
            StdoutVerdict::Truncated
        );
    }

    /// 进度回调 panic 不能毁掉输出消费：回调跑在 stdout 抽干线程里，panic 展开会让线程直接
    /// 死掉、读端关闭，后面所有片段无声丢失。这里用一个必然 panic 的回调，断言后续行仍被
    /// 解析出来（`catch_unwind` 兜住了它）。
    #[test]
    fn panicking_progress_callback_does_not_stop_stdout_consumption() {
        let lines = concat!(
            "{\"event\":\"segment\",\"index\":1,\"start\":0.0,\"end\":1.0,\"text\":\"a\"}\n",
            "{\"event\":\"segment\",\"index\":2,\"start\":1.0,\"end\":2.0,\"text\":\"b\"}\n",
            "{\"event\":\"finished\",\"elapsed_sec\":1.0}\n",
        );
        let mut reader = std::io::Cursor::new(lines.as_bytes().to_vec());
        let cb: crate::engines::SegmentProgressCb = Box::new(|_, _, _| panic!("UI 回调炸了"));
        // 静音 panic 打印，避免测试输出里出现像失败的堆栈。
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let summary = consume_stdout(&mut reader, Some(&cb), None);
        std::panic::set_hook(prev);

        assert_eq!(
            summary.segments.len(),
            2,
            "回调 panic 后仍必须解析出全部片段"
        );
        assert_eq!(summary.parsed_lines, 3);
        assert_eq!(summary.inference_elapsed, 1.0);
        assert!(!summary.read_failed);
    }

    /// stderr 解码必须对非 UTF-8 字节有兜底：中文 Windows 上 sherpa-onnx 的 C++ 日志是 GBK，
    /// 严格 UTF-8 解码失败会让「引擎没起来」被报成「请检查 python / 依赖」。
    #[test]
    fn stderr_decode_survives_non_utf8_and_truncated_bytes() {
        // GBK「未找到模型」（同时不是合法 UTF-8）
        let gbk = [0xCE, 0xB4, 0xD5, 0xD2, 0xB5, 0xBD, 0xC4, 0xA3, 0xD0, 0xCD];
        let out = decode_stderr_bytes(&gbk);
        assert!(
            !out.is_empty(),
            "非 UTF-8 stderr 不得变成空串（否则归因会跑偏）"
        );
        assert!(out.contains("未找到模型"), "GBK 应按 GBK 解出，实得: {out}");

        // 日志被截断成半个多字节序列 + BOM：内容仍要保留
        let mut truncated = vec![0xEF, 0xBB, 0xBF];
        truncated.extend_from_slice(b"Fallback to cpu: ");
        truncated.push(0xE4);
        let out = decode_stderr_bytes(&truncated);
        assert!(
            out.contains("Fallback to cpu"),
            "BOM 应被剥掉且内容保留: {out}"
        );

        assert_eq!(decode_stderr_bytes(b"Silero VAD ok"), "Silero VAD ok");
    }

    /// `inference_elapsed` 的契约：缺 `finished` 时必须由下限常量兜底，成功出口不得返回 0
    /// （调用方拿它做除法，0 会变成 0 / inf）。
    #[test]
    fn inference_elapsed_missing_finished_falls_back_to_floor() {
        assert_eq!(clamp_inference_elapsed(0.0), INFERENCE_ELAPSED_FLOOR_SEC);
        assert_eq!(clamp_inference_elapsed(-1.0), INFERENCE_ELAPSED_FLOOR_SEC);
        assert_eq!(
            clamp_inference_elapsed(f64::NAN),
            INFERENCE_ELAPSED_FLOOR_SEC
        );
        assert_eq!(
            clamp_inference_elapsed(f64::INFINITY),
            INFERENCE_ELAPSED_FLOOR_SEC
        );
        // 常量块：这条不变量在**编译期**就钉死（clippy 的 `assertions_on_constants` 也要求
        // 常量断言走 const 块）。下限一旦被改成 0 或负数，这里直接编译失败。
        const {
            assert!(
                INFERENCE_ELAPSED_FLOOR_SEC > 0.0,
                "下限必须是正数，否则除法照样得 inf"
            )
        };
        assert_eq!(clamp_inference_elapsed(2.5), 2.5, "正常值原样透出");
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

    #[test]
    fn clean_sensevoice_tags_removes_all_special_tokens() {
        let raw = "<|zh|><|NEUTRAL|><|Speech|><|withitn|>今天天气很好<|withitn|>";
        assert_eq!(clean_sensevoice_tags_rs(raw), "今天天气很好");
    }

    #[test]
    fn consume_stdout_parses_native_sherpa_output() {
        let native_line = "{\"text\":\"<|zh|><|NEUTRAL|>原生测试识别内容\"}\n".as_bytes();
        let mut cursor = std::io::Cursor::new(native_line.to_vec());
        let summary = consume_stdout(&mut cursor, None, None);
        assert_eq!(summary.segments.len(), 1);
        assert_eq!(summary.segments[0].text, "原生测试识别内容");
    }
}
