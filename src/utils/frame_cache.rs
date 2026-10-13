//! 视频帧智能缓存系统 — 解决时间轴拖动卡顿问题
//!
//! 缓存键 = 完整视频路径 + 文件签名(大小+mtime) + 量化时间(0.1s 精度)：
//! - 完整路径而非仅文件名，避免不同目录同名视频互相串缓存；
//! - 文件签名使同路径被覆盖为新视频后旧帧自动失效；
//! - 哈希采用跨进程稳定的 FNV-1a，保证重启后磁盘帧文件仍可命中复用；
//! - 请求位置附近已有帧时**复用邻格**（见 [`NEIGHBOR_TICKS`]）：拖动时间轴时每 40ms
//!   就有一次抽帧请求，若每次都真起 FFmpeg，画面必然跟不上游标。

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::engines::FFmpegEngine;

/// 磁盘帧目录的保留文件数上限。
///
/// 内存缓存有 `max_size` 上限，但磁盘上的 `%TEMP%/v2w_frames` 原先只增不减：
/// 每拖动一次时间轴（0.1s 量化，比过去细 5 倍）就可能落一个新 JPG，视频库每张卡片也会落一张，
/// 长期使用后该目录会累积成千上万个文件。按「最近使用」保留最近 N 个即可。
const DISK_KEEP_FILES: usize = 400;

/// 每新增这么多帧才扫一次磁盘目录。
/// 扫描需要 `read_dir` + 每个文件的 metadata，不能每抽一帧都做一遍。
const PRUNE_EVERY: usize = 32;

/// 时间量化精度（秒）——每秒 10 格，即拖动时最高 10 帧/秒的画面更新粒度。
///
/// 这里过去是 0.5s：拖动时间轴时 100~400ms 的位移会算出同一个缓存键，命中同一张
/// 旧帧，表现为「游标在动、监视器画面纹丝不动」。10 格/秒大致对应人眼能分辨出
/// 「画面在跟手」的下限，磁盘侧另有 [`DISK_KEEP_FILES`] 兜底，不会因为变密就爆盘。
const TIME_QUANTUM_SEC: f64 = 0.1;

