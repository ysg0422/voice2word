//! 媒体文件内容指纹：用于「同一份内容、路径不同」的缓存命中，以及
//! 「同一路径、内容已变」的缓存失效。
//!
//! # 为什么不能只按路径命中缓存
//!
//! 旧的 `find_cached_task` 只比对 `file_path`。这有两个方向的问题：
//! - **路径相同、内容变了**：用户用剪辑软件重新导出、覆盖了同名视频，程序却
//!   直接拿旧字幕命中缓存——用户看到的是**上一版**的字幕，且毫无提示。这是
//!   静默的正确性错误。
//! - **内容相同、路径变了**：复制 / 改名后的同一个文件，缓存全部落空，白跑一遍
//!   转写。
//!
//! 给内容算一个指纹即可同时解决两者。
//!
//! # 为什么不做「全文件 sha256」
//!
//! 5 GB 的视频全量哈希要几十秒，绝不能挂在选文件的交互路径上。这里用
//! **采样哈希**：`sha256(文件大小 ‖ 前 256 KB ‖ 后 256 KB)`。
//! - 对「重新导出的同名视频」——大小或首尾字节几乎必然变化，指纹变化 → 正确失效；
//! - 对「复制 / 改名」——内容与大小完全一致，指纹一致 → 正确命中；
//! - 代价只有两次寻址 + 512 KB 读取（实测个位数毫秒），可挂在选文件时算一次。
//!
//! 采样哈希理论上可能对「大小相同、首尾 256 KB 相同、仅中间不同」的两个文件
//! 误判为同一份。对教学视频/录音这类真实素材几乎不可能发生，且**误判的代价
//! 只是缓存命中（用户可手动重转）**，远小于全量哈希的性能代价。

use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 采样窗口大小（首 / 尾各取这么多字节）。
const SAMPLE_BYTES: u64 = 256 * 1024;

/// 计算媒体文件的内容指纹，返回 64 位十六进制字符串。
///
/// 文件不存在 / 读失败时返回 `None`——调用方应把 `None` 当作「指纹未知」，
/// 退回旧的「只按路径命中」行为，绝不能因此把一份完好缓存判为失效。
pub fn media_fingerprint<P: AsRef<Path>>(path: P) -> Option<String> {
    let path = path.as_ref();
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();

    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());

    let head_len = size.min(SAMPLE_BYTES);
    if head_len > 0 {
        let mut buf = vec![0u8; head_len as usize];
        if file.read_exact(&mut buf).is_err() {
            return None;
        }
        hasher.update(&buf);
    }

    // 尾部采样：仅当文件比一个窗口还大时才单独读尾，避免小文件把首部读两遍。
    if size > SAMPLE_BYTES {
        let tail_len = SAMPLE_BYTES.min(size - SAMPLE_BYTES);
        if file.seek(SeekFrom::Start(size - tail_len)).is_err() {
            return None;
        }
        let mut buf = vec![0u8; tail_len as usize];
        if file.read_exact(&mut buf).is_err() {
            return None;
        }
        hasher.update(&buf);
    }

    Some(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("v2w_fp_{}_{}", std::process::id(), name));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn identical_content_same_fingerprint() {
        let data = vec![7u8; 1024];
        let a = write_temp("a.bin", &data);
        let b = write_temp("b.bin", &data);
        assert_eq!(media_fingerprint(&a), media_fingerprint(&b));
        assert_eq!(media_fingerprint(&a).unwrap().len(), 64);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn different_content_differs() {
        let a = write_temp("c.bin", &vec![1u8; 4096]);
        let b = write_temp("d.bin", &vec![2u8; 4096]);
        assert_ne!(media_fingerprint(&a), media_fingerprint(&b));
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 尾部改动必须被察觉：这正是「重新导出同名视频」最常见的形态
    /// （前半段一样，末尾字幕/片尾变了）。
    #[test]
    fn tail_change_is_detected_for_large_files() {
        let n = (SAMPLE_BYTES * 3) as usize;
        let mut data = vec![9u8; n];
        let a = write_temp("big1.bin", &data);
        // 只改最后一个字节
        data[n - 1] = 0;
        let b = write_temp("big2.bin", &data);
        assert_ne!(
            media_fingerprint(&a),
            media_fingerprint(&b),
            "大文件尾部变化必须改变指纹"
        );
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn missing_file_returns_none() {
        let p = std::env::temp_dir().join("v2w_fp_definitely_missing_zzz.bin");
        let _ = std::fs::remove_file(&p);
        assert!(media_fingerprint(&p).is_none());
    }
}
