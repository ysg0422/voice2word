//! PunctuationEngine — 基于 CT-Transformer (ONNX) 的超轻量极速标点恢复引擎
//! 纯 CPU 单次推理毫秒级，比大模型快 50~100 倍

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::subtitle::Segment;

// provider 实况的判定与承载与 SenseVoice 同源：两处必须是同一套口径，否则
// 「SenseVoice 说回落、标点说正常」会自相矛盾。
use super::sensevoice::{provider_fallback_notice, stderr_provider_fallback, ProviderStatus};

// 取消强杀与 PID 登记复用 whisper 引擎那一套（SenseVoice 也是同一个复用点）：
// 登记表按「PID + 唯一序号」记账，子进程被 `wait()` 回收后立刻摘行，`cancel()` 只对着
// 「尚未回收」的名单强杀，因此不存在拿一个已归还系统的 PID 去误杀无关进程的可能。
use super::whisper::{take_live_pids, ChildEntry, ChildPidGuard};

// ───────────────────────── 线程数夹取 ─────────────────────────

/// CT-Transformer 标点恢复单会话线程数上限。
///
/// 与 SenseVoice 同源结论：16 核机器实测单会话在 8 线程附近已饱和，线程越多反而
/// 越慢（解码是 batch=1 的串行延迟链，多线程只加同步开销）。而 `whisper_threads`
/// 是全局共享项（16 核机器上可配到 16），CLI/GUI 都原样透传给本引擎；不夹的话，
/// 用户配 16 线程会真的以 16 线程起 runner，造成「改了没效果 / 越改越慢」的观感。
const MAX_THREADS: u32 = 8;

/// CT-Transformer 单会话线程数下限：1 线程在长文本上明显掉速，
/// 与设置页滑条最左档保持一致取 2。
const MIN_THREADS: u32 = 2;

/// 把线程数夹到本引擎自述的有效区间 [`MIN_THREADS`, `MAX_THREADS`]。
fn clamp_threads(threads: u32) -> u32 {
    threads.clamp(MIN_THREADS, MAX_THREADS)
}

// ───────────────────────── 有界等待、取消与输出收口 ─────────────────────────

/// 标点阶段的有界等待上限：到点即强杀，按「超时失败」收尾，绝不让调用方永久阻塞。
///
/// 为什么取 300 秒：标点是**秒级操作**——CT-Transformer 单次推理毫秒级，本引擎把整批
/// 字幕一次性喂给 runner，几千段在 CPU 上也只要数秒到数十秒。300 秒给足了「Python 冷
/// 启动 + onnxruntime/模型加载（首次可达十几秒）+ 低端机」的余量，同时把「runner 卡死」
/// 从「永久阻塞（只能关应用）」降级成「5 分钟后失败、字幕保持未标点」。
/// 与 `llm.rs::cli_wait_budget` 同思路：命令行路径没有读超时，上限必须自己带。
const PUNC_WAIT_BUDGET: Duration = Duration::from_secs(300);

/// 有界等待的轮询间隔：50ms 是「取消/超时的响应延迟」与「空转开销」的折中
/// （与 `llm.rs` 的命令行等待循环取同一个值）。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// stderr 进日志与错误串前的字符上限。
///
/// runner 失败时会把整段 Python traceback（有时还连带模型加载日志）吐到 stderr，
/// 原样拼进 `warn!`/`bail!` 能到几十万字符：日志被刷爆，用户看到的错误串也全是噪音，
/// 真正的原因反而被埋掉。截断只影响展示，判定（如 provider 回落特征串）仍用完整内容。
const STDERR_LOG_MAX_CHARS: usize = 2000;

/// 有界等待的出口（外加「等待本身失败」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// 子进程自行退出（退出码另行判断）。
    Exited,
    /// 用户点了终止。
    Cancelled,
    /// 到点仍未退出。
    TimedOut,
    /// `try_wait` 本身报错（句柄异常），不能死循环。
    WaitFailed,
}

/// 本次轮询是否该退出等待；`None` = 继续等。
///
/// 优先级：**取消 > 超时**。两者同时成立时按「已取消」处理——用户主动终止是更强的语义，
/// 文案也该显示「已取消」，而不是让用户以为自己撞上了超时。抽成纯函数是为了让这条
/// 优先级能在单测里直接钉住，不必真的起子进程、等满 5 分钟。
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
/// 返回「出口 + `try_wait` 拿到的退出状态」（非正常退出时为 `None`）。抽成独立函数是
/// 为了让三条出口都能在单测里用真实子进程 + 极小预算跑一遍，不必等满 [`PUNC_WAIT_BUDGET`]，
/// 也不必依赖 python 与模型文件。本函数**不**负责杀进程与收尸：那是调用方的事
/// （先 kill 再 wait），这样「谁在什么时候强杀」在调用点一眼可见。
fn wait_for_child<F>(
    child: &mut Child,
    budget: Duration,
    poll: Duration,
    cancelled: F,
) -> (WaitOutcome, Option<ExitStatus>)
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
                warn!(error = %err, "等待标点子进程失败，终止该进程");
                return (WaitOutcome::WaitFailed, None);
            }
        }
        // 判定抽成纯函数：让「取消优先于超时」这条语义能被单测直接钉住。
        if let Some(outcome) = wait_outcome(cancelled(), Instant::now() >= deadline) {
            return (outcome, None);
        }
        std::thread::sleep(poll);
    }
}

