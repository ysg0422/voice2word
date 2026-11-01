//! 文件夹监听：把「新素材落盘」自动变成批量队列里的待转写任务。
//!
//! # 为什么用轮询而不是 OS 文件通知
//!
//! Windows 的 `ReadDirectoryChangesW` 要额外的 API 绑定、事件缓冲与句柄生命周期
//! 管理；而监听场景对延迟的容忍度本来就是秒级。3 秒一次的目录扫描在大目录上也只要
//! 几毫秒（`read_dir` + 扩展名/大小/时间比较），换来的是零依赖与可测试的纯函数。
//!
//! # 为什么要有「落定」缓冲
//!
//! 复制/下载中的文件在扫描瞬间可能是半截：一旦入队，FFmpeg 解码会直接失败，用户
//! 看到的是「监听加入了一个坏文件」。所以只接受**大小 > 0 且 mtime 早于 now-5s**
//! 的文件——代价是新文件最多晚 5 秒入队，对离线转写完全可接受。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 监听认可的音视频扩展名（与文件对话框、拖放共用同一份小写口径）。
pub const WATCH_MEDIA_EXTS: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg", "3gp", "mp3",
    "wav", "m4a", "flac", "aac", "ogg", "opus", "wma",
];

/// 是否是监听范围内的媒体文件（扩展名大小写不敏感）。
pub fn is_watchable_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| WATCH_MEDIA_EXTS.contains(&e.as_str()))
}

/// 文件是否「已经写完」：非空，且修改时间早于 `now - settle`。
///
/// 读不到元数据（刚被删除/无权限）一律判 `false`，下一轮扫描再看。
pub fn is_settled(path: &Path, now: SystemTime, settle: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    let Ok(modified) = meta.modified() else {
        return false;
    };
    match now.duration_since(modified) {
        Ok(age) => age >= settle,
        // 修改时间在「未来」（跨盘符拷贝/时钟回拨）时按已落定处理：
        // 拒绝它会让文件永远进不了队列。
        Err(_) => true,
    }
}

/// 扫一遍目录，返回**新发现的、已落定**的媒体文件，并把这些路径记入 `seen`。
///
/// - `seen` 是跨轮次去重集合（调用方持有）；已见过的路径不再返回；
/// - 目录不存在/不可读时返回空表（监听目录被删掉不该让 UI 报错）；
/// - `recursive` 为真时递归子目录。
pub fn scan_watch_dir(
    dir: &Path,
    recursive: bool,
    seen: &mut HashSet<PathBuf>,
    now: SystemTime,
    settle: Duration,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if recursive {
                    stack.push(path);
                }
                continue;
            }
            if !is_watchable_media(&path) {
                continue;
            }
            if seen.contains(&path) || !is_settled(&path, now, settle) {
                continue;
            }
            seen.insert(path.clone());
            found.push(path);
        }
    }
    // 目录遍历顺序取决于文件系统，排序让行为可复现（测试与日志都依赖它）。
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "v2w_watch_{tag}_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("创建测试目录");
        dir
    }

    fn touch(path: &Path, bytes: usize) {
        fs::write(path, vec![0u8; bytes]).expect("写测试文件");
    }

    #[test]
    fn media_detection_is_case_insensitive() {
        assert!(is_watchable_media(Path::new("a.MP4")));
        assert!(is_watchable_media(Path::new("a.mkv")));
        assert!(is_watchable_media(Path::new("a.flac")));
        assert!(!is_watchable_media(Path::new("a.srt")));
        assert!(!is_watchable_media(Path::new("README")));
    }

    #[test]
    fn zero_byte_and_fresh_files_are_not_settled() {
        let dir = unique_dir("settle");
        let empty = dir.join("empty.mp4");
        touch(&empty, 0);
        let fresh = dir.join("fresh.mp4");
        touch(&fresh, 16);
        // `now` 必须在**两个文件都写完之后**取：写文件会把 mtime 推到「现在」，
        // 若先取 now 再写，mtime 落在 now 之后，`duration_since` 会是 Err
        //（代码按「时钟回拨」当已落定处理），这条断言就会随机翻车。
        let now = SystemTime::now();

        assert!(!is_settled(&empty, now, Duration::ZERO), "空文件不算落定");
        assert!(
            !is_settled(&empty, now, Duration::from_secs(1)),
            "空文件即便时间够也不算落定"
        );
        assert!(
            !is_settled(&fresh, now, Duration::from_secs(3600)),
            "刚写的文件在落定窗口内不算完"
        );
        assert!(
            is_settled(&fresh, now, Duration::ZERO),
            "窗口为 0 时立即可用"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_reports_only_new_media_and_respects_recursion() {
        let dir = unique_dir("scan");
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        touch(&dir.join("top.mp4"), 8);
        touch(&dir.join("note.txt"), 8);
        touch(&sub.join("deep.mkv"), 8);

        let now = SystemTime::now() + Duration::from_secs(10);
        let mut seen = HashSet::new();

        let flat = scan_watch_dir(&dir, false, &mut seen, now, Duration::ZERO);
        assert_eq!(flat.len(), 1, "非递归只看一层: {flat:?}");
        assert!(flat[0].ends_with("top.mp4"));

        // 已见过的文件不会重复返回
        let again = scan_watch_dir(&dir, false, &mut seen, now, Duration::ZERO);
        assert!(again.is_empty(), "同一文件不应重复入队: {again:?}");

        let deep = scan_watch_dir(&dir, true, &mut seen, now, Duration::ZERO);
        assert_eq!(deep.len(), 1, "递归应发现子目录文件: {deep:?}");
        assert!(deep[0].ends_with("deep.mkv"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_directory_is_silent() {
        let mut seen = HashSet::new();
        let out = scan_watch_dir(
            Path::new("D:/definitely/not/here"),
            true,
            &mut seen,
            SystemTime::now(),
            Duration::ZERO,
        );
        assert!(out.is_empty());
    }
}
