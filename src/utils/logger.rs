//! 日志管理工具 — 同时输出到控制台与本地 txt 文件，捕获完整 panic 堆栈

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::info;

#[derive(Clone)]
pub struct DualWriter {
    file: Arc<Mutex<File>>,
    latest_file: Arc<Mutex<File>>,
}

impl io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stdout().write_all(buf);
        let _ = io::stdout().flush();

        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(buf);
            let _ = f.flush();
        }

        if let Ok(mut f) = self.latest_file.lock() {
            let _ = f.write_all(buf);
            let _ = f.flush();
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stdout().flush();
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
        if let Ok(mut f) = self.latest_file.lock() {
            let _ = f.flush();
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DualWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 保留的历史日志文件数量上限（不含 `latest.txt`）。
///
/// 每次启动都会新建一个 `voice2word_<时间戳>.txt` 且只追加、从不轮转，
/// 因此若不清理，`logs/` 会随启动次数线性增长（当前已有 100+ 个文件）。
/// 保留最近 20 次启动的日志，既够排查近期问题，又不会无限膨胀。
const KEEP_LOG_FILES: usize = 20;

/// 清理 `logs/` 下过期的历史日志：按修改时间只保留最近 `keep` 个。
///
/// 只处理 `voice2word_*.txt` 这种自动生成的名字，`latest.txt` 与用户自己
/// 放进来的文件一律不动。删除失败静默忽略——清理失败不该阻断启动。
fn prune_old_logs(logs_dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("voice2word_") && name.ends_with(".txt")) {
                return None;
            }
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, e.path()))
        })
        .collect();

    if logs.len() <= keep {
        return;
    }
    // 新的排在前面，跳过头 keep 个后剩下的都是最旧的
    logs.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, path) in logs.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

/// 初始化 txt 文件与控制台双写日志
pub fn init_logger() -> anyhow::Result<PathBuf> {
    let logs_dir = crate::utils::AppConfig::app_root_dir().join("logs");
    fs::create_dir_all(&logs_dir)?;
    prune_old_logs(&logs_dir, KEEP_LOG_FILES);

    let now = chrono::Local::now();
    let file_name = format!("voice2word_{}.txt", now.format("%Y%m%d_%H%M%S"));
    let log_path = logs_dir.join(file_name);
    let latest_path = logs_dir.join("latest.txt");

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // latest.txt 每次启动重新创建
    let latest_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&latest_path)?;

    let writer = DualWriter {
        file: Arc::new(Mutex::new(file)),
        latest_file: Arc::new(Mutex::new(latest_file)),
    };

    let writer_for_subscriber = writer.clone();

    // 初始化 tracing subscriber
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,voice2word=debug".into()),
        )
        .with_ansi(false) // txt 文件保持纯文本，无终端转义色块
        .with_writer(writer_for_subscriber)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| anyhow::anyhow!("设置日志订阅者失败: {}", e))?;

    // 注册全局 panic 捕获钩子，确保发生异常时将详细堆栈无遗漏写入 txt 文件
    let panic_log_path = log_path.clone();
    let panic_latest_path = latest_path.clone();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::capture();
        let panic_msg = format!(
            "\n\n==================== [CRITICAL PANIC DETECTED] ====================\n\
            时间: {}\n\
            详情: {}\n\
            堆栈:\n{}\n\
            ===================================================================\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            info,
            backtrace
        );

        eprintln!("{}", panic_msg);

        // 追加写入到当前日志与 latest.txt
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log_path)
        {
            let _ = f.write_all(panic_msg.as_bytes());
            let _ = f.flush();
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_latest_path)
        {
            let _ = f.write_all(panic_msg.as_bytes());
            let _ = f.flush();
        }

        prev_hook(info);
    }));

    info!("==========================================");
    info!("Voice2Word 文本日志系统初始化成功");
    info!("实时日志文件: {:?}", log_path);
    info!("最新日志快捷查看: {:?}", latest_path);
    info!("==========================================");

    Ok(log_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 日志轮转只保留最近 keep 个 `voice2word_*.txt`，且绝不碰 latest.txt 与其它文件。
    #[test]
    fn prune_old_logs_keeps_newest_and_spares_latest() {
        let dir = std::env::temp_dir().join(format!("v2w_logs_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        for i in 0..4 {
            fs::write(
                dir.join(format!("voice2word_2026010{i}_000000.txt")),
                b"log",
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        fs::write(dir.join("latest.txt"), b"latest").unwrap();
        fs::write(dir.join("_warn_now.log"), b"agent scratch").unwrap();

        prune_old_logs(&dir, 2);

        let remaining: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            remaining
                .iter()
                .filter(|n| n.starts_with("voice2word_"))
                .count(),
            2
        );
        assert!(dir.join("latest.txt").exists(), "latest.txt 不能被删");
        assert!(
            dir.join("_warn_now.log").exists(),
            "非 voice2word_ 前缀的文件不能被删"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