/// 邻格复用窗口（单位：量化格）。
///
/// 请求位置的 ±1 格内已有帧就直接复用，不再起 FFmpeg：拖动时间轴时每 40ms 来一次
/// 请求，而真抽一帧要几十到几百毫秒——每次都真抽，画面更新率就是抽帧速度而不是
/// 拖动速度。复用带来的最大时间偏差 = 1 格 + 半个量化误差 ≈ 0.15s，拖动中看不出来；
/// 单击定位仍先看**精确格**（偏差 ≤ 0.05s），只有精确格没有才退到邻格。
const NEIGHBOR_TICKS: i64 = 1;

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
    /// 预取是否已有一次在跑。
    ///
    /// 预取是「发射后不管」的，而在快速拖动时间轴时**每提交一帧就会请求一次预取**：
    /// 没有这层单飞，拖动几秒就能同时起十几个 FFmpeg，反过来把正在等画面的那次
    /// 抽帧挤慢——本来是为了更跟手，结果更卡。同一时刻只留一次预取即可：
    /// 它只负责「下一格」，而下一格在拖动过程中会被随后的请求覆盖。
    prefetch_busy: std::sync::atomic::AtomicBool,
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
            prefetch_busy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 取缓存锁；即便持锁线程此前 panic 导致 Mutex 中毒，也取回内部值继续用。
    /// 帧缓存只是可随时重建的旁路数据，不值得因一次中毒打断整条预览抽帧链路。
    fn lock_cache(&self) -> std::sync::MutexGuard<'_, FrameCacheInner> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 记一次新增帧，攒够阈值就裁剪一次磁盘目录。
    fn note_disk_write(&self, disk_dir: &Path) {
        if self.since_prune.fetch_add(1, Ordering::Relaxed) + 1 >= PRUNE_EVERY {
            self.since_prune.store(0, Ordering::Relaxed);
            prune_disk_dir(disk_dir, DISK_KEEP_FILES);
        }
    }

    /// 视频文件签名：路径 + 大小 + 亚秒 mtime。
    ///
    /// 只用路径不够：同一路径被覆盖成新视频后，旧帧必须失效。只取整秒 mtime 也不够，
    /// 同一秒内被覆盖的同名同大小视频会得到相同的键，从而复用到上一版的旧帧
    /// （静默显示错帧）。
    fn signature(video_path: &Path) -> u64 {
        let mut sig_parts: Vec<Vec<u8>> = vec![video_path.to_string_lossy().as_bytes().to_vec()];
        if let Ok(meta) = std::fs::metadata(video_path) {
            sig_parts.push(meta.len().to_le_bytes().to_vec());
            if let Ok(modified) = meta.modified() {
                if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                    sig_parts.push(d.as_nanos().to_le_bytes().to_vec());
                }
            }
        }
        let refs: Vec<&[u8]> = sig_parts.iter().map(|v| v.as_slice()).collect();
        stable_hash(&refs)
    }

    /// 时间 → 缓存格号（负时间落回第 0 格）
    fn tick_of(time_sec: f64) -> i64 {
        (time_sec.max(0.0) / TIME_QUANTUM_SEC).round() as i64
    }

    /// 格号 → 该格的取样时间（秒），即这一格的中心。
    ///
    /// 预取必须按**格**取时间（而不是沿用请求里的亚格偏移），否则 `10.04` 与 `10.06`
    /// 会各抽一帧、落进同一个键互相覆盖，白白多跑一次 FFmpeg。
    fn tick_time(tick: i64) -> f64 {
        tick as f64 * TIME_QUANTUM_SEC
    }

    /// 预取目标时间：「请求所在格的下一格」的取样时间（秒）。
    ///
    /// 拖动时请求单调推进，下一格几乎必然被下一次请求命中；提前落盘可以把
    /// 「命中等待 FFmpeg 的几十到几百毫秒」压成一次文件读取。纯函数（不碰任何状态）
    /// 便于单测；负时间与第 0 格一视同仁地前进一格。
    pub fn next_tick_time(time_sec: f64) -> f64 {
        Self::tick_time(Self::tick_of(time_sec) + 1)
    }

    /// 由签名与格号拼键。
    ///
    /// `q10` 是量化精度标记（每秒 10 格）：以后调整精度时一并改标记，旧精度的磁盘帧
    /// 自然不再匹配，交给目录裁剪清掉即可，不必手写迁移。
    fn key_at_tick(sig: u64, tick: i64) -> String {
        format!("{:016x}_q10_{}", sig, tick)
    }

    /// 请求位置可接受的候选键，按优先级排列：精确格 → 偏早的邻格 → 偏晚的邻格。
    ///
    /// 偏早优先：剪辑时「比目标时刻稍早一帧」看到的是这一句的开头，而「稍晚一帧」
    /// 可能已经跳到下一句的画面上了。
    fn candidate_keys(sig: u64, tick: i64) -> Vec<String> {
        let mut keys = Vec::with_capacity(1 + 2 * NEIGHBOR_TICKS as usize);
        keys.push(Self::key_at_tick(sig, tick));
        for step in 1..=NEIGHBOR_TICKS {
            keys.push(Self::key_at_tick(sig, (tick - step).max(0)));
            keys.push(Self::key_at_tick(sig, tick + step));
        }
        keys
    }

    /// 按候选键顺序在内存 + 磁盘里找帧，命中即返回——邻格命中也算命中。
    ///
    /// 内存里记着但文件已被裁剪/清理的情况视为未命中，不能把不存在的路径交给渲染层。
    ///
    /// `register_disk_hits` 决定磁盘命中是否登记进内存 LRU：正式请求要（下次同格直接
    /// 内存命中），`peek` 这类只读探测不要（探测不该有副作用）。
    fn lookup_cached(
        &self,
        keys: &[String],
        disk_dir: &Path,
        register_disk_hits: bool,
    ) -> Option<(String, PathBuf)> {
        for key in keys {
            if let Some(path) = self.lock_cache().map.get(key).cloned() {
                if path.exists() {
                    return Some((key.clone(), path));
                }
            }
            let on_disk = disk_dir.join(format!("{key}.jpg"));
            if on_disk.exists() {
                if register_disk_hits {
                    self.lock_cache()
                        .insert(key.clone(), on_disk.clone(), self.max_size);
                }
                return Some((key.clone(), on_disk));
            }
        }
        None
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
        let sig = Self::signature(video_path);
        let tick = Self::tick_of(time_sec);
        let keys = Self::candidate_keys(sig, tick);

        let temp_dir = Self::disk_dir();
        let _ = std::fs::create_dir_all(&temp_dir);

        // 先按候选键（精确格 → 邻格）找：命中就复用，不起 FFmpeg。
        // 这是「拖动时间轴画面跟手」的关键一步（见 [`NEIGHBOR_TICKS`]）。
        if let Some((_, cached)) = self.lookup_cached(&keys, &temp_dir, true) {
            return Ok(cached);
        }

        // 未命中，只能真抽一帧：按**精确格**取键。
        self.extract_at(video_path, time_sec, sig, tick, ffmpeg)
    }

    /// 只读探测：`time_sec` 的精确格或 ±1 邻格是否已有可用帧（内存或磁盘）。
    ///
    /// 不抽帧、不写盘、**不登记**内存缓存——供预取/调度路径廉价地问「下一格要不要
    /// 提前抽」。命中判定与正式请求同源（[`Self::candidate_keys`]），所以探测说
    /// 「有」的位置，正式请求也一定能立即拿到。
    pub fn peek(&self, video_path: &Path, time_sec: f64) -> Option<PathBuf> {
        let sig = Self::signature(video_path);
        let tick = Self::tick_of(time_sec);
        let keys = Self::candidate_keys(sig, tick);
        self.lookup_cached(&keys, &Self::disk_dir(), false)
            .map(|(_, path)| path)
    }

    /// 预取「请求位置的下一格」：命中（内存/磁盘）直接返回；未命中则真抽一格落盘。
    ///
    /// **必须在后台线程 / 后台执行器里调用**：未命中会起一次 FFmpeg。与
    /// [`Self::get_or_extract`] 的区别是**不走邻格复用**——预取的目标就是下一格
    /// 本身，允许邻格复用的话它会把当前这一格原样还回来，等于没预取。
    /// 抽帧失败时返回错误，调用方按「预取失败」忽略即可（不影响当前请求）。
    pub fn prefetch_next_tick(
        &self,
        video_path: &Path,
        time_sec: f64,
        ffmpeg: &FFmpegEngine,
    ) -> Result<PathBuf> {
        // 单飞：已有预取在跑就立刻返回。预取只是「提前把下一格做好」，
        // 拖动过程中下一次请求很快就会来，重复起 FFmpeg 只会互相抢 CPU。
        if self
            .prefetch_busy
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            anyhow::bail!("已有预取在进行中");
        }
        // 无论下面哪条 return/早退路径，都要把标志放回去（含 `?` 提前返回）。
        let _guard = PrefetchGuard(&self.prefetch_busy);

        let next_time = Self::next_tick_time(time_sec);
        let sig = Self::signature(video_path);
        let tick = Self::tick_of(next_time);
        let key = Self::key_at_tick(sig, tick);
        let temp_dir = Self::disk_dir();
        // 只按**精确格**找（单键候选）：预取要的是「下一格」本身。
        if let Some((_, cached)) = self.lookup_cached(&[key], &temp_dir, true) {
            return Ok(cached);
        }
        self.extract_at(video_path, next_time, sig, tick, ffmpeg)
    }

    /// 真抽一帧并原子落盘（按**精确格**，不走邻格复用）。
    ///
    /// 磁盘文件名直接使用缓存键（含路径+签名哈希），同名视频互不覆盖。
    fn extract_at(
        &self,
        video_path: &Path,
        time_sec: f64,
        sig: u64,
        tick: i64,
        ffmpeg: &FFmpegEngine,
    ) -> Result<PathBuf> {
        let temp_dir = Self::disk_dir();
        let _ = std::fs::create_dir_all(&temp_dir);
        let key = Self::key_at_tick(sig, tick);
        let out_jpg = temp_dir.join(format!("{}.jpg", key));

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
        self.lock_cache()
            .insert(key, out_jpg.clone(), self.max_size);
        // 磁盘目录也要有上限，否则长期使用后 v2w_frames 会只增不减
        self.note_disk_write(&temp_dir);

        Ok(out_jpg)
    }

    /// 清理所有缓存（含磁盘帧目录）
    pub fn clear(&self) {
        {
            let mut inner = self.lock_cache();
            inner.map.clear();
            inner.order.clear();
        }
        let _ = std::fs::remove_dir_all(Self::disk_dir());
    }

    /// 获取当前缓存大小
    pub fn size(&self) -> usize {
        self.lock_cache().map.len()
    }
}

