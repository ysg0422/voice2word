//! 视频帧智能缓存系统 — 解决时间轴拖动卡顿问题

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::info;

use crate::engines::FFmpegEngine;

pub struct FrameCache {
    cache: Arc<Mutex<HashMap<String, PathBuf>>>,
    max_size: usize,
}

impl FrameCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            max_size,
        }
    }

    /// 生成缓存键：视频路径 + 量化时间（0.5s 精度）
    fn cache_key(video_path: &Path, time_sec: f64) -> String {
        let video_name = video_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("video");
        let quantized = (time_sec * 2.0).round() as i64;
        format!("{}_{}", video_name, quantized)
    }

    /// 获取缓存帧，如果不存在则立即提取
    pub fn get_or_extract(
        &self,
        video_path: &Path,
        time_sec: f64,
        ffmpeg: &FFmpegEngine,
    ) -> Result<PathBuf> {
        let key = Self::cache_key(video_path, time_sec);

        // 尝试从缓存获取
        if let Some(path) = self.cache.lock().unwrap().get(&key).cloned() {
            if path.exists() {
                return Ok(path);
            }
        }

        // 不存在则提取
        let temp_dir = std::env::temp_dir().join("v2w_frames");
        let _ = std::fs::create_dir_all(&temp_dir);

        let stem = video_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("vid");
        let sec_key = (time_sec * 2.0).round() as i64;
        let out_jpg = temp_dir.join(format!("{}_{}.jpg", stem, sec_key));

        ffmpeg.extract_frame(video_path, time_sec, &out_jpg)?;

        // 限制缓存大小，FIFO 淘汰
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= self.max_size {
            if let Some(oldest_key) = cache.keys().next().cloned() {
                cache.remove(&oldest_key);
            }
        }
        cache.insert(key, out_jpg.clone());

        Ok(out_jpg)
    }

    /// 清理所有缓存
    pub fn clear(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// 获取当前缓存大小
    pub fn size(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}
