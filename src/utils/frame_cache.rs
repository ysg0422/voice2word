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

    /// 异步预加载周围帧（前后各 10 秒）
    pub fn preload_surrounding_frames(
        &self,
        video_path: PathBuf,
        current_time: f64,
        ffmpeg: Arc<FFmpegEngine>,
    ) {
        let cache_clone = self.cache.clone();
        let max_size = self.max_size;

        std::thread::spawn(move || {
            let temp_dir = std::env::temp_dir().join("v2w_frames");
            let _ = std::fs::create_dir_all(&temp_dir);

            // 预加载前后 10 秒，每 0.5 秒一帧（共 40 帧）
            for offset in -20..=20 {
                let t = current_time + (offset as f64 * 0.5);
                if t < 0.0 {
                    continue;
                }

                let key = Self::cache_key(&video_path, t);

                // 检查是否已缓存
                {
                    let cache = cache_clone.lock().unwrap();
                    if cache.len() >= max_size {
                        break; // 缓存已满，停止预加载
                    }
                    if cache.contains_key(&key) {
                        continue;
                    }
                }

                // 生成缓存帧
                let stem = video_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("vid");
                let sec_key = (t * 2.0).round() as i64;
                let out_jpg = temp_dir.join(format!("{}_{}.jpg", stem, sec_key));

                if !out_jpg.exists() {
                    if let Ok(_) = ffmpeg.extract_frame(&video_path, t, &out_jpg) {
                        let mut cache = cache_clone.lock().unwrap();
                        if cache.len() < max_size {
                            cache.insert(key, out_jpg);
                        }
                    }
                }
            }

            info!("视频帧预加载完成，缓存大小: {}", cache_clone.lock().unwrap().len());
        });
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