/// 预取单飞标志的 RAII 放行器：函数从任何路径返回（含 `?` 传播错误）都会复位。
struct PrefetchGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for PrefetchGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    // 刻意**不写** `use super::*`：本组只依赖显式列出的名字，父模块增删私有项不会
    // 悄悄改变这里的解析结果（与 `ui/views/library.rs`、`engines/whisper.rs` 单测同规）。
    use super::{prune_disk_dir, FrameCache};
    use std::path::{Path, PathBuf};

    fn key_for(path: &str, t: f64) -> String {
        FrameCache::key_at_tick(
            FrameCache::signature(Path::new(path)),
            FrameCache::tick_of(t),
        )
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

    /// 同路径被覆盖为新视频后旧帧必须失效：文件签名里的 mtime 必须精确到亚秒。
    /// 只取整秒时，同一秒内被覆盖的同名同大小文件会算出同一个键，从而复用到旧帧
    /// （界面上显示上一版画面，且稳定复现）。
    #[test]
    fn signature_tracks_subsecond_mtime() {
        let path = std::env::temp_dir().join(format!("v2w_fc_sig_{}.mp4", std::process::id()));
        std::fs::write(&path, b"AAAA").unwrap();

        let base = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_500);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(base)
            .unwrap();
        let k0 = key_for(&path.to_string_lossy(), 3.0);

        // 同一整秒内被覆盖（大小不变），只有 mtime 的亚秒部分变化
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(base + std::time::Duration::from_millis(250))
            .unwrap();
        let k1 = key_for(&path.to_string_lossy(), 3.0);

        assert_ne!(k0, k1, "同一秒内被覆盖的同名同大小视频必须失效旧帧");
        let _ = std::fs::remove_file(&path);
    }

    /// 0.1s 量化：格内共享同一张帧，跨格不共享。
    ///
    /// 这里守住的是「精度别被改回 0.5s」——过去 0.5s 精度下拖动时间轴会出现
    /// 「游标在动、画面不跟手」，正是本模块要根治的毛病。
    #[test]
    fn quantized_time_shares_entry() {
        // 10.01 与 10.04 都在第 100 格（10.00~10.05）
        assert_eq!(key_for(r"C:\v.mp4", 10.01), key_for(r"C:\v.mp4", 10.04));
        // 10.21 落在第 102 格，必须换帧
        assert_ne!(key_for(r"C:\v.mp4", 10.01), key_for(r"C:\v.mp4", 10.21));
        // 旧 0.5s 精度下 10.01 与 10.24 会撞键，现在必须区分开
        assert_ne!(key_for(r"C:\v.mp4", 10.01), key_for(r"C:\v.mp4", 10.24));
    }

    /// 邻格复用：候选键顺序为「精确格 → 偏早邻格 → 偏晚邻格」，且负格号夹到 0。
    ///
    /// 顺序不能乱：偏早优先，拖动时看到的是这一句的开头；偏晚可能已经跳到下一句画面。
    #[test]
    fn neighbor_tick_reuses_nearby_frame() {
        let sig = 0x1234_5678_9abc_def0_u64;
        let keys = FrameCache::candidate_keys(sig, 100);
        assert_eq!(keys.len(), 3, "1 个精确格 + 前后各 1 个邻格");
        assert_eq!(keys[0], FrameCache::key_at_tick(sig, 100));
        assert_eq!(
            keys[1],
            FrameCache::key_at_tick(sig, 99),
            "偏早邻格优先于偏晚"
        );
        assert_eq!(keys[2], FrameCache::key_at_tick(sig, 101));

        // 第 0 格没有「-1 格」：夹到 0 而不是负数（负数键会生成永不命中的文件名）
        let first = FrameCache::candidate_keys(sig, 0);
        assert_eq!(first[0], FrameCache::key_at_tick(sig, 0));
        assert_eq!(first[1], FrameCache::key_at_tick(sig, 0));
        assert_eq!(first[2], FrameCache::key_at_tick(sig, 1));
    }

    #[test]
    fn evicts_oldest_fifo() {
        let cache = FrameCache::new(2);
        let inner_path = |s: &str| PathBuf::from(s);
        {
            let mut inner = cache.lock_cache();
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

    /// 预取目标：请求所在格的**下一格**；同一格内的亚格偏移不改变目标格，
    /// 负时间先夹到第 0 格再前进一格。
    #[test]
    fn next_tick_time_advances_one_quantum() {
        assert_eq!(FrameCache::next_tick_time(3.0), FrameCache::tick_time(31));
        // 3.0 与 2.96 都落在第 30 格：预取目标必须一致（否则同格反复抽帧互相覆盖）
        assert_eq!(
            FrameCache::next_tick_time(3.0),
            FrameCache::next_tick_time(2.96)
        );
        // 已经跨到下一格时目标随之推进
        assert_eq!(FrameCache::next_tick_time(3.2), FrameCache::tick_time(33));
        // 负时间与第 0 格一视同仁：夹到第 0 格后再前进一格
        assert_eq!(FrameCache::next_tick_time(-5.0), FrameCache::tick_time(1));
        assert_eq!(FrameCache::next_tick_time(0.0), FrameCache::tick_time(1));
    }

    /// `peek` 是只读探测：精确格优先、±1 邻格可命中、更远不命中，
    /// 且**不改动**内存缓存（磁盘命中也不登记）。
    #[test]
    fn peek_reports_exact_then_neighbor_without_side_effects() {
        let cache = FrameCache::new(4);
        // 不存在的视频：签名只由路径决定，磁盘上不会有它的帧
        let video = Path::new("Z:/v2w_frame_cache_peek/missing_video.mp4");
        assert!(cache.peek(video, 3.0).is_none(), "全新位置不应命中");
        assert_eq!(cache.size(), 0, "peek 未命中不得改动内存缓存");

        // 造一张真实存在的「帧文件」，登记到第 30 格（3.0s）
        let dir = std::env::temp_dir().join(format!("v2w_peek_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame = dir.join("tick30.jpg");
        std::fs::write(&frame, b"jpg").unwrap();
        let sig = FrameCache::signature(video);
        cache.lock_cache().insert(
            FrameCache::key_at_tick(sig, FrameCache::tick_of(3.0)),
            frame.clone(),
            cache.max_size,
        );

        assert_eq!(cache.peek(video, 3.0), Some(frame.clone()), "精确格命中");
        // 2.94 落在第 29 格：候选键 29 → 28 → 30，偏晚邻格也能命中
        assert_eq!(
            cache.peek(video, 2.94),
            Some(frame.clone()),
            "±1 邻格复用必须与正式请求同源"
        );
        // 3.2 落在第 32 格：候选键 32 → 31 → 33，离第 30 格两格以上，必须未命中
        assert!(cache.peek(video, 3.2).is_none(), "±2 格之外必须重新抽帧");

        // 磁盘命中同样不登记（peek 无任何写副作用）：用一个真实帧目录里的唯一键验证
        let disk_frame =
            FrameCache::disk_dir().join(format!("{}.jpg", FrameCache::key_at_tick(sig, 55)));
        std::fs::create_dir_all(FrameCache::disk_dir()).unwrap();
        std::fs::write(&disk_frame, b"jpg").unwrap();
        let before = cache.size();
        assert_eq!(
            cache.peek(video, 5.5).as_deref(),
            Some(disk_frame.as_path())
        );
        assert_eq!(cache.size(), before, "磁盘命中的 peek 不得登记内存缓存");
        let _ = std::fs::remove_file(&disk_frame);

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 预取必须单飞：已经有一次在跑时，再请求要**立刻**返回 Err。
    ///
    /// 回归护栏：快速拖动时每提交一帧都会请求一次预取，没有单飞就会同时起十几个
    /// FFmpeg，反过来把正在等画面那次抽帧挤慢——为了更跟手反而更卡。
    #[test]
    fn prefetch_is_single_flight() {
        let cache = FrameCache::new(4);
        cache
            .prefetch_busy
            .store(true, std::sync::atomic::Ordering::Release);
        let ffmpeg = crate::engines::FFmpegEngine::new("ffmpeg-does-not-exist");
        let err = cache
            .prefetch_next_tick(std::path::Path::new("C:/v.mp4"), 1.0, &ffmpeg)
            .expect_err("已有预取在跑时必须被挡下")
            .to_string();
        assert!(err.contains("已有预取"), "错误文案要说清原因：{err}");
        cache
            .prefetch_busy
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// 预取失败也必须把单飞标志放回去：否则一次失败就把预取永久卡死。
    #[test]
    fn prefetch_releases_single_flight_flag_after_failure() {
        let cache = FrameCache::new(4);
        let ffmpeg = crate::engines::FFmpegEngine::new("definitely-not-a-real-ffmpeg");
        let _ =
            cache.prefetch_next_tick(std::path::Path::new("C:/no-such-video.mp4"), 0.0, &ffmpeg);
        assert!(
            !cache
                .prefetch_busy
                .load(std::sync::atomic::Ordering::Acquire),
            "失败路径必须放行，否则预取永久卡死"
        );
    }
}
