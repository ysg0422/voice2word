//! 视频帧智能缓存系统 — 解决时间轴拖动卡顿问题
//!
//! 缓存键 = 完整视频路径 + 文件签名(大小+mtime) + 量化时间(0.5s 精度)：
//! - 完整路径而非仅文件名，避免不同目录同名视频互相串缓存；
//! - 文件签名使同路径被覆盖为新视频后旧帧自动失效；
//! - 哈希采用跨进程稳定的 FNV-1a，保证重启后磁盘帧文件仍可命中复用。

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::engines::FFmpegEngine;

/// 磁盘帧目录的保留文件数上限。
///
/// 内存缓存有 `max_size` 上限，但磁盘上的 `%TEMP%/v2w_frames` 原先只增不减：
/// 每拖动一次时间轴（0.5s 量化）就可能落一个新 JPG，视频库每张卡片也会落一张，
/// 长期使用后该目录会累积成千上万个文件。按「最近使用」保留最近 N 个即可。
const DISK_KEEP_FILES: usize = 400;

/// 每新增这么多帧才扫一次磁盘目录。
/// 扫描需要 `read_dir` + 每个文件的 metadata，不能每抽一帧都做一遍。
const PRUNE_EVERY: usize = 32;

/// 按修改时间保留磁盘帧目录里最近的 `keep` 个 JPG，其余删除。
///
/// 只删 `.jpg`，其它文件一概不动；任何一步失败都静默跳过——磁盘裁剪只是
/// 缓存维护，不该影响抽帧结果。
///
/// **只删真正的帧文件**：抽帧的中间产物形如 `<key>.<pid>-<seq>.part.jpg`，
/// 但 `Path::extension()` 对它是 `"jpg"`。若把它一起算进候选并按「最旧」删掉，
/// 就可能在另一个线程刚写完、正要做 rename 时被本函数删除，导致该次抽帧
/// 白跑一趟（rename 失败）。因此这里显式排除 `.part.jpg` 中间名。
fn prune_disk_dir(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jpg") {
                return None;
            }
            if is_partial_frame_name(&e.file_name().to_string_lossy()) {
                return None;
            }
            let modified = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, path))
        })
        .collect();

    if files.len() <= keep {
        return;
    }
    // 新的排在前面，跳过最近 keep 个后剩下的都是最旧的
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// 是否是抽帧中间产物（`<key>.<pid>-<seq>.part.jpg`）。
fn is_partial_frame_name(name: &str) -> bool {
    name.ends_with(".part.jpg")
}

pub struct FrameCache {
    cache: Arc<Mutex<FrameCacheInner>>,
    max_size: usize,
    /// 距上次磁盘裁剪新增的帧数，攒够 [`PRUNE_EVERY`] 才触发一次目录扫描。
    since_prune: AtomicUsize,
}

struct FrameCacheInner {
    map: HashMap<String, PathBuf>,
    order: VecDeque<String>,
}

