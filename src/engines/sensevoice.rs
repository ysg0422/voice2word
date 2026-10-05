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

use super::whisper::{take_live_pids, ChildEntry, ChildPidGuard};

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

#[derive(Clone)]
pub struct SenseVoiceEngine {
    runner_path: PathBuf,
    model_path: PathBuf,
    tokens_path: PathBuf,
    vad_model_path: PathBuf,
    threads: u32,
    /// 运行 runner 脚本的 Python 解释器（默认 "python"，可配置为绝对路径）
    python_path: PathBuf,
    cancel: Arc<AtomicBool>,
    active_children: Arc<Mutex<Vec<ChildEntry>>>,
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
    pub fn with_python<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>, P4: AsRef<Path>, P5: AsRef<Path>>(
        runner_path: P1,
        model_path: P2,
        tokens_path: P3,
        vad_model_path: P4,
        threads: u32,
        python_path: P5,
    ) -> Self {
        Self {
            runner_path: runner_path.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            tokens_path: tokens_path.as_ref().to_path_buf(),
            vad_model_path: vad_model_path.as_ref().to_path_buf(),
            threads: threads.max(1),
            python_path: python_path.as_ref().to_path_buf(),
            cancel: Arc::new(AtomicBool::new(false)),
            active_children: Arc::new(Mutex::new(Vec::new())),
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
    pub fn reset(&self) {
        self.cancel.store(false, Ordering::SeqCst);
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
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        let audio_path = audio_path.as_ref();
        info!(
            "SenseVoice 启动极速非自回归转写: {:?}, 语言: {:?}, 线程: {}",
            audio_path, language, threads.unwrap_or(self.threads)
        );

        let mut cmd = Command::new(&self.python_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }

        let th = threads.unwrap_or(self.threads);
        cmd.arg(&self.runner_path)
            .arg("--model").arg(&self.model_path)
            .arg("--tokens").arg(&self.tokens_path)
            .arg("--vad-model").arg(&self.vad_model_path)
            .arg("--input").arg(audio_path)
            .arg("--threads").arg(th.to_string())
            .arg("--language").arg(language.unwrap_or("auto"));

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
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
        info!("SenseVoice 启动纯内存管道非自回归推流转写 (0 磁盘 I/O)");

        let mut cmd = Command::new(&self.python_path);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000);
        }

        let th = threads.unwrap_or(self.threads);
        cmd.arg(&self.runner_path)
            .arg("--model").arg(&self.model_path)
            .arg("--tokens").arg(&self.tokens_path)
            .arg("--vad-model").arg(&self.vad_model_path)
            .arg("--input").arg("-")
            .arg("--threads").arg(th.to_string())
            .arg("--language").arg(language.unwrap_or("auto"));

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
        progress_cb: Option<Box<dyn Fn(f64, &str, Option<Segment>) + Send + Sync>>,
    ) -> Result<(Vec<Segment>, f64)> {
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

        let mut child = cmd.spawn().with_context(|| format!("启动 SenseVoice 进程失败: {:?}", self.runner_path))?;
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
            let stdin = child.stdin.take().context("获取 SenseVoice 标准输入管道失败")?;
            Some(std::thread::spawn(move || {
                use std::io::{copy, BufReader, BufWriter, Write};
                let mut reader = BufReader::with_capacity(128 * 1024, stream);
                let mut writer = BufWriter::with_capacity(128 * 1024, stdin);
                let _ = copy(&mut reader, &mut writer);
                let _ = writer.flush();
            }))
        } else {
            None
        };

        let stdout = child.stdout.take().context("获取 SenseVoice 标准输出管道失败")?;
        let stderr = child.stderr.take().context("获取 SenseVoice 标准错误管道失败")?;

        let stderr_handle = std::thread::spawn(move || {
            use std::io::Read;
            let mut err_str = String::new();
            let _ = std::io::BufReader::new(stderr).read_to_string(&mut err_str);
            err_str
        });

        let mut segments = Vec::new();
        let mut inference_elapsed = 0.0f64;

        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line_res in reader.lines() {
            let line = line_res.unwrap_or_default();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Ok(parsed) = serde_json::from_str::<SenseVoiceStreamLine>(trimmed) {
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
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
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
            let _ = h.join();
        }
        let stderr_str = stderr_handle.join().unwrap_or_default();

        // 用户主动终止：被杀进程的退出码不重要，按空结果返回，由管线走取消收尾
        if self.cancel.load(Ordering::SeqCst) {
            return Ok((Vec::new(), 0.0));
        }

        if !status.success() {
            warn!("SenseVoice 退出状态非 0: {:?}, 错误日志: {}", status.code(), stderr_str);
            anyhow::bail!("SenseVoice 识别失败: {}", stderr_str.trim());
        }

        // 智能优化时间轴：消除 100ms 重叠鬼影、消除时间重叠冲突、广播级短句延展平滑
        crate::subtitle::optimize_segments(&mut segments);

        if let Some(ref cb) = progress_cb {
            cb(1.0, &format!("SenseVoice 转写完成，共 {} 句", segments.len()), None);
        }

        info!(segments = segments.len(), elapsed = inference_elapsed, "SenseVoice 转写完成");
        Ok((segments, inference_elapsed))
    }
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
        cmd.args([
            "/C",
            &format!("echo spawned> \"{}\"", marker.display()),
        ])
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
}