/// 截断过长的 stderr：保留前 `max` 个字符（按字符切，不会切坏 UTF-8），并在尾部注明省略量。
fn truncate_stderr(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…（stderr 过长，已省略 {} 字符）", total - max)
}

/// 统计有多少段在 runner 结果里找不到对应 `index`（结果与输入的对应校验）。
fn missing_index_count(segments: &[Segment], map: &HashMap<usize, String>) -> usize {
    segments
        .iter()
        .filter(|s| !map.contains_key(&s.index))
        .count()
}

/// 起一个后台线程把子进程管道抽干，返回线程句柄；`None` = 该管道没有被 pipe。
///
/// 管道缓冲区只有几十 KB：没人读的话，子进程写满就阻塞在 write 上，而父进程又在
/// 等它退出——双方一起卡死（在「有界等待」下会被误判成 runner 挂住而强杀）。
fn drain_pipe<P: Read + Send + 'static>(
    pipe: Option<P>,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut p| {
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let _ = p.read_to_end(&mut buf);
            buf
        })
    })
}

/// 收尾抽干线程：读线程 panic / 未启动都按空字节处理，不把诊断通道的问题升级成主流程失败。
fn join_pipe(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

/// 标点阶段的可辨识失败：流水线据此区分「用户取消」与「超时」的文案。
///
/// 为什么不用裸字符串表达：上层要对两者展示不同文案（取消是静默收尾、超时是明确告警），
/// 靠 `err.to_string().contains("取消")` 这种约定一改文案就失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PuncStageError {
    /// 用户点了「终止」。
    Cancelled,
    /// 超过 [`PUNC_WAIT_BUDGET`] 仍未退出，已强杀。
    TimedOut,
}

impl std::fmt::Display for PuncStageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "标点阶段已被用户取消"),
            Self::TimedOut => write!(f, "标点引擎超时，已终止；字幕保持未标点"),
        }
    }
}

impl std::error::Error for PuncStageError {}

/// 跨平台强杀指定进程及其子进程树（用户「终止转写」与超时兜底共用）。
///
/// 与 whisper / sensevoice 各自那份同源；`taskkill` 失败时留一条 warn，避免
/// 「点了终止但标点照跑」在日志里完全查不到线索。
fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new(taskkill_program());
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        match cmd.args(["/F", "/T", "/PID", &pid.to_string()]).output() {
            Ok(out) if out.status.success() => {}
            Ok(out) => warn!(pid, code = ?out.status.code(), "taskkill 未能终止标点子进程树"),
            Err(err) => warn!(pid, error = %err, "taskkill 启动失败，标点子进程可能仍在运行"),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

/// 强杀时使用的 `taskkill` 程序路径（Windows）。
///
/// 为什么不裸用 `Command::new("taskkill")`：Windows 创建进程时的搜索顺序把**当前目录**
/// 排在系统目录之前，应用目录里被塞一个同名 `taskkill.exe`，就会在用户点「终止」时
/// 被执行。这里优先用 `%SystemRoot%\System32\taskkill.exe`，只有它确实不存在时才回退
/// PATH 查找。与 whisper 引擎的 `kill_process_tree` 同一套加固口径（那边是模块私有函数，
/// 无法跨模块复用）。
#[cfg(windows)]
fn taskkill_program() -> String {
    let candidate = taskkill_program_from(std::env::var("SystemRoot").ok().as_deref());
    if Path::new(&candidate).is_file() {
        candidate
    } else {
        "taskkill".to_string()
    }
}

/// [`taskkill_program`] 的纯函数部分：只做路径拼装，便于单测（不读环境变量、不碰文件系统）。
#[cfg(windows)]
fn taskkill_program_from(system_root: Option<&str>) -> String {
    match system_root.map(str::trim).filter(|s| !s.is_empty()) {
        Some(root) => format!(
            "{}\\System32\\taskkill.exe",
            root.trim_end_matches(['\\', '/'])
        ),
        None => "taskkill".to_string(),
    }
}

/// RAII 守卫：持有「已 `spawn()` 但尚未 `wait()`」的子进程，守卫离开作用域时若句柄仍在，
/// 就 `kill()` + `wait()` 回收它。
///
/// 为什么不能只在错误分支里补一行 kill：`spawn()` 与 `wait_with_output()` 之间夹着三条
/// 提前退出路径——`stdin.write_all(...)?`、`stdin.flush()?`，以及调用方传入的 `progress_cb`
/// 回调（UI 层代码，panic 会直接展开栈）。任意一条触发时，`Child` 句柄被丢弃但 Python
/// runner 仍在运行，占着解释器与模型内存，成为没人回收的孤儿进程。
/// 把回收绑到栈变量生命周期上，所有退出方式（提前返回、panic 展开）都覆盖，
/// 以后新增分支也不必再记着补 kill。
/// （与 `src/utils/child_registry.rs` 的分工：那边收容「发起后没人 `wait()`」的进程；
/// 这边管的是「本来就要 `wait()`、但中途可能提前退出」的进程，所以不进全局登记表。）
struct ChildReaper(Option<Child>);

impl ChildReaper {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    /// 借用内部子进程，用于在交出句柄前写 stdin。
    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("子进程句柄在 take() 之前必然存在")
    }

    /// 交出句柄，改由调用方 `wait_with_output()` 接管；守卫此后不再干预。
    fn take(&mut self) -> Child {
        self.0.take().expect("子进程句柄只能被取走一次")
    }
}