impl FrameCacheInner {
    fn insert(&mut self, key: String, path: PathBuf, max_size: usize) {
        if self.map.contains_key(&key) {
            // 重复插入的键移至队尾，避免 FIFO 误淘汰仍活跃的条目
            self.order.retain(|k| *k != key);
        }
        self.map.insert(key.clone(), path);
        self.order.push_back(key);

        while self.map.len() > max_size {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// FNV-1a 64 位哈希：跨进程稳定，重启后磁盘缓存键不变
fn stable_hash(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

impl FrameCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(FrameCacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            })),
            max_size: max_size.max(1),
            since_prune: AtomicUsize::new(0),
        }
    }

    /// 记一次新增帧，攒够阈值就裁剪一次磁盘目录。
    fn note_disk_write(&self, disk_dir: &Path) {
        if self.since_prune.fetch_add(1, Ordering::Relaxed) + 1 >= PRUNE_EVERY {
            self.since_prune.store(0, Ordering::Relaxed);
            prune_disk_dir(disk_dir, DISK_KEEP_FILES);
        }
    }

    /// 生成缓存键：完整视频路径 + 文件签名(大小+mtime) + 量化时间（0.5s 精度）
    fn cache_key(video_path: &Path, time_sec: f64) -> String {
        let mut sig_parts: Vec<Vec<u8>> = vec![video_path.to_string_lossy().as_bytes().to_vec()];
        if let Ok(meta) = std::fs::metadata(video_path) {
            sig_parts.push(meta.len().to_le_bytes().to_vec());
            if let Ok(modified) = meta.modified() {
                if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                    sig_parts.push(d.as_secs().to_le_bytes().to_vec());
                }
            }
        }
        let refs: Vec<&[u8]> = sig_parts.iter().map(|v| v.as_slice()).collect();
        let path_sig = stable_hash(&refs);
        let quantized = (time_sec * 2.0).round() as i64;
        format!("{:016x}_{}", path_sig, quantized)
    }

    fn disk_dir() -> PathBuf {
        std::env::temp_dir().join("v2w_frames")
    }

    /// 获取缓存帧，如果不存在则立即提取
    pub fn get_or_extract(
        &self,
        video_path: &Path,
        time_sec: f64,
        ffmpeg: &FFmpegEngine,
    ) -> Result<PathBuf> {
        let key = Self::cache_key(video_path, time_sec);

        // 尝试从内存缓存获取
        if let Some(path) = self.cache.lock().unwrap().map.get(&key).cloned() {
            if path.exists() {
                return Ok(path);
            }
        }

        let temp_dir = Self::disk_dir();
        let _ = std::fs::create_dir_all(&temp_dir);

        // 磁盘文件名直接使用缓存键（含路径+签名哈希），同名视频互不覆盖
        let out_jpg = temp_dir.join(format!("{}.jpg", key));

        // 检查磁盘是否已存在抽取过的帧，存在则直接复用
        if out_jpg.exists() {
            self.cache
                .lock()
                .unwrap()
                .insert(key.clone(), out_jpg.clone(), self.max_size);
            return Ok(out_jpg);
        }

        // 抽帧先落到**本进程独占**的临时文件，成功后 rename 到最终名。
        //
        // 为什么不能直接 `extract_frame(..., &out_jpg)`：`ffmpeg` 是流式写盘的，
        // 中途失败/被取消会在最终名上留下**半截 JPG**。那个文件一旦存在，
        // 「磁盘已存在则复用」的分支下次就会把它当成可用帧返回，界面显示一张
        // 损坏图，且此后每次拖动时间轴都稳定复现。rename 在同目录内是原子的，
        // 因此最终名要么不存在，要么是一张完整可用的 JPG。
        let tmp_jpg = temp_dir.join(format!("{}.{}-{}.part.jpg", key, std::process::id(), {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        }));
        if let Err(err) = ffmpeg.extract_frame(video_path, time_sec, &tmp_jpg) {
            let _ = std::fs::remove_file(&tmp_jpg);
            return Err(err);
        }
        if let Err(err) = std::fs::rename(&tmp_jpg, &out_jpg) {
            let _ = std::fs::remove_file(&tmp_jpg);
            return Err(err).with_context(|| format!("转存抽帧结果失败: {}", out_jpg.display()));
        }

        // 限制缓存大小，FIFO 淘汰
        self.cache
            .lock()
            .unwrap()
            .insert(key, out_jpg.clone(), self.max_size);
        // 磁盘目录也要有上限，否则长期使用后 v2w_frames 会只增不减
        self.note_disk_write(&temp_dir);

        Ok(out_jpg)
    }

    /// 清理所有缓存（含磁盘帧目录）
    pub fn clear(&self) {
        {
            let mut inner = self.cache.lock().unwrap();
            inner.map.clear();
            inner.order.clear();
        }
        let _ = std::fs::remove_dir_all(Self::disk_dir());
    }

    /// 获取当前缓存大小
    pub fn size(&self) -> usize {
        self.cache.lock().unwrap().map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_for(path: &str, t: f64) -> String {
        FrameCache::cache_key(Path::new(path), t)
    }

    #[test]
    fn same_name_different_dir_has_different_keys() {
        let a = key_for(r"C:\videos\lesson.mp4", 12.3);
        let b = key_for(r"D:\backup\lesson.mp4", 12.3);
        assert_ne!(a, b, "同文件名不同目录不应共享缓存键");
    }

    #[test]
    fn key_is_stable_across_calls() {
        let a = key_for(r"C:\videos\lesson.mp4", 12.3);
        let b = key_for(r"C:\videos\lesson.mp4", 12.3);
        assert_eq!(a, b);
    }

    #[test]
    fn quantized_time_shares_entry() {
        assert_eq!(key_for(r"C:\v.mp4", 10.01), key_for(r"C:\v.mp4", 10.24));
        assert_ne!(key_for(r"C:\v.mp4", 10.01), key_for(r"C:\v.mp4", 10.51));
    }

    #[test]
    fn evicts_oldest_fifo() {
        let cache = FrameCache::new(2);
        let inner_path = |s: &str| PathBuf::from(s);
        {
            let mut inner = cache.cache.lock().unwrap();
            inner.insert("k1".into(), inner_path("p1"), cache.max_size);
            inner.insert("k2".into(), inner_path("p2"), cache.max_size);
            // k1 重复插入移至队尾：下次淘汰的应是 k2
            inner.insert("k1".into(), inner_path("p1"), cache.max_size);
            inner.insert("k3".into(), inner_path("p3"), cache.max_size);
            assert_eq!(inner.map.len(), 2);
            assert!(!inner.map.contains_key("k2"), "FIFO 应淘汰最早入队的 k2");
            assert!(inner.map.contains_key("k1"));
            assert!(inner.map.contains_key("k3"));
        }
    }

    /// 磁盘目录按数量裁剪：只留最近 keep 个 JPG，且不碰非 jpg 文件。
    #[test]
    fn prune_disk_dir_keeps_newest_jpgs_only() {
        let dir = std::env::temp_dir().join(format!("v2w_prune_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 逐个写入并让 mtime 递增，保证「最近」可判定
        for i in 0..5 {
            let p = dir.join(format!("f{i}.jpg"));
            std::fs::write(&p, b"x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        std::fs::write(dir.join("keep_me.txt"), b"not a frame").unwrap();

        prune_disk_dir(&dir, 2);

        let jpgs: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".jpg"))
            .collect();
        assert_eq!(jpgs.len(), 2, "只应保留最近 2 个 jpg: {jpgs:?}");
        assert!(jpgs.contains(&"f4.jpg".to_string()), "最新的一帧必须留下");
        assert!(dir.join("keep_me.txt").exists(), "非 jpg 文件不该被删");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 抽帧中间产物（`.part.jpg`）绝不能进裁剪候选：它可能在另一个线程
    /// 刚写完、正要做 rename，被删掉就白跑一趟。
    #[test]
    fn prune_disk_dir_never_deletes_in_flight_partials() {
        let dir = std::env::temp_dir().join(format!("v2w_prune_part_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 先放 3 个「旧」正式帧
        for i in 0..3 {
            std::fs::write(dir.join(format!("f{i}.jpg")), b"x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        // 再放一个中间产物（比正式帧更新，但即便更旧也不能被删）
        std::fs::write(dir.join("abc.123-0.part.jpg"), b"partial").unwrap();

        prune_disk_dir(&dir, 1);

        assert!(
            dir.join("abc.123-0.part.jpg").exists(),
            "进行中的中间产物不能被裁剪删除"
        );
        let formal = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".jpg") && !n.ends_with(".part.jpg"))
            .count();
        assert_eq!(formal, 1, "正式帧应只保留最近 1 个");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
