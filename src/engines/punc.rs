//! PunctuationEngine — 基于 CT-Transformer (ONNX) 的超轻量极速标点恢复引擎
//! 纯 CPU 单次推理毫秒级，比大模型快 50~100 倍

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;
use tracing::{info, warn};

use crate::subtitle::Segment;

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
    /// 让路模式：子进程降到 BELOW_NORMAL_PRIORITY_CLASS（此前只设了
    /// CREATE_NO_WINDOW，完全忽略用户的 `gpu.yield_to_desktop` 开关）。
    yield_to_desktop: bool,
}

impl PunctuationEngine {
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
    ) -> Self {
        Self::with_python(
            runner_script,
            model_path,
            threads,
            PathBuf::from("python"),
            // 默认与 config.toml 的 gpu.yield_to_desktop 默认值一致
            true,
        )
    }

    /// 指定 Python 解释器（用于 PATH 上有多个 Python、默认 `python` 缺依赖的场景）
    pub fn with_python<P1: AsRef<Path>, P2: AsRef<Path>, P3: AsRef<Path>>(
        runner_script: P1,
        model_path: P2,
        threads: u32,
        python_path: P3,
        yield_to_desktop: bool,
    ) -> Self {
        Self {
            runner_script: runner_script.as_ref().to_path_buf(),
            model_path: model_path.as_ref().to_path_buf(),
            threads,
            python_path: python_path.as_ref().to_path_buf(),
            yield_to_desktop,
        }
    }

    pub fn is_available(&self) -> bool {
        self.model_path.exists() && self.runner_script.exists()
    }

    pub fn add_punctuation(
        &self,
        mut segments: Vec<Segment>,
        progress_cb: Option<Box<dyn Fn(f64, &str) + Send>>,
    ) -> Result<Vec<Segment>> {
        if segments.is_empty() {
            return Ok(segments);
        }

        if !self.is_available() {
            warn!("CT-Punc 模型或脚本未就绪: {:?}, 跳过极速标点", self.model_path);
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

        let input_json = serde_json::to_vec(&items)
            .context("序列化字幕输入失败")?;

        let mut cmd = Command::new(&self.python_path);
        cmd.arg(&self.runner_script)
            .arg("--model")
            .arg(&self.model_path)
            .arg("--threads")
            .arg(self.threads.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 让路模式：标点恢复也是 Python 子进程，同样要跟随用户的开关
        super::media_pipeline::apply_child_flags(&mut cmd, self.yield_to_desktop);
        let child = cmd.spawn().context("启动 CT-Transformer 标点恢复子进程失败")?;
        crate::utils::child_registry::adopt(&child);
        // 从这里到 wait_with_output() 之间的任何提前退出都由守卫兜底回收子进程
        let mut reaper = ChildReaper::new(child);

        if let Some(mut stdin) = reaper.child_mut().stdin.take() {
            stdin.write_all(&input_json)?;
            stdin.flush()?;
        }

        if let Some(ref cb) = progress_cb {
            cb(0.5, "正在进行毫秒级标点预测与断句...");
        }

        let output = reaper
            .take()
            .wait_with_output()
            .context("等待标点子进程退出失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("CT-Punc 执行报错: {}", stderr);
            anyhow::bail!("CT-Punc 执行失败: {}", stderr);
        }

        #[derive(serde::Deserialize)]
        struct OutputResult {
            index: usize,
            polished: String,
        }

        #[derive(serde::Deserialize)]
        struct OutputPayload {
            #[allow(dead_code)]
            count: usize,
            #[allow(dead_code)]
            elapsed_sec: f64,
            results: Vec<OutputResult>,
        }

        let payload: OutputPayload = serde_json::from_slice(&output.stdout)
            .context("解析标点输出 JSON 失败")?;

        let map: HashMap<usize, String> = payload
            .results
            .into_iter()
            .map(|r| (r.index, r.polished))
            .collect();

        for seg in segments.iter_mut() {
            if let Some(pol) = map.get(&seg.index) {
                seg.polished = pol.clone();
            } else {
                seg.polished = seg.text.clone();
            }
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
    #[cfg(target_os = "windows")]
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

    /// `take()` 交出句柄后，守卫析构不得再干预——否则会与调用方的 `wait_with_output()`
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
}