impl Drop for ChildReaper {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // 进程可能已自行退出，kill 失败不算错误
            let _ = child.kill();
            // kill 之后必须 wait，否则句柄不释放
            let _ = child.wait();
        }
    }
}

#[derive(Clone)]
pub struct PunctuationEngine {
    runner_script: PathBuf,
    model_path: PathBuf,
    threads: u32,
    /// 运行 runner 脚本的 Python 解释器（默认 "python"，可配置为绝对路径）
    python_path: PathBuf,
    /// ONNX 执行后端：`cpu` | `dml`。默认 `cpu`（零回归）；
    /// `dml` 只有在 Python 环境装了 DirectML 版 sherpa-onnx 时才真正生效。
    provider: String,
    /// 回落可见化：runner 回报的 provider 实况（首个回报为准）。
    provider_status: Arc<Mutex<Option<ProviderStatus>>>,
    /// 回落告警只打一次。
    fallback_reported: Arc<AtomicBool>,
    /// 取消标志：`cancel()` 置位、`reset()` 复位（由管线在任务起手时调用）。
    /// 与 whisper / sensevoice 同款语义，保证「终止」对标点阶段同样即时生效。
    cancel: Arc<AtomicBool>,
    /// 在册的 runner 子进程 PID（供 `cancel()` 强杀）。
    active_children: Arc<Mutex<Vec<ChildEntry>>>,
}

