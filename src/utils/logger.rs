//! 日志管理工具 — 同时输出到控制台与本地 txt 文件，捕获完整 panic 堆栈

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
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

/// 初始化 txt 文件与控制台双写日志
pub fn init_logger() -> anyhow::Result<PathBuf> {
    let logs_dir = crate::utils::AppConfig::app_root_dir().join("logs");
    fs::create_dir_all(&logs_dir)?;

    let now = chrono::Local::now();
    let file_name = format!("voice2word_{}.txt", now.format("%Y%m%d_%H%M%S"));
    let log_path = logs_dir.join(file_name);
    let latest_path = logs_dir.join("latest.txt");

    let file = OpenOptions::new()
        .create(true)
        .write(true)
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
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&panic_log_path) {
            let _ = f.write_all(panic_msg.as_bytes());
            let _ = f.flush();
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&panic_latest_path) {
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
