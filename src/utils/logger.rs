//! 日志管理工具 — 同时输出到控制台与本地 txt 文件，捕获完整 panic 堆栈

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::info;

/// 双写器：一条日志同时进 stdout、本次启动的日志文件、以及 `latest.txt`。
///
/// # 写失败为什么不静默吞掉
///
/// 磁盘满、权限被撤、文件被别的程序锁住时写入会失败。这里有两件都不能做的事：
/// 一是把失败当成成功（用户以为「日志有记录」，实际文件是空的，反馈问题时零线索），
/// 二是让 `tracing` 每次写入都去报错（会变成「报错本身也要写日志」的循环）。
/// 折中做法：累计一个失败计数，并**只在第一次**失败时往 stderr 喊一句。
///
/// mutex 中毒同理：`.unwrap()` 会让此后的每次写日志都 panic，`.into_inner()`
/// 至少还留着已经写进文件的历史内容可以继续追加。
#[derive(Clone)]
pub struct DualWriter {
    file: Arc<Mutex<File>>,
    latest_file: Arc<Mutex<File>>,
}

/// 进程级的日志写失败计数（stdout 失败不算——GUI 进程下它本来就不存在）。
///
/// 放在全局而不是 `DualWriter` 的字段里：`DualWriter` 只被 `init_logger` 内部持有，
/// 外部拿不到实例；而「日志是不是静默失效了」正是排查启动问题时最需要知道的一件事，
/// 必须有一个进程级、任何地方都能读的入口（[`log_write_failures`]）。
static WRITE_FAILURES: AtomicUsize = AtomicUsize::new(0);
/// 是否已经就「写失败」向 stderr 提醒过（只喊一次，避免刷屏）。
static WRITE_WARNED: AtomicBool = AtomicBool::new(false);

/// 累计的日志写失败次数。> 0 意味着日志文件里可能缺内容（磁盘满 / 权限 / 被锁）。
pub fn log_write_failures() -> usize {
    WRITE_FAILURES.load(Ordering::Relaxed)
}

impl DualWriter {
    /// 把 `buf` 写进某个文件槽；失败只计数 + 首次提醒，不向上传播。
    fn write_to(&self, slot: &Mutex<File>, buf: &[u8]) {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = guard.write_all(buf).and_then(|()| guard.flush()) {
            note_write_failure(&e);
        }
    }
}

/// 记一次日志写失败；第一次失败时向 stderr 提醒，避免「日志静默失效」。
fn note_write_failure(error: &io::Error) {
    WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
    if !WRITE_WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("[voice2word] 日志写入失败（后续同类失败不再重复提示）：{error}");
    }
}

