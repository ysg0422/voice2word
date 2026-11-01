//! yt-dlp 链接下载：把网页视频落到本地，再交给转写管线。
//!
//! 与其它引擎同一套进程契约：双端抽干、有界等待、取消即 kill。这里额外做两件事：
//! 1. `--print after_move:filepath` 让 yt-dlp 把**最终文件路径**打出来，避免靠
//!    「到目录里找最新文件」猜（并发下载会猜错）；
//! 2. `--ffmpeg-location` 指向本项目 `tools` 目录——音视频分流（bv+ba）需要合并，
//!    而用户机器上的 PATH 不一定有 ffmpeg。

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::info;

use crate::utils::AppConfig;

/// 相对项目根的可执行体路径（与模型下载项 `yt-dlp` 的 dest 同源）。
pub const YTDLP_REL: &str = "tools/yt-dlp.exe";

/// 单次下载的等待预算：30 分钟（大文件 + 慢线路），超时即终止。
pub const YTDLP_WAIT_BUDGET: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct YtDlpEngine {
    exe: PathBuf,
    /// 取消位（`Arc` 共享：`Clone` 出去的句柄取消同一个任务）。
    cancel: Arc<AtomicBool>,
}

impl YtDlpEngine {
    pub fn new() -> Self {
        Self {
            exe: AppConfig::resolve_path(YTDLP_REL),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 组件是否就位（缺失时调用方应提示去「模型与组件」里一键下载）。
    pub fn is_available(&self) -> bool {
        self.exe.is_file()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn begin_task(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// 下载 `url` 到 `out_dir`，返回**最终文件路径**。
    pub fn download(&self, url: &str, out_dir: &Path) -> Result<PathBuf> {
        let url = url.trim();
        let authority = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .and_then(|rest| rest.split(['/', '?', '#']).next());
        if authority.is_none_or(str::is_empty) || url.chars().any(char::is_whitespace) {
            anyhow::bail!("只支持有效的 http(s) 链接: {url}");
        }
        if self.cancel.load(Ordering::SeqCst) {
            anyhow::bail!("链接下载已取消");
        }
        if !self.is_available() {
            anyhow::bail!(
                "缺少 yt-dlp（{}）：请在「模型与组件」里下载，或放到该路径",
                self.exe.display()
            );
        }
        std::fs::create_dir_all(out_dir)
            .with_context(|| format!("创建下载目录失败: {}", out_dir.display()))?;

        let tools_dir = self.exe.parent().unwrap_or(Path::new("."));
        let args = build_download_args(url, out_dir, tools_dir);
        info!(url, dir = %out_dir.display(), "yt-dlp 开始下载");

        let mut cmd = Command::new(&self.exe);
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("执行 yt-dlp 失败: {}", self.exe.display()))?;

        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = out_pipe.take() {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = err_pipe.take() {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });

        let deadline = Instant::now() + YTDLP_WAIT_BUDGET;
        let status = loop {
            if self.cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_reader.join();
                let _ = err_reader.join();
                anyhow::bail!("链接下载已取消");
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = out_reader.join();
                        let _ = err_reader.join();
                        anyhow::bail!(
                            "链接下载超时（预算 {} 分钟）：网络过慢或站点无响应，已终止",
                            YTDLP_WAIT_BUDGET.as_secs() / 60
                        );
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(err).context("等待 yt-dlp 退出失败");
                }
            }
        };

        let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        if !status.success() {
            anyhow::bail!("yt-dlp 下载失败: {}", tail_line(&stderr, &stdout));
        }
        let path = parse_final_path(&stdout).with_context(|| {
            format!("yt-dlp 未报告输出文件路径: {}", tail_line(&stderr, &stdout))
        })?;
        if !path.is_file() {
            anyhow::bail!("yt-dlp 报告的输出文件不存在: {}", path.display());
        }
        info!(file = %path.display(), "yt-dlp 下载完成");
        Ok(path)
    }
}

impl Default for YtDlpEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// 组装 yt-dlp 命令行参数（纯函数，便于单测）。
///
/// `-f "bv*+ba/b"`：优先「视频流 + 最佳音频」并合并，站点没有分流时回退最佳单文件；
/// `--merge-output-format mp4` 让合并结果统一成 mp4（后续管线按扩展名分派）；
/// `--no-part` 不留 `.part` 半截文件，失败时目录里不会堆垃圾。
pub fn build_download_args(url: &str, out_dir: &Path, tools_dir: &Path) -> Vec<OsString> {
    vec![
        "--no-playlist".into(),
        "--newline".into(),
        "--no-part".into(),
        "--no-mtime".into(),
        "-f".into(),
        "bv*+ba/b".into(),
        "--merge-output-format".into(),
        "mp4".into(),
        "--ffmpeg-location".into(),
        tools_dir.as_os_str().to_os_string(),
        "--print".into(),
        "after_move:filepath".into(),
        "-P".into(),
        out_dir.as_os_str().to_os_string(),
        "-o".into(),
        "%(title).120B [%(id)s].%(ext)s".into(),
        url.into(),
    ]
}

/// 从 yt-dlp 的 stdout 里取最终文件路径。
///
/// `--print after_move:filepath` 只打印路径本身；进度/警告行以 `[` 开头或被
/// `--newline` 拆成多行。取**最后一个非空且不是 `[...]` 行**的内容，兼容站点额外
/// 打印提示行的情况。
pub fn parse_final_path(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with('['))
        .map(PathBuf::from)
}