impl PunctuationEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
    ) -> Self {
        Self::with_python(runner_script, model_path, threads, PathBuf::from("python"))
    }

    /// 指定 Python 解释器（用于 PATH 上有多个 Python、默认 `python` 缺依赖的场景）
    pub fn with_python<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
        python_path: P3,
    ) -> Self {
        Self::with_python_and_provider(runner_script, model_path, threads, python_path, "cpu")
    }

    /// 完整指定：解释器 + ONNX 执行后端（`cpu` | `dml`）。
    pub fn with_python_and_provider<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
        python_path: P3,
        provider: &str,
    ) -> Self {
        let provider = match provider.trim().to_ascii_lowercase().as_str() {
            "dml" | "directml" => "dml".to_string(),
            _ => "cpu".to_string(),
        };
        Self {
            runner_script: runner_script.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            threads: clamp_threads(threads),
            python_path: python_path.as_ref().to_path_buf(),
            provider,
            provider_status: Arc::new(Mutex::new(None)),
            fallback_reported: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            active_children: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn is_available(&self) -> bool {
        self.model_path.exists() && self.runner_script.exists()
    }

    /// 记录 runner 回报的 provider 实况；命中回落时打印一次可操作告警。
    fn record_provider(&self, requested: &str, actual: &str) {
        let status = ProviderStatus {
            requested: requested.trim().to_string(),
            actual: actual.trim().to_string(),
        };
        let mut slot = self
            .provider_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let first = slot.is_none();
        if first {
            *slot = Some(status.clone());
        }
        drop(slot);
        // 只对「首次落盘的那次回报」告警：首个回报为准的口径下，后续回报即便
        // 与首个矛盾也不得覆盖状态，更不该借此重复刷告警。
        if first {
            if let Some(notice) = provider_fallback_notice(&status.requested, &status.actual) {
                if !self.fallback_reported.swap(true, Ordering::SeqCst) {
                    warn!("{notice}");
                }
            }
        }
    }

    /// 读取 runner 回报的 provider 实况（尚未跑过标点时可能为 `None`）。
    pub fn provider_status(&self) -> Option<ProviderStatus> {
        self.provider_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 用户请求终止：置位取消标志并强杀正在运行的标点 runner 子进程。
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

    /// 复位取消标志（每次新任务开始前由管线调用，语义与 whisper / sensevoice 一致）。
    ///
    /// 与 `cancel()` 成对：不复位的话，上一轮的取消会把下一轮的标点阶段直接掐掉。
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// 当前是否处于「已取消」状态（管线与单测的观察口）。
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// 登记 runner PID，并回报「登记这一刻取消是否已经到达」。
    ///
    /// 收口 `cancel()` 与「刚 `spawn()`、尚未登记」之间的竞态：`cancel()` 先置位取消
    /// 标志，再 `take_live_pids()` 取名单强杀；若它恰好插在 `spawn()` 成功与本次登记
    /// 之间，登记表里还没有这一行，那一发信号就丢了，runner 会一直跑到自己结束。
    /// `cancel()` 的置位与本次复查都是 SeqCst，而 drain 与 register 又在同一把 Mutex
    /// 下互斥：drain 没看到本行 ⇒ drain 早于 register ⇒ 置位早于 register ⇒ 此处复查
    /// 必然读到 `true`。于是「drain 发信号」与「本处补杀」两条路径必有一条命中。
    ///
    /// 返回值交给调用方去补 `kill_process_tree`，而不是在本函数里直接杀，是为了让
    /// 这条时序能在单测里直接断言，不必真的拉起子进程、发系统信号。
    fn register_child(&self, pid: u32) -> (ChildPidGuard, bool) {
        let guard = ChildPidGuard::register(self.active_children.clone(), pid);
        (guard, self.is_cancelled())
    }

    pub fn add_punctuation(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<crate::engines::TextProgressCb>,
    ) -> Result<Vec<Segment>> {
        if segments.is_empty() {
            return Ok(segments);
        }

        if !self.is_available() {
            warn!(
                "CT-Punc 模型或脚本未就绪: {:?}, 跳过极速标点",
                self.model_path
            );
            return Ok(segments);
        }

        if let Some(ref cb) = progress_cb {
            cb(0.1, "CT-Transformer 极速标点引擎加载中...");
        }

        let started = Instant::now();

        #[derive(serde::Serialize)]
        struct InputItem<'a> {
            index: usize,
            text: &'a str,
        }

        let items: Vec<InputItem> = segments
            .iter()
            .map(|s| InputItem {
                index: s.index,
                text: &s.text,
            })
            .collect();

        let input_json = serde_json::to_vec(&items).context("序列化字幕输入失败")?;

        let mut cmd = Command::new(&self.python_path);
        super::media_pipeline::apply_python_env(&mut cmd);
        cmd.arg(&self.runner_script)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--threads")
            .arg(clamp_threads(self.threads).to_string())
            .arg("--provider")
            .arg(&self.provider)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 让路模式：标点恢复也是 Python 子进程，同样要跟随用户的开关
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        // 与 SenseVoice 同因同治：找不到解释器时把「缺 Python」讲明白，
        // 而不是把 OS 的「系统找不到指定的文件」原样丢给用户。
        let program = cmd.get_program().to_os_string();
        let python_hint = (std::path::Path::new(&program) == self.python_path.as_path())
            .then_some(self.python_path.as_path());
        // spawn 前的取消早退（与 whisper / sensevoice 同一道门）：取消若在准备区就已到达，
        // 没必要真的把 Python 拉起来再靠 register_child 的复查把它杀掉——那几毫秒里
        // 解释器启动与 ONNX 动态库加载已经白做一遍，用户看到的是「点了终止却还闪了
        // 一下子进程」。返回形态与下面的取消出口一致：`PuncStageError::Cancelled`。
        if self.is_cancelled() {
            return Err(anyhow::Error::new(PuncStageError::Cancelled));
        }

        let child = cmd.spawn().map_err(|err| {
            super::spawn_failure_message(&err, &program, " 标点恢复进程", python_hint)
        })?;
        crate::utils::child_registry::adopt(&child);

        // 声明顺序是有意的：`reaper` 先声明 ⇒ 逆序析构 ⇒ `child_guard` 先摘登记行、
        // `reaper` 后 kill+wait+关闭进程句柄。与 whisper / sensevoice 的次序一致，也是
        //「Windows 上不存在陈旧 PID 被 cancel() 误杀」这条结论的前提：PID 只有在进程
        // 对象的最后一个句柄关闭后才可能被复用，而那一刻登记行早已摘掉。
        // 从这里到进程被 wait() 回收之间的任何提前退出都由守卫兜底回收子进程；
        // 登记行由 child_guard 的 Drop 兜底摘除（正常跑完时在 wait() 返回处主动 disarm）。
        let mut reaper = ChildReaper::new(child);
        // 登记 runner PID（供「终止」强杀）并复查竞态：与 whisper / sensevoice 同一套。
        let (mut child_guard, cancelled_before_register) =
            self.register_child(reaper.child_mut().id());
        if cancelled_before_register {
            // cancel() 恰好插在 spawn() 成功与上面的登记之间：那会儿表里还没有本行，
            // take_live_pids() 拿到空名单，这一发强杀信号就丢了。register_child 的
            // SeqCst 复查保证此处必然命中。此刻句柄仍在，PID 不可能被系统复用。
            kill_process_tree(reaper.child_mut().id());
        }
        // 取消已经到达就没必要再写 stdin / 等结果：直接按「已取消」返回，让调用方尽快收尾。
        if self.is_cancelled() {
            return Err(anyhow::Error::new(PuncStageError::Cancelled));
        }

        // 必须边等边抽干管道：管道缓冲区只有几十 KB，整批字幕的结果 JSON 很容易写满，
        // 子进程会阻塞在 write 上、永远不退出——那样本方法只会「超时误杀一个其实正常
        // 工作的 runner」。读线程负责抽干。抽干必须排在写 stdin 之前：runner 收尾会把
        // 捕获到的原生日志一次性回灌 stderr，无人读时它写满管道就阻塞，而我们还可能在
        // 写 stdin——父子互等，超时预算根本没机会启动。
        let stdout_reader = drain_pipe(reaper.child_mut().stdout.take());
        let stderr_reader = drain_pipe(reaper.child_mut().stderr.take());

        // stdin 写入同样必须离开本线程：Windows 管道容量只有 64KB，而 runner 是
        // `import sherpa_onnx` + 建好标点会话（见 tools/punc_runner.py）之后才读 stdin。
        // 若它卡在启动阶段（正是本方法要防的场景），几千段字幕的输入 JSON 会让主线程
        // 阻塞在 WriteFile 上——此时超时预算还没开始计时，`spawn_blocking` 依旧永不返回。
        // 写入线程由后面的 kill+wait 保证能退出：子进程一死，管道写端报错的 WriteFile
        // 立刻返回，因此这里 join 不会挂。
        let stdin_writer = reaper.child_mut().stdin.take().map(|mut stdin| {
            std::thread::spawn(move || -> std::io::Result<()> {
                stdin.write_all(&input_json)?;
                stdin.flush()
            })
        });

        if let Some(ref cb) = progress_cb {
            cb(0.5, "正在进行毫秒级标点预测与断句...");
        }

        // 有界等待：正常退出 / 取消 / 超时三个出口，出口之后一律 kill + wait 回收。
        // 旧写法直接 `wait_with_output()`：runner 卡死（onnxruntime 挂住、管道互等）时
        // 永不返回，`spawn_blocking` 也就永不返回，界面永远停在「极速标点」，
        // 唯一出口是关掉应用。
        let child_pid = reaper.child_mut().id();
        let (outcome, status) = wait_for_child(
            reaper.child_mut(),
            PUNC_WAIT_BUDGET,
            WAIT_POLL_INTERVAL,
            || self.is_cancelled(),
        );

        let mut child = reaper.take();
        if !matches!(outcome, WaitOutcome::Exited) {
            // 先按进程树强杀（与 whisper / sensevoice 的取消路径同一套），再 kill 句柄
            // 兜底：runner 是 Python，但 ONNX 运行时在部分后端下会拉起辅助进程。
            // 句柄是我们自己 spawn 的，TerminateProcess 必然成功，因此后面的 wait()
            // 不会变成第二次永久阻塞——它只是回收句柄（kill 之后必须 wait）。
            kill_process_tree(child_pid);
            let _ = child.kill();
        }
        // kill 之后必须 wait，否则句柄不释放；已退出的进程由 try_wait 收过尸，这里拿到
        // 的是缓存状态，不会二次阻塞。
        let _ = child.wait();
        // 进程已被 wait() 回收：立刻摘掉登记行，杜绝 cancel() 拿着陈旧 PID 去误杀一个
        // 复用该号码的无关进程（详见 ChildPidGuard::disarm）。
        child_guard.disarm();
        // 进程已退出 ⇒ 管道关闭 ⇒ 抽干线程立刻结束：即使是超时强杀也不会卡在这里。
        let stdout_bytes = join_pipe(stdout_reader);
        let stderr_bytes = join_pipe(stderr_reader);
        let stderr_raw = String::from_utf8_lossy(&stderr_bytes);
        // 在分流之前就 join 写入线程：此刻子进程已死 ⇒ 管道写端已断 ⇒ 阻塞中的 WriteFile
        // 立刻返回，join 不会挂。它的失败只用来判「runner 根本没读走输入」，不参与
        // 取消/超时的定性（那两条出口的写失败是预期内的）。
        let stdin_error: Option<std::io::Error> = stdin_writer
            .and_then(|h| h.join().ok())
            .and_then(|res| res.err());

        match outcome {
            // 取消/超时都不得把半截输出当结果写回，也不得继续往下走解析。
            WaitOutcome::Cancelled => {
                info!("CT-Punc 标点已取消，字幕保持未标点");
                return Err(anyhow::Error::new(PuncStageError::Cancelled));
            }
            WaitOutcome::TimedOut => {
                // 带上 runner 最后的 stderr（截断展示）：排查时能看出它卡在哪一步——
                // 导入依赖、建 ONNX 会话，还是卡在读输入之前。
                warn!(
                    seconds = PUNC_WAIT_BUDGET.as_secs(),
                    stderr = %truncate_stderr(&stderr_raw, STDERR_LOG_MAX_CHARS),
                    "CT-Punc 超过等待上限仍未退出，已强杀；字幕保持未标点"
                );
                return Err(anyhow::Error::new(PuncStageError::TimedOut));
            }
            // try_wait 报错的 io 详情已在 `wait_for_child` 里 warn 过，这里给上层一句可读结论。
            WaitOutcome::WaitFailed => return Err(anyhow::anyhow!("等待标点子进程退出失败")),
            WaitOutcome::Exited => {}
        }

        let status = status.expect("Exited 出口必然带退出码");
        if !status.success() {
            let stderr = truncate_stderr(&stderr_raw, STDERR_LOG_MAX_CHARS);
            warn!("CT-Punc 执行报错: {}", stderr);
            anyhow::bail!("CT-Punc 执行失败: {}", stderr);
        }
        // 进程正常退出却没读走输入：结果不可信（stdout 只会是空/半截），如实报错而不是
        // 让后面的 JSON 解析失败去背这个锅。
        if let Some(err) = stdin_error {
            return Err(anyhow::Error::new(err).context("向标点子进程 stdin 写入输入 JSON 失败"));
        }

        // 兜底通道：runner 未回报 provider（旧版脚本）时，从 stderr 特征串识别回落。
        // 与 stdout 的 `provider` 字段同源，`record_provider` 首次为准，不会重复告警。
        // 判定用**完整** stderr——上面的截断只服务于日志与错误串的展示长度。
        if stderr_provider_fallback(&stderr_raw) {
            let requested = self.provider.clone();
            self.record_provider(&requested, "cpu");
        }

        #[derive(serde::Deserialize)]
        struct OutputResult {
            index: usize,
            polished: String,
        }

        /// runner 顶层回报的 provider 实况。老版本 runner 没有这个键，`Option`
        /// 让缺省成为「未回报」而不是解析失败。
        #[derive(serde::Deserialize)]
        struct OutputProvider {
            #[serde(default)]
            requested: String,
            #[serde(default)]
            actual: String,
        }

        #[derive(serde::Deserialize)]
        struct OutputPayload {
            #[allow(dead_code)]
            count: usize,
            #[allow(dead_code)]
            elapsed_sec: f64,
            #[serde(default)]
            provider: Option<OutputProvider>,
            results: Vec<OutputResult>,
        }

        let payload: OutputPayload =
            serde_json::from_slice(&stdout_bytes).context("解析标点输出 JSON 失败")?;

        if let Some(p) = payload.provider.as_ref() {
            self.record_provider(&p.requested, &p.actual);
        }

        let map: HashMap<usize, String> = payload
            .results
            .into_iter()
            .map(|r| (r.index, r.polished))
            .collect();

        // 结果与输入的对应校验：runner 若按自身分句重编号（或协议版本不匹配），会有段落
        // 查不到结果。旧写法对这类段落静默写回原文，完成日志又只打 `segments.len()`，
        // 于是「整批都没标上」在日志里完全看不出来。这里只补一条汇总告警，正常路径的
        // 写回行为不变。
        let missing = missing_index_count(&segments, &map);
        for seg in segments.iter_mut() {
            if let Some(pol) = map.get(&seg.index) {
                seg.polished = pol.clone();
            } else {
                seg.polished = seg.text.clone();
            }
        }
        if missing > 0 {
            warn!(
                "CT-Punc 有 {} / {} 段未在结果中找到对应 index，已写回未标点原文",
                missing,
                segments.len()
            );
        }

        if let Some(ref cb) = progress_cb {
            cb(1.0, "标点恢复完成");
        }

        info!(
            elapsed = ?started.elapsed(),
            count = segments.len(),
            "CT-Transformer 标点恢复完成"
        );

        Ok(segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "windows")]
    use std::time::{Duration, Instant};

    // 这些测试用 cmd/ping 验证守卫行为，只在 Windows 上跑（项目目标平台）。

    #[cfg(target_os = "windows")]
    fn spawn_long_lived() -> Child {
        // ping 会阻塞约 30 秒，足够验证守卫析构真的杀了它
        Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ping")
    }

    #[cfg(target_os = "windows")]
    fn spawn_short_lived() -> Child {
        Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    /// 提前返回（如 `stdin.write_all(...)?` 失败）丢弃守卫时，仍在运行的子进程必须被回收。
    /// 若守卫只是「移除句柄」而不 kill，这个 30 秒的 ping 会拖到超时。
    #[cfg(target_os = "windows")]
    #[test]
    fn dropping_reaper_kills_running_child() {
        let reaper = ChildReaper::new(spawn_long_lived());
        let started = Instant::now();
        drop(reaper);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "守卫析构必须 kill+wait 后立即返回，而不是等子进程自己跑完"
        );
    }

    /// `take()` 交出句柄后，守卫析构不得再干预——否则会与调用方的 `wait()`
    /// 争抢同一个进程，造成二次 wait 或误杀正常完成的进程。
    #[cfg(target_os = "windows")]
    #[test]
    fn taken_child_is_not_reaped_by_guard() {
        let mut reaper = ChildReaper::new(spawn_short_lived());
        let mut child = reaper.take();
        drop(reaper);
        let status = child.wait().expect("wait 交出的子进程");
        assert!(status.success(), "take() 之后子进程应由调用方正常回收");
    }

    /// 进程已自行退出时，`kill()` 会失败，但守卫析构必须不 panic、不留句柄。
    #[cfg(target_os = "windows")]
    #[test]
    fn dropping_reaper_after_child_exited_is_harmless() {
        let reaper = ChildReaper::new(spawn_short_lived());
        std::thread::sleep(Duration::from_millis(500));
        drop(reaper);
    }

    /// `child_mut()` 必须借出真正被 spawn 的那个进程，供写 stdin 使用。
    #[cfg(target_os = "windows")]
    #[test]
    fn child_mut_exposes_spawned_child() {
        let child = spawn_short_lived();
        let pid = child.id();
        let mut reaper = ChildReaper::new(child);
        assert_eq!(reaper.child_mut().id(), pid);
    }

    // ---- provider 回落可见化（与 SenseVoice 共用同一套判定） ----

    /// 造一个不依赖真实 python/model 的引擎，只验证状态机。
    fn punc_engine_for_provider_state() -> PunctuationEngine {
        PunctuationEngine::with_python_and_provider("runner.py", "model.onnx", 1, "python", "dml")
    }

    #[test]
    fn punc_records_provider_fallback_from_runner_report() {
        let engine = punc_engine_for_provider_state();
        assert!(engine.provider_status().is_none());
        engine.record_provider("directml", "cpu");
        let status = engine.provider_status().expect("回报后应有实况");
        assert!(status.fell_back(), "请求 directml 落到 cpu 必须判为回落");
    }

    #[test]
    fn punc_provider_status_keeps_first_report() {
        let engine = punc_engine_for_provider_state();
        engine.record_provider("dml", "cpu");
        engine.record_provider("dml", "dml");
        assert_eq!(
            engine.provider_status().unwrap().actual,
            "cpu",
            "首个回报为准，后续重复不得覆盖"
        );
    }

    /// 构造器必须把线程数夹到 runner 的有效区间：`whisper_threads` 是全局共享项
    /// （16 核可配到 16），原样透传会让 runner 真的以 16 线程启动，与
    /// 「8 线程已饱和」的实测结论相悖。
    #[test]
    fn constructor_clamps_threads_to_runner_range() {
        let threads_of = |threads: u32| {
            PunctuationEngine::with_python_and_provider(
                "runner.py",
                "model.onnx",
                threads,
                "python",
                "cpu",
            )
            .threads
        };
        assert_eq!(threads_of(0), MIN_THREADS, "0 必须抬到下限");
        assert_eq!(threads_of(1), MIN_THREADS, "1 低于下限，抬到 MIN_THREADS");
        assert_eq!(threads_of(64), MAX_THREADS, "64 超上限，夹到 MAX_THREADS");
        assert_eq!(threads_of(4), 4, "区间内必须原样保留");
    }

    // ---- 有界等待 / 取消 / 结果对应校验 / stderr 截断（纯判定，不必真起子进程） ----

    /// 超时预算必须有界且落在「秒级操作 + 冷启动余量」的区间内：太小会在慢机器上
    /// 误杀一个其实正常工作的 runner，太大就等于没把整条流水线从永久阻塞里救出来。
    #[test]
    fn punc_wait_budget_is_bounded() {
        assert_eq!(
            PUNC_WAIT_BUDGET,
            Duration::from_secs(300),
            "预算值本身要钉住"
        );
        assert!(
            PUNC_WAIT_BUDGET >= Duration::from_secs(60),
            "标点是秒级操作，但 Python 冷启动 + 模型加载要余量，下限不得低于 60s"
        );
        assert!(
            PUNC_WAIT_BUDGET <= Duration::from_secs(900),
            "上限超过 15 分钟等于没把「永久阻塞」降级成有限失败"
        );
        assert!(WAIT_POLL_INTERVAL > Duration::ZERO, "轮询间隔不能为 0");
        assert!(
            WAIT_POLL_INTERVAL <= Duration::from_millis(500),
            "轮询间隔过长会让取消/超时的响应明显迟滞"
        );
    }

    /// 判定语义：都没命中就继续等；命中就退出；两者同时成立时「取消」优先
    /// （用户主动终止是更强的语义，文案也该显示「已取消」而不是「超时」）。
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

    /// 「用户取消」与「超时」必须能从返回的错误里分辨出来：上层据此展示不同文案，
    /// 靠错误字符串里的关键字判断会一改文案就失效。
    #[test]
    fn cancel_and_timeout_errors_are_distinguishable() {
        let cancelled = anyhow::Error::new(PuncStageError::Cancelled);
        let timed_out = anyhow::Error::new(PuncStageError::TimedOut);
        assert_eq!(
            cancelled.downcast_ref::<PuncStageError>(),
            Some(&PuncStageError::Cancelled)
        );
        assert_eq!(
            timed_out.downcast_ref::<PuncStageError>(),
            Some(&PuncStageError::TimedOut)
        );
        assert!(
            timed_out.to_string().contains("字幕保持未标点"),
            "超时文案要讲清「字幕保持未标点」，而不是把半截输出当结果"
        );
    }

    /// 取消入口的语义：置位后可读、reset() 后新任务不被上一轮取消误伤，且没有在册
    /// runner 时 cancel() 不会凭空取到要杀的 PID。
    #[test]
    fn cancel_flag_is_observable_and_resettable() {
        let engine = punc_engine_for_provider_state();
        assert!(!engine.is_cancelled(), "初始必须是未取消");
        engine.cancel();
        assert!(engine.is_cancelled(), "cancel() 之后必须可观察到已取消");
        assert!(
            take_live_pids(&engine.active_children).is_empty(),
            "没有在册 runner 时 cancel() 不得取到任何要杀的 PID"
        );
        engine.reset();
        assert!(!engine.is_cancelled(), "reset() 后不得残留上一轮的取消");
    }

    /// 取消必须真的把在册 runner 杀掉：用户点「终止」后标点不能继续跑到自己结束。
    /// 用 `ping` 冒充 runner——登记它的 PID 后 cancel()，它必须被立刻终止。
    #[cfg(target_os = "windows")]
    #[test]
    fn cancel_kills_registered_child_promptly() {
        let engine = punc_engine_for_provider_state();
        let mut child = spawn_long_lived();
        let _guard = ChildPidGuard::register(engine.active_children.clone(), child.id());
        let started = Instant::now();
        engine.cancel();
        let _ = child.wait();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel() 必须强杀在册子进程，而不是等它自己跑完"
        );
        assert!(engine.is_cancelled());
        assert!(
            take_live_pids(&engine.active_children).is_empty(),
            "cancel() 取走名单后登记表必须为空，避免重复强杀同一个 PID"
        );
    }

    /// 结果与输入的对应校验：缺 index 的段要能被数出来（旧行为是静默写回原文，
    /// 完成日志又只打总段数，「整批都没标上」在日志里看不出来）。
    #[test]
    fn missing_index_count_reports_unmatched_segments() {
        let s1 = Segment::new(1, 0.0, 1.0, "a");
        let s2 = Segment::new(2, 1.0, 2.0, "b");
        let mut map: HashMap<usize, String> = HashMap::new();
        map.insert(1usize, "a，".to_string());
        assert_eq!(missing_index_count(&[s1.clone(), s2.clone()], &map), 1);
        map.insert(2usize, "b。".to_string());
        assert_eq!(missing_index_count(&[s1, s2], &map), 0);
        assert_eq!(missing_index_count(&[], &map), 0, "空输入恒为 0");
    }

    /// stderr 截断：短文本原样返回；超长文本按字符截断（不切坏 UTF-8）并注明省略量，
    /// 结果长度有界，不会把日志刷爆。
    #[test]
    fn stderr_truncation_is_bounded_and_annotated() {
        assert_eq!(truncate_stderr("short", STDERR_LOG_MAX_CHARS), "short");
        let long: String = "错".repeat(STDERR_LOG_MAX_CHARS + 137);
        let out = truncate_stderr(&long, STDERR_LOG_MAX_CHARS);
        assert!(
            out.starts_with(&"错".repeat(STDERR_LOG_MAX_CHARS)),
            "必须保留前 N 个字符"
        );
        assert!(
            out.contains("已省略 137 字符"),
            "截断后必须注明省略了多少字符: {out}"
        );
        assert!(
            out.chars().count() <= STDERR_LOG_MAX_CHARS + 40,
            "截断结果长度必须有界"
        );
    }

    /// 有界等待的三条出口都要能用**真实子进程**跑通：正常退出、超时（预算极小 + 长跑
    /// 进程）、取消。预算与轮询间隔是参数，因此不必等满 300s，也不依赖 python / 模型。
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

        // 取消：预算给到 30 秒，但取消标志已置位 → 立刻返回「取消」，不必等到超时。
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

    /// `taskkill` 必须优先用系统目录里的绝对路径：应用目录里被塞一个同名 exe 时，
    /// 裸用 "taskkill"（Windows 会先搜当前目录）就会在用户点终止时执行那个冒牌程序。
    #[cfg(target_os = "windows")]
    #[test]
    fn taskkill_path_prefers_system32() {
        assert_eq!(
            taskkill_program_from(Some("C:\\Windows")),
            "C:\\Windows\\System32\\taskkill.exe"
        );
        assert_eq!(
            taskkill_program_from(Some("C:\\Windows\\")),
            "C:\\Windows\\System32\\taskkill.exe",
            "尾部反斜杠不得拼出双反斜杠"
        );
        assert_eq!(
            taskkill_program_from(Some("  ")),
            "taskkill",
            "SystemRoot 为空白时必须回退 PATH 查找"
        );
        assert_eq!(taskkill_program_from(None), "taskkill");
    }
}
