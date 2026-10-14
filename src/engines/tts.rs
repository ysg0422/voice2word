//! Edge-TTS 语音合成与时间线配音引擎
//! 支持基于微软 Edge-TTS 免费云端接口生成高质量配音，并按照字幕时间轴进行对齐混音与音轨替换。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

use crate::subtitle::Segment;
use crate::utils::AppConfig;

/// 预设的高质量 Edge-TTS 声音选项：(声音 ID, 展示名称, 语言, 性别)
pub const PRESET_VOICES: &[(&str, &str, &str, &str)] = &[
    ("zh-CN-XiaoxiaoNeural", "晓晓 (温柔自然)", "zh", "女"),
    ("zh-CN-YunxiNeural", "云希 (活泼生动)", "zh", "男"),
    ("zh-CN-YunjianNeural", "云健 (沉稳解说)", "zh", "男"),
    ("zh-CN-XiaoyiNeural", "晓伊 (亲切随和)", "zh", "女"),
    ("en-US-JennyNeural", "Jenny (英语标准)", "en", "女"),
    ("en-US-GuyNeural", "Guy (英语标准)", "en", "男"),
];

/// 批量合成配置项
#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct BatchItem {
    text: String,
    voice: String,
    output: String,
}

/// TTS 配音引擎
#[derive(Debug, Clone)]
pub struct TTSEngine {
    pub python_path: PathBuf,
    pub runner_path: PathBuf,
}

impl TTSEngine {
    /// 构造 TTS 引擎实例，自动解析 Python 解释器与 runner 脚本
    pub fn new() -> Self {
        let python_path = AppConfig::resolve_path("tools/python.exe");
        let runner_path = AppConfig::resolve_path("tools/edge_tts_runner.py");
        Self {
            python_path,
            runner_path,
        }
    }

    /// 检查 TTS 执行脚本是否存在
    pub fn is_available(&self) -> bool {
        self.runner_path.is_file()
    }

    /// 合成单句文本至目标音频文件 (MP3 或 WAV)
    pub fn synthesize_sentence<P: AsRef<Path>>(
        &self,
        text: &str,
        voice: &str,
        output_path: P,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let out = output_path.as_ref();
        info!("Edge-TTS 合成文本: {:?}, 音色: {}", text, voice);

        let mut cmd = Command::new(&self.python_path);
        super::media_pipeline::apply_default_child_flags(&mut cmd);
        cmd.arg(&self.runner_path)
            .arg("--text")
            .arg(text)
            .arg("--voice")
            .arg(voice)
            .arg("--output")
            .arg(out);

        let output = cmd
            .output()
            .with_context(|| format!("执行 Edge-TTS 脚本失败: {:?}", self.runner_path))?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Edge-TTS 合成失败: {err}");
        }
        Ok(())
    }

    /// 批量为字幕片段生成配音任务清单（纯函数，便于测试）
    pub fn plan_batch_items(
        segments: &[Segment],
        voice: &str,
        temp_dir: &Path,
    ) -> Vec<(usize, String, String, PathBuf)> {
        segments
            .iter()
            .filter(|s| !s.display_text().trim().is_empty())
            .map(|s| {
                let out_file = temp_dir.join(format!("tts_seg_{}.mp3", s.index));
                (
                    s.index,
                    s.display_text().to_string(),
                    voice.to_string(),
                    out_file,
                )
            })
            .collect()
    }
}

impl Default for TTSEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_voices_are_valid() {
        assert!(!PRESET_VOICES.is_empty());
        for (id, name, lang, gender) in PRESET_VOICES {
            assert!(!id.is_empty());
            assert!(!name.is_empty());
            assert!(!lang.is_empty());
            assert!(!gender.is_empty());
        }
    }

    #[test]
    fn plan_batch_items_filters_empty_text() {
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "第一句"),
            Segment::new(2, 1.0, 2.0, "   "),
            Segment::new(3, 2.0, 3.0, "第三句"),
        ];
        let items = TTSEngine::plan_batch_items(&segs, "zh-CN-XiaoxiaoNeural", Path::new("temp"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, 1);
        assert_eq!(items[0].1, "第一句");
        assert_eq!(items[0].2, "zh-CN-XiaoxiaoNeural");
        assert_eq!(items[1].0, 3);
        assert_eq!(items[1].1, "第三句");
        assert_eq!(items[1].2, "zh-CN-XiaoxiaoNeural");
    }
}
