//! 启动时清扫 `%TEMP%` 下过期的中间产物。
//!
//! 为什么需要它：转写链路会在系统临时目录里留下好几类中间文件——
//! 语音增强/压实后的 WAV (`v2w_prep_*.wav`)、CPU 切块用的临时 WAV
//! (`*_voice2word.wav`)、切块目录 (`v2w_chunks_*`)、whisper 的 JSON 输出
//! (`v2w_whisper_*.json`)、预览代理视频 (`voice2word_proxy_*.mp4`)、
//! 预览字幕 (`voice2word_preview_*.srt`)、LLM 提示词 (`v2w_prompt_*.txt`) 等。
//!
//! 这些文件**只在「一路跑完」的正常路径上**才会被删除。一旦转写中途报错、
//! 被用户取消、或进程被强杀，删除代码就不会被执行，文件会永久留在用户的
//! TEMP 目录里逐次累积（实测一次失败即留下 ~13 MB 的压实 WAV）。
//!
//! 这里刻意**不去改转写管线的删除职责**（那会牵动并发编辑中的核心文件），
//! 而是加一道兜底：每次启动清扫一次「足够旧」的残留。因为只删超过
//! [`STALE_AFTER`] 的文件，所以不会误伤正在运行的另一个实例或其它进程。

use std::time::{Duration, SystemTime};

/// 只有年龄超过这个值的条目才会被清扫。
///
/// 取 12 小时而不是更短，是为了绝不误删「正在进行的超长视频转写」的中间文件：
/// 单次转写不可能持续 12 小时，而崩溃/取消留下的残留最迟在次日启动时被清掉，
/// 因此 TEMP 里的堆积量被限制在「最近一天」以内，不会无限增长。
const STALE_AFTER: Duration = Duration::from_secs(12 * 60 * 60);

/// 明确属于本项目的文件名前缀。只清扫这些名字，绝不泛扫整个 TEMP。
const NAME_PREFIXES: [&str; 3] = ["v2w_", "voice2word_", "test_v2w"];

/// 明确属于本项目的文件名后缀（这类名字没有项目前缀，例如 `片名_voice2word.wav`）。
const NAME_SUFFIXES: [&str; 2] = ["_voice2word.wav", "_voice2word.srt"];

/// 帧缓存目录由 [`crate::utils::FrameCache`] 自己按数量裁剪，这里不重复管理。
const FRAME_CACHE_DIR: &str = "v2w_frames";

fn is_ours(name: &str) -> bool {
    if name == FRAME_CACHE_DIR {
        return false;
    }
    NAME_PREFIXES.iter().any(|p| name.starts_with(p))
        || NAME_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// 清扫 `%TEMP%` 下过期的本项目中间产物，返回删除的条目数（文件 + 目录）。
///
/// 任何一步失败都静默跳过：清扫只是兜底，不能因为它让应用启动失败。
pub fn sweep_stale_temp_files() -> usize {
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0usize;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_ours(&name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        // 系统时钟被回拨时 duration_since 会返回 Err，此时保守跳过而不是当成「无限旧」
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < STALE_AFTER {
            continue;
        }

        let path = entry.path();
        let ok = if meta.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if ok {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_project_temp_names() {
        assert!(is_ours("v2w_prep_1234_567.wav"));
        assert!(is_ours("v2w_chunks_1234_567"));
        assert!(is_ours("v2w_whisper_1234_567.json"));
        assert!(is_ours("voice2word_proxy_x_1_2_720p.mp4"));
        assert!(is_ours("voice2word_preview_1234.srt"));
        assert!(is_ours("第一课_voice2word.wav"));
    }

    #[test]
    fn leaves_foreign_files_alone() {
        // 绝不能碰其它程序的临时文件
        assert!(!is_ours("chrome_installer.exe"));
        assert!(!is_ours("tmp1234.tmp"));
        assert!(!is_ours("voice2word.db"));
        assert!(!is_ours("v2w_frames")); // 由 FrameCache 自己裁剪
    }
}