impl io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // stdout 写失败不影响文件日志：release 构建下 GUI 进程根本没有可用的
        // stdout/stderr，那不是「日志系统坏了」。
        let _ = io::stdout().write_all(buf);
        let _ = io::stdout().flush();

        self.write_to(&self.file, buf);
        self.write_to(&self.latest_file, buf);

        // 仍然按「写入了 buf.len() 字节」返回：写日志不是业务流程，不该让 tracing
        // 自己再走一遍错误处理。失败已经记进 `write_failure_count` 并喊过一声。
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stdout().flush();
        for slot in [&self.file, &self.latest_file] {
            let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            let _ = guard.flush();
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

/// 「最新一次启动」的快捷日志文件名（每次启动重建）。
pub(crate) const LATEST_LOG_NAME: &str = "latest.txt";

/// 是不是本项目日志系统产出的日志文件：`latest.txt` 或
/// `voice2word_<YYYYMMDD>_<HHMMSS>.txt`。
///
/// **必须按严格的时间戳形状匹配，不能写成 `voice2word_*.txt` 前缀匹配**：
/// `logs/` 是用户看得见、也可能自己往里放文件的目录，而轮转清理会**删除**匹配到的
/// 文件——前缀匹配会把用户放进去的 `voice2word_笔记.txt` 一起删掉（删除失败还被静默
/// 吞掉），那是不可接受的误删。崩溃自检也用这个判定挑要读的日志（见
/// `crash_report::detect_previous_crash`），两处共用同一份规则就不会漂移。
pub(crate) fn is_project_log_name(name: &str) -> bool {
    if name == LATEST_LOG_NAME {
        return true;
    }
    let Some(rest) = name.strip_prefix("voice2word_") else {
        return false;
    };
    let Some(stamp) = rest.strip_suffix(".txt") else {
        return false;
    };
    // `YYYYMMDD_HHMMSS`：长度固定，且除分隔符外每一位都是 ASCII 数字
    let b = stamp.as_bytes();
    b.len() == 15
        && b[8] == b'_'
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[9..].iter().all(u8::is_ascii_digit)
}

/// 是否可以由轮转删除：自动生成的按日期日志。
/// `latest.txt` 是**每次启动重建**的快捷入口，绝不能当成「最旧的一个」删掉。
fn is_prunable_log_name(name: &str) -> bool {
    name != LATEST_LOG_NAME && is_project_log_name(name)
}

/// 清理 `logs/` 下过期的历史日志：按修改时间只保留最近 `keep` 个。
///
/// 只处理严格按时间戳自动生成的名字（[`is_prunable_log_name`]），`latest.txt`
/// 与用户自己放进来的文件（哪怕名字带 `voice2word_` 前缀）一律不动。
/// 删除失败静默忽略——清理失败不该阻断启动。
fn prune_old_logs(logs_dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_prunable_log_name(&name) {
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
    let latest_path = logs_dir.join(LATEST_LOG_NAME);

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
    // 过滤器：默认 `info`，本 crate 放宽到 `debug`。
    //
    // # 为什么不能直接 `try_from_default_env().unwrap_or(...)`
    //
    // 那样写有个**实测踩到的**坑：环境里只要存在 `RUST_LOG`（本机就是 `RUST_LOG=warn`，
    // 由别的工具设的全局变量），它就会**整体接管**过滤规则，本程序自己的诊断日志
    // 随即变成空白——日志文件建出来了、0 字节，用户来反馈时什么线索都没有。
    // 一个与业务无关的环境变量不该能静默关掉程序的诊断能力。
    //
    // 修法：先解析环境变量（保留用户显式调级别 / 调第三方库的能力），再**追加**一条
    // 针对本 crate 的指令。`EnvFilter` 按「具体程度」排序指令，带 target 的指令比
    // 裸级别更具体，因此 `voice2word=debug` 会稳定压过 `warn`，而我们自己的日志
    // 永远留得住；`RUST_LOG=voice2word=error` 这种更精确的写法仍然说了算。
    let mut filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if let Ok(directive) = "voice2word=debug".parse() {
        filter = filter.add_directive(directive);
    }
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
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

    /// **反例**：名字只是恰好带 `voice2word_` 前缀的用户文件绝不能被轮转删掉。
    ///
    /// 前缀匹配的旧实现会在这里失败：`voice2word_笔记.txt` 会被当成「最旧的日志」
    /// 删掉，而删除失败还被静默吞掉——用户放在 `logs/` 里的东西就这么没了。
    #[test]
    fn prune_spares_user_file_that_only_shares_the_prefix() {
        let dir = std::env::temp_dir().join(format!("v2w_logs_user_{}", std::process::id()));
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
        // 用户自己放进来的文件：带同样的前缀，但不是我们生成的时间戳名字
        fs::write(dir.join("voice2word_笔记.txt"), "用户的东西".as_bytes()).unwrap();
        fs::write(
            dir.join("voice2word_backup_2026.txt"),
            "用户的东西".as_bytes(),
        )
        .unwrap();
        fs::write(dir.join(LATEST_LOG_NAME), b"latest").unwrap();

        prune_old_logs(&dir, 1);

        assert!(
            dir.join("voice2word_笔记.txt").exists(),
            "用户文件不能被当成历史日志删掉"
        );
        assert!(
            dir.join("voice2word_backup_2026.txt").exists(),
            "前缀相同但形状不对的名字同样不能删"
        );
        assert!(dir.join(LATEST_LOG_NAME).exists(), "latest.txt 不能被删");
        let kept = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| is_prunable_log_name(&e.file_name().to_string_lossy()))
            .count();
        assert_eq!(kept, 1, "自动生成的日志只保留最近 1 个");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 日志文件名判定的真值表：只认 `latest.txt` 与严格的时间戳形状。
    #[test]
    fn project_log_name_truth_table() {
        assert!(is_project_log_name(LATEST_LOG_NAME));
        assert!(is_project_log_name("voice2word_20261011_113744.txt"));
        for no in [
            "voice2word_笔记.txt",
            "voice2word_backup_2026.txt",
            "voice2word.txt",
            // 少一位 / 多一位数字都不是我们写出来的名字
            "voice2word_2026101_113744.txt",
            "voice2word_20261011_11374.txt",
            "voice2word_20261011_1137445.txt",
            // 非数字混进时间戳
            "voice2word_20261a11_113744.txt",
            "voice2word_20261011_11374a.txt",
            // 换了扩展名
            "voice2word_20261011_113744.json",
            "latest.log",
            "other.txt",
            "",
        ] {
            assert!(!is_project_log_name(no), "{no:?} 不能被当成我们的日志文件");
        }
        // 判定用的是「能不能被轮转删除」，latest.txt 必须被排除在外。
        assert!(!is_prunable_log_name(LATEST_LOG_NAME));
        assert!(is_prunable_log_name("voice2word_20261011_113744.txt"));
    }

    /// 日志写失败必须变成**可观测**的事实，不能静默当成成功。
    ///
    /// 用只读句柄当日志文件，`write_all` 必然失败（Windows 上写只读句柄返回拒绝访问）。
    /// 旧实现在这里什么都不记：用户以为日志有内容，实际文件从某一行起就断流了。
    #[test]
    fn write_failures_are_counted_instead_of_silently_swallowed() {
        let dir = std::env::temp_dir().join(format!("v2w_logs_fail_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("readonly.txt");
        fs::write(&path, b"seed").unwrap();

        // 只读句柄：读取没问题，写入必失败
        let readonly = File::open(&path).unwrap();
        let writer = DualWriter {
            file: Arc::new(Mutex::new(readonly)),
            latest_file: Arc::new(Mutex::new(File::open(&path).unwrap())),
        };

        let before = log_write_failures();
        let mut w = writer.clone();
        // 写入接口对 tracing 仍然「成功」返回：写日志不该让它再走一遍错误处理
        w.write_all(b"hello")
            .expect("写失败由计数暴露，不通过 Err 表达");
        assert!(
            log_write_failures() > before,
            "写失败必须被计数，否则「日志有内容」是假的"
        );
        // 失败不能靠 panic 表达
        w.flush().unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    /// mutex 中毒（持锁线程 panic）之后仍然要能把日志写进文件。
    ///
    /// 旧实现用 `if let Ok(mut f) = self.lock()`：一旦中毒，此后每次写日志都静默
    /// 变成 no-op——日志永久断流且毫无提示。
    #[test]
    fn keeps_writing_after_mutex_poisoning() {
        let dir = std::env::temp_dir().join(format!("v2w_logs_poison_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("poisoned.txt");
        let latest = dir.join("latest.txt");

        let slot = Arc::new(Mutex::new(File::create(&path).unwrap()));
        // 另起一条线程持锁 panic，把 mutex 毒化；join 必然返回 Err（panic 已捕获）。
        let poison = Arc::clone(&slot);
        let joined = std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("故意把日志 mutex 毒化");
        })
        .join();
        assert!(joined.is_err(), "构造场景：持锁线程确实 panic 了");

        let writer = DualWriter {
            file: Arc::clone(&slot),
            latest_file: Arc::new(Mutex::new(File::create(&latest).unwrap())),
        };
        let mut w = writer.clone();
        w.write_all(b"after-poison\n").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("after-poison"),
            "中毒之后必须继续写日志，不能静默失效"
        );
        let latest_content = fs::read_to_string(&latest).unwrap();
        assert!(latest_content.contains("after-poison"));

        let _ = fs::remove_dir_all(&dir);
    }
}