/// 错误提示用：优先最后一条 stderr，其次 stdout。
fn tail_line(stderr: &str, stdout: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .or_else(|| stdout.lines().map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("未知错误")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_include_merge_and_path_print() {
        let args = build_download_args(
            "https://example.com/v",
            Path::new("D:/out"),
            Path::new("D:/app/tools"),
        );
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = text.join(" ");
        assert!(joined.contains("-f bv*+ba/b"), "{joined}");
        assert!(joined.contains("--merge-output-format mp4"), "{joined}");
        assert!(joined.contains("--print after_move:filepath"), "{joined}");
        assert!(
            joined.contains("--ffmpeg-location D:/app/tools"),
            "{joined}"
        );
        assert!(joined.contains("-P D:/out"), "{joined}");
        assert!(text.last().unwrap() == "https://example.com/v");
    }

    #[test]
    fn final_path_takes_last_plain_line() {
        let stdout = "[youtube] Extracting URL\n[info] Downloading 1 format(s)\n\
                      /tmp/videos/标题 [abc123].mp4\n";
        assert_eq!(
            parse_final_path(stdout).as_deref(),
            Some(Path::new("/tmp/videos/标题 [abc123].mp4"))
        );
        assert!(parse_final_path("[only] warnings").is_none());
        assert!(parse_final_path("").is_none());
    }

    #[test]
    fn engine_reports_missing_binary() {
        // 构造一个指向不存在路径的引擎：这里直接验证「缺组件」的报错路径。
        let engine = YtDlpEngine {
            exe: PathBuf::from("D:/definitely/not/here/yt-dlp.exe"),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        assert!(!engine.is_available());
        let err = engine
            .download("https://example.com/v", Path::new("D:/tmp"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("缺少 yt-dlp"), "{err:#}");
    }

    #[test]
    fn download_error_keeps_stderr_even_when_stdout_has_progress() {
        assert_eq!(
            tail_line("ERROR: authentication failed\n", "[download] 25%\n"),
            "ERROR: authentication failed"
        );
        assert_eq!(tail_line(" \n", "fallback\n"), "fallback");
    }

    #[test]
    fn download_validates_url_and_cancel_before_starting_process() {
        let engine = YtDlpEngine {
            exe: PathBuf::from("missing-yt-dlp.exe"),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        for url in [
            "httpjunk",
            "http://",
            "https:///missing-host",
            "https://host with spaces/v",
        ] {
            let error = engine
                .download(url, Path::new("unused"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("有效"), "{url}: {error}");
        }
        engine.cancel();
        let error = engine
            .download("https://example.com/v", Path::new("unused"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("已取消"), "{error}");
    }

    #[test]
    fn reject_non_http_url() {
        let engine = YtDlpEngine::new();
        let err = engine
            .download("ftp://example.com/v", Path::new("D:/tmp"))
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("http") || text.contains("缺少"), "{text}");
    }
}
