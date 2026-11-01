//! VocalSepEngine — UVR MDX-Net 类 ONNX 人声分离引擎（识别前置降噪）
//!
//! BGM / 背景人声重的素材上识别精度差：识别前先用源分离模型抽出人声轨，再送进
//! 识别链路。DSP 与推理都在 `tools/vocal_sep_runner.py`（numpy 手写 STFT/ISTFT +
//! onnxruntime），本文件只负责**进程契约**：spawn、双端抽干、有界等待、取消、
//! 结果 JSON 解析、provider 回落可见化——与 `punc.rs` / `sensevoice.rs` 同一套
//! 口径（错误要可操作、绝不静默吞错）。

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::utils::AppConfig;

// ───────────────────────── 有界等待预算 ─────────────────────────

/// 等待预算的固定开销（秒）：Python 冷启动 + onnxruntime 加载 + 模型读盘。
const BASE_WAIT_SECS: u64 = 120;

/// 每秒输入音频追加的等待预算（秒）。
///
/// CPU 上 MDX-Net 源分离约 0.5~2× 实时（低端机更慢），DirectML 明显更快；
/// 取 6× 实时做余量，慢机器上不会误杀一个其实正常工作的 runner。
const WAIT_SECS_PER_INPUT_SEC: f64 = 6.0;

/// 等待预算封顶：多小时素材的「120 + 6×输入秒」会到数小时，必须有界——
/// 「runner 卡死」要降级成有限时间后的失败，而不是第二次永久阻塞。
const MAX_WAIT_SECS: u64 = 4 * 3600;

/// 有界等待的轮询间隔：100ms 是「取消/超时响应延迟」与「空转开销」的折中
/// （单次分离是分钟级操作，不必像 punc 那样 50ms 紧盯）。
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// stderr 进日志与错误串前的字符上限（理由同 punc.rs：Python traceback 可以
/// 长到刷爆日志、把真正的原因埋掉；判定不受影响，截断只服务展示）。
const STDERR_LOG_MAX_CHARS: usize = 2000;

/// 有界等待的出口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    /// 子进程自行退出（退出码另行判断）。
    Exited,
    /// 用户取消（`cancel` 置位）。
    Cancelled,
    /// 超过 [`wait_budget`] 的预算仍未退出。
    TimedOut,
    /// `try_wait` 本身报错（句柄异常），不能死循环。
    WaitFailed,
}

/// 本次轮询是否该退出等待；`None` = 继续等。优先级：**取消 > 超时**
/// （用户主动终止是更强的语义，文案也该显示「已取消」）。抽成纯函数是为了让
/// 这条优先级能被单测直接钉住，不必真起子进程等满预算。
fn wait_outcome(cancelled: bool, past_deadline: bool) -> Option<WaitOutcome> {
    if cancelled {
        Some(WaitOutcome::Cancelled)
    } else if past_deadline {
        Some(WaitOutcome::TimedOut)
    } else {
        None
    }
}

/// 有界等待预算：`120s + 6s × 输入秒数`，封顶 [`MAX_WAIT_SECS`]。
///
/// 输入秒数非法（NaN/负数）按 0 处理——预算只该来自真实的媒体时长，坏输入
/// 不能把预算放大成无界，也不能 panic。
fn wait_budget(input_secs: f64) -> Duration {
    let secs = if input_secs.is_finite() && input_secs > 0.0 {
        input_secs
    } else {
        0.0
    };
    let total = (BASE_WAIT_SECS as f64 + WAIT_SECS_PER_INPUT_SEC * secs).min(MAX_WAIT_SECS as f64);
    Duration::from_secs_f64(total)
}

// ───────────────────────── provider 收敛（与 runner 同口径） ─────────────────────────

/// 把配置/命令行里的 provider 收敛到引擎内口径 `cpu` | `dml`。
///
/// 与 `punc.rs` / `sensevoice.rs` 构造器、`GpuConfig::resolve_onnx_provider`
/// 同一套归一；下发给 runner 后由 Python 侧再映射成 onnxruntime 认识的
/// `DmlExecutionProvider`（runner 的 `resolve_provider` 把 `dml` 收敛为
/// `directml`）。未知取值一律 `cpu`，绝不猜。
fn resolve_provider(requested: &str) -> &'static str {
    match requested.trim().to_ascii_lowercase().as_str() {
        "dml" | "directml" => "dml",
        _ => "cpu",
    }
}

