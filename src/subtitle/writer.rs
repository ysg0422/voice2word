//! 字幕生成与导出器 (SRT / ASS / TXT)

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::Segment;
use crate::utils::time::{seconds_to_ass_time, seconds_to_srt_time};

pub struct SubtitleWriter;

impl SubtitleWriter {
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        format: &str,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match format.to_lowercase().as_str() {
            "srt" => Self::write_srt(segments, path),
            "ass" => Self::write_ass(segments, path),
            "txt" => Self::write_txt(segments, path),
            other => anyhow::bail!("不支持的字幕格式: {}", other),
        }
    }

    /// 生成标准 SRT 格式
    pub fn write_srt<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 SRT 文件失败")?;
        for (i, seg) in segments.iter().enumerate() {
            let start = seconds_to_srt_time(seg.start);
            let end = seconds_to_srt_time(seg.end);
            let text = seg.display_text();
            writeln!(file, "{}", i + 1)?;
            writeln!(file, "{} --> {}", start, end)?;
            writeln!(file, "{}", text)?;
            writeln!(file)?;
        }
        Ok(())
    }

    /// 生成标准 ASS 高级字幕格式
    pub fn write_ass<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 ASS 文件失败")?;
        let header = r#"[Script Info]
Title: Voice2Word Subtitle
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
Timer: 100.0000

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Microsoft YaHei,48,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,20,20,30,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#;
        write!(file, "{}", header)?;
        for seg in segments {
            let start = seconds_to_ass_time(seg.start);
            let end = seconds_to_ass_time(seg.end);
            let text = seg.display_text().replace('\n', "\\N");
            writeln!(
                file,
                "Dialogue: 0,{},{},Default,,0,0,0,,{}",
                start, end, text
            )?;
        }
        Ok(())
    }

    /// 生成 TXT 纯文本 (每行一句)
    pub fn write_txt<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 TXT 文件失败")?;
        for seg in segments {
            writeln!(file, "{}", seg.display_text())?;
        }
        Ok(())
    }
}
