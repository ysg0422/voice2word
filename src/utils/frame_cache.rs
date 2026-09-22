//! 视频帧智能缓存系统 — 解决时间轴拖动卡顿问题
//!
//! 缓存键 = 完整视频路径 + 文件签名(大小+mtime) + 量化时间(0.5s 精度)：
//! - 完整路径而非仅文件名，避免不同目录同名视频互相串缓存；
//! - 文件签名使同路径被覆盖为新视频后旧帧自动失效；
//! - 哈希采用跨进程稳定的 FNV-1a，保证重启后磁盘帧文件仍可命中复用。

use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::engines::FFmpegEngine;

pub struct FrameCache {
    cache: Arc<Mutex<FrameCacheInner>>,
    max_size: usize,
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
            self.cache.lock().unwrap().insert(key.clone(), out_jpg.clone(), self.max_size);
            return Ok(out_jpg);
        }

        ffmpeg.extract_frame(video_path, time_sec, &out_jpg)?;

        // 限制缓存大小，FIFO 淘汰
        self.cache.lock().unwrap().insert(key, out_jpg.clone(), self.max_size);

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
}