/// 两个 provider 取值是否指同一个后端（`dml` ≡ `directml`）。
///
/// runner 回报的 JSON 用 `cpu|directml`，引擎内存储是 `cpu|dml`——直接字符串
/// 相等会把「真吃到 DirectML」误报成回落。抽成纯函数便于单测同口径。
fn same_provider(a: &str, b: &str) -> bool {
    resolve_provider(a) == resolve_provider(b)
}

// ───────────────────────── 输出解析 ─────────────────────────

/// runner stdout 末行 JSON 的契约形态（`tools/vocal_sep_runner.py` 的 `emit_result`）。
#[derive(serde::Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum RunnerReport {
    Ok {
        input_sec: f64,
        /// 只用于协议完整性校验（缺字段 = 协议不完整），本引擎不消费它——
        /// 输出路径由调用方自己掌握。
        #[allow(dead_code)]
        output: String,
        provider: String,
        elapsed_sec: f64,
    },
    Error {
        error: String,
    },
}

/// 解析 runner stdout **末行**的结果 JSON 成 [`VocalSepReport`]。
///
/// 末行才是契约（runner 的进度/调试噪音可能在前面）；`status:"error"`、
/// 缺字段、坏 JSON、非法数值都要给出点名到字段的可操作报错，绝不返回半截结果。
fn parse_report(stdout: &str) -> Result<VocalSepReport> {
    let line = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .unwrap_or("");
    if line.is_empty() {
        anyhow::bail!(
            "人声分离输出为空：runner 未在 stdout 留下结果 JSON（末行应为 {{\"status\":\"ok\",...}}）"
        );
    }
    // serde 的缺字段/坏 JSON 详情必须拼进**顶层**消息：anyhow 的 `to_string()` 只显示
    // 最外层 context，把原因塞进 cause 链会让「missing field `input_sec`」这类点名信息
    // 在 UI / 日志里完全看不见（错误必须可操作）。
    let parsed: RunnerReport = serde_json::from_str(line).map_err(|err| {
        anyhow::anyhow!(
            "解析人声分离输出 JSON 失败（末行：{}）：{err}",
            truncate_stderr(line, 200)
        )
    })?;
    match parsed {
        RunnerReport::Error { error } => anyhow::bail!("人声分离 runner 报错：{error}"),
        RunnerReport::Ok {
            input_sec,
            output: _,
            provider,
            elapsed_sec,
        } => {
            let provider = provider.trim().to_string();
            if provider.is_empty() {
                anyhow::bail!("人声分离输出 JSON 的 provider 字段为空：无法判定实际生效的执行后端");
            }
            if !input_sec.is_finite() || input_sec < 0.0 {
                anyhow::bail!("人声分离输出 JSON 的 input_sec 非法（{input_sec}）：应为非负秒数");
            }
            if !elapsed_sec.is_finite() || elapsed_sec < 0.0 {
                anyhow::bail!(
                    "人声分离输出 JSON 的 elapsed_sec 非法（{elapsed_sec}）：应为非负秒数"
                );
            }
            Ok(VocalSepReport {
                input_sec,
                elapsed_sec,
                provider,
            })
        }
    }
}

/// 截断过长的 stderr：保留前 `max` 个字符（按字符切，不切坏 UTF-8）并注明省略量。
fn truncate_stderr(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…（stderr 过长，已省略 {} 字符）", total - max)
}

/// 起一个后台线程把子进程管道抽干，返回线程句柄；`None` = 该管道没有被 pipe。
///
/// 管道缓冲区只有几十 KB（Windows 实测 64KB）：没人读的话 runner 的 stderr
/// （进度/回落告警）与 stdout（结果 JSON）写满就阻塞在 write 上，而父进程在等它
/// 退出——双方互等，有界等待只能以「超时误杀一个正常 runner」收场。stdout 与
/// stderr **各自**独立抽干，互不阻塞（见 punc.rs 同款注释）。
fn drain_pipe<P: std::io::Read + Send + 'static>(
    pipe: Option<P>,
) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    pipe.map(|mut p| {
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            buf
        })
    })
}

/// 收尾抽干线程：读线程 panic / 未启动都按空字节处理，不把诊断通道的问题升级成主流程失败。
fn join_pipe(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

// ───────────────────────── 引擎 ─────────────────────────

/// 人声分离引擎：进程契约的封装，DSP/推理在 Python runner 里。
#[derive(Debug, Clone)]
pub struct VocalSepEngine {
    /// 运行 runner 的 Python 解释器（内置便携运行时优先，见 [`AppConfig::resolve_command`]）。
    pub python_path: PathBuf,
    /// runner 脚本路径（`tools/vocal_sep_runner.py`）。
    pub runner_path: PathBuf,
    /// UVR MDX-Net 类 ONNX 模型路径（如 `models/vocal_sep/UVR-MDX-NET-Inst_HQ_3.onnx`）。
    pub model_path: PathBuf,
    /// ONNX 执行后端，引擎内口径：`cpu` | `dml`。
    pub provider: String,
}

/// 一次分离的回执（供管线写性能报告与回落告警）。
#[derive(Debug, Clone, PartialEq)]
pub struct VocalSepReport {
    /// 输入音频时长（秒）。
    pub input_sec: f64,
    /// runner 实际耗时（秒，含模型加载）。
    pub elapsed_sec: f64,
    /// 实际生效的 provider（`cpu` | `directml`，回落时如实是 `cpu`）。
    pub provider: String,
}

impl VocalSepEngine {
    /// 构造引擎：解释器与 runner 路径按仓库统一口径解析。
    ///
    /// 解释器必须走 [`AppConfig::resolve_command`]（内置便携 Python 优先 → PATH
    /// 兜底），不能用 `resolve_path("tools/python.exe")`——内置运行时的真实路径是
    /// `tools/python/python.exe`，写死错路径会让分离永远 spawn 失败（见 tts.rs 同款注释）。
    pub fn new(provider: String, model_path: PathBuf) -> Self {
        Self {
            python_path: AppConfig::resolve_command("python"),
            runner_path: AppConfig::resolve_path("tools/vocal_sep_runner.py"),
            model_path,
            provider: resolve_provider(&provider).to_string(),
        }
    }

    /// runner 与模型是否都在位（管线据此决定是否启用前置分离）。
    pub fn is_available(&self) -> bool {
        self.runner_path.is_file() && self.model_path.is_file()
    }

    /// 跑一次人声分离：`input`（44100Hz pcm_s16le WAV）→ `output`（人声 WAV）。
    ///
    /// `cancel` 置位时立刻杀掉子进程并以「已取消」收尾；等待预算见 [`wait_budget`]。
    /// stdout/stderr 各自独立抽干，杜绝 64KB 管道互等（见 [`drain_pipe`]）。
    pub fn separate(
        &self,
        input: &Path,
        output: &Path,
        input_secs: f64,
        cancel: &Arc<AtomicBool>,
    ) -> Result<VocalSepReport> {
        if !self.is_available() {
            // 缺模型/脚本 → **报错**，不假装成功：静音降级会让「没做分离」的识别
            // 精度问题被误归因成模型不好。
            warn!(
                model = %self.model_path.display(),
                runner = %self.runner_path.display(),
                "人声分离模型或脚本未就绪"
            );
            anyhow::bail!(
                "人声分离未就绪：模型（{}）或 runner（{}）缺失。请在「模型管理」下载 UVR-MDX-NET 人声分离模型，\
                 或关闭「人声分离前置」后重试。",
                self.model_path.display(),
                self.runner_path.display()
            );
        }

        let mut cmd = Command::new(&self.python_path);
        // Python 环境清洗（剥 PYTHONHOME/PYTHONPATH、固定 UTF-8）与 punc / sensevoice /
        // tts 同一口径：用户机器上残留的 PYTHONHOME 会让任何 Python 子进程报
        // 「No module named encodings」。
        super::media_pipeline::apply_python_env(&mut cmd);
        cmd.arg(&self.runner_path)
            .arg("--input")
            .arg(input)
            .arg("--output")
            .arg(output)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--provider")
            .arg(&self.provider)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 让路模式：人声分离是重 CPU/GPU 子进程，同样要跟随用户的全局开关。
        super::media_pipeline::apply_default_child_flags(&mut cmd);

        // 与 punc / sensevoice 同因同治：找不到解释器时把「缺 Python」讲明白，
        // 而不是把 OS 的「系统找不到指定的文件」原样丢给用户。
        let program = cmd.get_program().to_os_string();
        let python_hint = (std::path::Path::new(&program) == self.python_path.as_path())
            .then_some(self.python_path.as_path());
        let child = cmd.spawn().map_err(|err| {
            super::spawn_failure_message(&err, &program, " 人声分离进程", python_hint)
        })?;
        // 并入全局作业对象：本进程一旦消失，runner 被系统一并清理，不留孤儿进程。
        crate::utils::child_registry::adopt(&child);

        // ChildReaper 兜底回收：从这里到 wait() 之间的任何提前返回（下面的
        // bail! 分支）都不得留下仍在跑的 runner。
        let mut reaper = crate::utils::child_registry::ChildReaper::new(child);
        // 抽干必须先于等待启动：见 `drain_pipe` 注释的管道互等陷阱。
        let stdout_reader = drain_pipe(reaper.child_mut().stdout.take());
        let stderr_reader = drain_pipe(reaper.child_mut().stderr.take());

        // 有界等待：正常退出 / 取消 / 超时三个出口，出口之后一律 kill + wait。
        // 旧式 `wait_with_output()` 在 runner 卡死（onnxruntime 挂住）时永不返回，
        // 界面会永远停在「人声分离中」，唯一出口是关应用。
        let budget = wait_budget(input_secs);
        let deadline = Instant::now() + budget;
        let (outcome, status) = loop {
            match reaper.child_mut().try_wait() {
                Ok(Some(status)) => break (WaitOutcome::Exited, Some(status)),
                Ok(None) => {}
                Err(err) => {
                    // 等待本身出错（句柄异常）时不能死循环：按失败处理，由下面的
                    // kill+wait 统一回收。
                    warn!(error = %err, "等待人声分离子进程失败，终止该进程");
                    break (WaitOutcome::WaitFailed, None);
                }
            }
            // 判定抽成纯函数：「取消优先于超时」的语义由单测钉住。
            if let Some(outcome) =
                wait_outcome(cancel.load(Ordering::SeqCst), Instant::now() >= deadline)
            {
                break (outcome, None);
            }
            std::thread::sleep(WAIT_POLL_INTERVAL);
        };

        if !matches!(outcome, WaitOutcome::Exited) {
            // kill 之后必须 wait，否则句柄不释放；已退出的进程由 try_wait 收过尸，
            // 这里拿到的是缓存状态，不会二次阻塞。
            let _ = reaper.child_mut().kill();
        }
        let _ = reaper.child_mut().wait();
        // 进程已退出 ⇒ 管道关闭 ⇒ 抽干线程立刻结束：超时强杀也不会卡在这里。
        let stdout_bytes = join_pipe(stdout_reader);
        let stderr_bytes = join_pipe(stderr_reader);
        let stderr_raw = String::from_utf8_lossy(&stderr_bytes);

        match outcome {
            WaitOutcome::Cancelled => {
                info!("人声分离已取消");
                anyhow::bail!("人声分离已取消");
            }
            WaitOutcome::TimedOut => {
                // 带上 runner 最后的 stderr（截断展示）：排查时能看出它卡在哪一步。
                warn!(
                    budget_secs = budget.as_secs(),
                    stderr = %truncate_stderr(&stderr_raw, STDERR_LOG_MAX_CHARS),
                    "人声分离超过等待预算仍未退出，已强杀子进程"
                );
                anyhow::bail!(
                    "人声分离超时（等待预算 {}s = 120s + 6s × 输入时长 {:.1}s，封顶 {}s），已终止子进程。\
                     可稍后重试；持续超时说明机器跑不动该模型，请换更快的执行后端（provider=dml）或更短的素材。",
                    budget.as_secs(),
                    input_secs,
                    MAX_WAIT_SECS
                );
            }
            WaitOutcome::WaitFailed => anyhow::bail!("等待人声分离子进程退出失败"),
            WaitOutcome::Exited => {}
        }

        let status = status.expect("Exited 出口必然带退出码");
        if !status.success() {
            // runner 失败会把 traceback / 可操作报错写 stderr：原样带给调用方，
            // 而不是一句干巴巴的「进程返回非零」。
            let stderr = truncate_stderr(&stderr_raw, STDERR_LOG_MAX_CHARS);
            warn!("人声分离执行报错: {}", stderr);
            anyhow::bail!("人声分离执行失败: {}", stderr);
        }

        let report = parse_report(&String::from_utf8_lossy(&stdout_bytes))?;
        // 回落可见化：请求的 provider 与 runner 回报的实际生效值不一致 ⇒ 一定
        // 是「静默回落」（onnxruntime 未编译进 DirectML、驱动/DLL 异常后重试 CPU），
        // 必须留下可操作告警，而不是让用户以为 GPU 加速在跑。
        if !same_provider(&self.provider, &report.provider) {
            warn!(
                requested = %self.provider,
                actual = %report.provider,
                "人声分离 provider 回落：请求 `{}` 实际 `{}`。这通常是 Python 环境里的 onnxruntime 未编译进 \
                 DirectML。修复：`pip install onnxruntime-directml==1.23.0`（或用 scripts/build_python_runtime.ps1 \
                 重建内置运行时）；若暂不升级，请把 provider 切回 `cpu` 以消除误导。",
                self.provider,
                report.provider
            );
        }
        info!(
            input_sec = report.input_sec,
            elapsed_sec = report.elapsed_sec,
            provider = %report.provider,
            "人声分离完成"
        );
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 报告解析（好 / 坏 / 缺字段 / 错误态） ----

    #[test]
    fn parse_report_reads_ok_payload() {
        let report = parse_report(
            r#"{"status":"ok","input_sec":12.5,"output":"C:/tmp/人声.wav","provider":"directml","elapsed_sec":3.25}"#,
        )
        .expect("合法 ok 报文必须解析成功");
        assert!((report.input_sec - 12.5).abs() < 1e-9);
        assert!((report.elapsed_sec - 3.25).abs() < 1e-9);
        assert_eq!(report.provider, "directml");
    }

    #[test]
    fn parse_report_takes_last_non_empty_line() {
        // 契约是「stdout **末行** JSON」：前面允许有进度/调试噪音，行尾允许多余空行。
        let stdout = "{\"event\":\"progress\"}\n{\"status\":\"error\",\"error\":\"旧事件不该被解析\"}\n\
                      {\"status\":\"ok\",\"input_sec\":1.0,\"output\":\"o.wav\",\"provider\":\"cpu\",\"elapsed_sec\":2.0}\n\n";
        let report = parse_report(stdout).expect("必须取末行而不是中间的事件行");
        assert!((report.elapsed_sec - 2.0).abs() < 1e-9);
        assert_eq!(report.provider, "cpu");
    }

    #[test]
    fn parse_report_rejects_missing_fields() {
        // 缺 input_sec：报错要点名到字段，而不是一句「解析失败」。
        let err =
            parse_report(r#"{"status":"ok","output":"o.wav","provider":"cpu","elapsed_sec":1.0}"#)
                .expect_err("缺字段必须报错");
        assert!(err.to_string().contains("input_sec"), "{err}");
        // 缺 provider 同理。
        let err =
            parse_report(r#"{"status":"ok","input_sec":1.0,"output":"o.wav","elapsed_sec":1.0}"#)
                .expect_err("缺字段必须报错");
        assert!(err.to_string().contains("provider"), "{err}");
    }

    #[test]
    fn parse_report_rejects_bad_json_and_empty_stdout() {
        let err = parse_report("完全不是 JSON").expect_err("坏 JSON 必须报错");
        assert!(
            err.to_string().contains("解析人声分离输出 JSON 失败"),
            "{err}"
        );
        let err = parse_report("   \n  ").expect_err("空 stdout 必须报错");
        assert!(err.to_string().contains("输出为空"), "{err}");
    }

    #[test]
    fn parse_report_surfaces_runner_error_status() {
        // runner 自己报错时，message 必须原样带给上层（可操作性来自 runner 文案）。
        let err =
            parse_report(r#"{"status":"error","error":"输入 WAV 采样率是 16000Hz，请重新抽取"}"#)
                .expect_err("status=error 必须转成 Err");
        assert!(err.to_string().contains("16000Hz"), "{err}");
    }

    #[test]
    fn parse_report_rejects_illegal_numbers_and_blank_provider() {
        let err = parse_report(
            r#"{"status":"ok","input_sec":-1.0,"output":"o.wav","provider":"cpu","elapsed_sec":1.0}"#,
        )
        .expect_err("负时长必须报错");
        assert!(err.to_string().contains("input_sec"), "{err}");
        let err = parse_report(
            r#"{"status":"ok","input_sec":1.0,"output":"o.wav","provider":"  ","elapsed_sec":1.0}"#,
        )
        .expect_err("空 provider 必须报错");
        assert!(err.to_string().contains("provider"), "{err}");
    }

    // ---- 等待预算（输入时长缩放 + 封顶） ----

    #[test]
    fn wait_budget_scales_with_input_and_caps() {
        assert_eq!(wait_budget(0.0), Duration::from_secs(120), "固定开销 120s");
        assert_eq!(wait_budget(10.0), Duration::from_secs(180), "6s × 10s 输入");
        assert_eq!(
            wait_budget(600.0),
            Duration::from_secs(3720),
            "10 分钟输入 = 120 + 6×600"
        );
        assert_eq!(
            wait_budget(3600.0),
            Duration::from_secs(MAX_WAIT_SECS),
            "1 小时输入的原始预算 21720s 超过封顶，必须按封顶 14400s 返回"
        );
        assert_eq!(
            wait_budget(1.0e12),
            Duration::from_secs(MAX_WAIT_SECS),
            "超长输入必须封顶，不能无界"
        );
        assert_eq!(
            wait_budget(-5.0),
            Duration::from_secs(120),
            "坏输入按 0 处理"
        );
        assert_eq!(
            wait_budget(f64::NAN),
            Duration::from_secs(120),
            "NaN 输入按 0 处理，不 panic"
        );
        assert!(wait_budget(1.0) > Duration::ZERO);
    }

    // ---- provider 收敛（与 runner 同口径） ----

    #[test]
    fn provider_resolution_matches_runner_contract() {
        assert_eq!(resolve_provider("dml"), "dml");
        assert_eq!(resolve_provider("DML "), "dml", "大小写与空白归一");
        assert_eq!(
            resolve_provider("directml"),
            "dml",
            "directml 归一到引擎内口径 dml"
        );
        assert_eq!(resolve_provider("cpu"), "cpu");
        assert_eq!(resolve_provider(""), "cpu", "空串回 cpu");
        assert_eq!(resolve_provider("cuda"), "cpu", "未知取值不猜，回 cpu");
    }

    #[test]
    fn same_provider_treats_dml_and_directml_as_one() {
        assert!(same_provider("dml", "directml"), "dml ≡ directml");
        assert!(same_provider("DirectML", "dml"));
        assert!(same_provider("cpu", "cpu"));
        assert!(
            !same_provider("dml", "cpu"),
            "请求 dml 落到 cpu 是回落，不能判等"
        );
        assert!(!same_provider("cpu", "directml"));
    }

    // ---- 取消 / 超时判定 ----

    #[test]
    fn wait_outcome_prioritises_cancel_over_timeout() {
        assert_eq!(wait_outcome(false, false), None, "都没到点必须继续等");
        assert_eq!(wait_outcome(true, false), Some(WaitOutcome::Cancelled));
        assert_eq!(wait_outcome(false, true), Some(WaitOutcome::TimedOut));
        assert_eq!(
            wait_outcome(true, true),
            Some(WaitOutcome::Cancelled),
            "同时成立时按「已取消」上报"
        );
    }

    // ---- 可用性与「缺模型必须报错」 ----

    #[test]
    fn availability_tracks_runner_and_model() {
        let runner = AppConfig::resolve_path("tools/vocal_sep_runner.py");
        assert!(
            runner.is_file(),
            "runner 是仓库内脚本，必须随仓库存在：{runner:?}"
        );
        let missing = AppConfig::resolve_path("models/vocal_sep/definitely-missing.onnx");
        let engine = VocalSepEngine::new("cpu".to_string(), missing);
        assert!(!engine.is_available(), "模型缺失必须判不可用");

        // 造一个存在的占位模型文件：runner + model 都在 ⇒ 可用。
        let probe =
            std::env::temp_dir().join(format!("vocal_sep_probe_{}.onnx", std::process::id()));
        std::fs::write(&probe, b"probe").expect("写占位模型");
        let engine = VocalSepEngine::new("dml".to_string(), probe.clone());
        assert!(engine.is_available(), "runner 与 model 都在 ⇒ 可用");
        assert_eq!(
            engine.provider, "dml",
            "构造器应把 directml 收敛成引擎内口径 dml"
        );
        let _ = std::fs::remove_file(&probe);
    }

    #[test]
    fn separate_reports_missing_model_actionably() {
        let engine = VocalSepEngine::new(
            "cpu".to_string(),
            AppConfig::resolve_path("models/vocal_sep/definitely-missing.onnx"),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let err = engine
            .separate(Path::new("in.wav"), Path::new("out.wav"), 1.0, &cancel)
            .expect_err("缺模型必须报错，不许静默成功");
        let msg = err.to_string();
        assert!(msg.contains("人声分离未就绪"), "{msg}");
        assert!(msg.contains("模型管理"), "错误要给出修复入口：{msg}");
    }

    #[test]
    fn cancel_flag_is_observed_by_reference() {
        // separate() 的取消入口是外部传入的 `Arc<AtomicBool>`：置位后必须能被观察到
        // （进程契约的其余部分由集成测试覆盖）。
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(!cancel.load(Ordering::SeqCst));
        cancel.store(true, Ordering::SeqCst);
        assert!(cancel.load(Ordering::SeqCst));
    }
}
