//! 字幕生成与导出器 (SRT / ASS / TXT)

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment};
use crate::utils::time::{seconds_to_ass_time, seconds_to_srt_time};
use crate::utils::SubtitleStyleConfig;

pub struct SubtitleWriter;

impl SubtitleWriter {
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        format: &str,
    ) -> Result<()> {
        let mode = if segments.iter().any(|s| s.translation.is_some()) {
            ExportMode::Bilingual
        } else {
            ExportMode::RawOnly
        };
        Self::write_to_file_with_mode(segments, path, format, mode)
    }

    pub fn write_to_file_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        format: &str,
        mode: ExportMode,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        match format.to_lowercase().as_str() {
            "srt" => Self::write_srt_with_mode(segments, path, mode),
            "ass" => Self::write_ass_with_mode(segments, path, mode),
            "vtt" => Self::write_vtt_with_mode(segments, path, mode),
            "txt" => Self::write_txt_with_mode(segments, path, mode),
            "fcpxml" => super::fcpxml::FcpXmlExporter::write_to_file(segments, path, "Voice2Word Subtitles"),
            "xml" | "premiere" => super::premiere::PremiereXmlExporter::write_to_file(segments, path, "Voice2Word Subtitles"),
            "jianying" => {
                let _ = super::jianying::JianYingExporter::export_to_folder(segments, None, path, "Voice2Word Draft")?;
                Ok(())
            }
            other => anyhow::bail!("不支持的字幕格式: {}", other),
        }
    }

    /// 生成标准 SRT 格式
    pub fn write_srt<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        Self::write_srt_with_mode(segments, path, ExportMode::RawOnly)
    }

    pub fn write_srt_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 SRT 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for (i, seg) in segments.iter().enumerate() {
            let start = seconds_to_srt_time(seg.start);
            let end = seconds_to_srt_time(seg.end);
            let text = super::segment::export_text_for(seg, mode, with_speaker);
            writeln!(file, "{}", i + 1)?;
            writeln!(file, "{} --> {}", start, end)?;
            writeln!(file, "{}", text)?;
            writeln!(file)?;
        }
        Ok(())
    }

    /// 生成标准 ASS 高级字幕格式
    pub fn write_ass<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        Self::write_ass_with_mode(segments, path, ExportMode::RawOnly)
    }

    pub fn write_ass_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        Self::write_ass_with_style(segments, path, mode, &SubtitleStyleConfig::default())
    }

    /// 带样式的写出：目前仅 ASS 需要样式表（PlayResY 固定 1080，
    /// 因此 `font_size` / `bottom_margin` 可与配置 1:1 对应），其余格式委托原实现。
    pub fn write_to_file_with_style<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        format: &str,
        mode: ExportMode,
        style: &SubtitleStyleConfig,
    ) -> Result<()> {
        if format.eq_ignore_ascii_case("ass") {
            Self::write_ass_with_style(segments, path, mode, style)
        } else {
            Self::write_to_file_with_mode(segments, path, format, mode)
        }
    }

    /// 按用户配置的字幕样式生成 ASS。
    ///
    /// ASS 的 PlayResY 固定为 1080，所以 `font_size` 与 `bottom_margin` 可直接写入，
    /// 与主界面预览共用同一套参数（预览只是按 `PREVIEW_SCALE` 缩放显示）。
    /// 注意：ASS 规范没有行高字段，`line_spacing` 仅作用于预览，不写入 ASS。
    pub fn write_ass_with_style<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        style: &SubtitleStyleConfig,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 ASS 文件失败")?;
        let (primary, outline, back, border_style, outline_w, shadow) =
            ass_preset_colors(&style.preset_name);
        let header = format!(
            r#"[Script Info]
Title: Voice2Word Subtitle
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
Timer: 100.0000

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Microsoft YaHei,{size},{primary},&H000000FF,{outline},{back},0,0,0,0,100,100,{spacing},0,{border_style},{outline_w},{shadow},2,20,20,{margin_v},1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#,
            size = style.font_size,
            spacing = style.letter_spacing,
            margin_v = style.bottom_margin,
        );
        write!(file, "{}", header)?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let start = seconds_to_ass_time(seg.start);
            let end = seconds_to_ass_time(seg.end);
            let text = super::segment::export_text_for(seg, mode, with_speaker).replace('\n', "\\N");
            writeln!(
                file,
                "Dialogue: 0,{},{},Default,,0,0,0,,{}",
                start, end, text
            )?;
        }
        Ok(())
    }

    /// 生成 WebVTT 格式 (无序号、点号毫秒分隔，供网页 <track> 使用)
    pub fn write_vtt_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 VTT 文件失败")?;
        writeln!(file, "WEBVTT")?;
        writeln!(file)?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let start = seconds_to_srt_time(seg.start).replace(',', ".");
            let end = seconds_to_srt_time(seg.end).replace(',', ".");
            let text = super::segment::export_text_for(seg, mode, with_speaker);
            writeln!(file, "{} --> {}", start, end)?;
            writeln!(file, "{}", text)?;
            writeln!(file)?;
        }
        Ok(())
    }

    /// 生成 TXT 纯文本 (每行一句)
    pub fn write_txt<P: AsRef<Path>>(segments: &[Segment], path: P) -> Result<()> {
        Self::write_txt_with_mode(segments, path, ExportMode::RawOnly)
    }

    pub fn write_txt_with_mode<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 TXT 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            writeln!(file, "{}", super::segment::export_text_for(seg, mode, with_speaker))?;
        }
        Ok(())
    }
}

/// 预设 → ASS 样式参数：
/// `(PrimaryColour, OutlineColour, BackColour, BorderStyle, Outline, Shadow)`
///
/// ASS 颜色写作 `&HAABBGGRR`（alpha 越大越透明，`00` = 不透明）。
/// GPUI 无法渲染描边与投影，所以预设的「黑边 / 黑影 / 底框」差异主要体现在导出结果上。
fn ass_preset_colors(preset: &str) -> (&'static str, &'static str, &'static str, u32, f32, f32) {
    match preset {
        // 黄字 + 粗黑边
        "黄字黑边" => ("&H0000D7FF", "&H00000000", "&H80000000", 1, 3.0, 1.0),
        // 白字 + 半透明底框（BorderStyle = 3 为不透明框模式）
        "半透明黑框" => ("&H00FFFFFF", "&H00000000", "&H80000000", 3, 1.0, 0.0),
        // 无边框无阴影，纯悬浮白字
        "电影沉浸" => ("&H00FFFFFF", "&H00000000", "&H00000000", 1, 0.0, 0.0),
        // 白字黑影（默认）：白字 + 细黑边 + 投影
        _ => ("&H00FFFFFF", "&H00000000", "&H80000000", 1, 2.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_webvtt_header_and_dot_timestamps() {
        let segs = vec![Segment::new(1, 1.0, 2.5, "你好世界")];
        let temp = std::env::temp_dir().join("test_v2w.vtt");
        SubtitleWriter::write_to_file(&segs, &temp, "vtt").unwrap();

        let content = std::fs::read_to_string(&temp).unwrap();
        assert!(content.starts_with("WEBVTT\n"), "缺少 WEBVTT 头: {content}");
        assert!(content.contains("00:00:01.000 --> 00:00:02.500"), "毫秒应以点号分隔: {content}");
        assert!(content.contains("你好世界"));
        assert!(!content.contains(','), "VTT 时间戳不应含逗号");

        let _ = std::fs::remove_file(temp);
    }

    /// 回归：主界面配置的字幕样式必须真正写进 ASS 样式表，
    /// 否则「样式配置」只是 UI 摆设。
    #[test]
    fn ass_style_line_follows_config() {
        let segs = vec![Segment::new(1, 1.0, 2.5, "测试字幕")];
        let temp = std::env::temp_dir().join("test_v2w_style.ass");

        let mut style = SubtitleStyleConfig::default();
        style.font_size = 48;
        style.letter_spacing = 4;
        style.bottom_margin = 80;
        style.preset_name = "黄字黑边".to_string();
        SubtitleWriter::write_ass_with_style(&segs, &temp, ExportMode::RawOnly, &style).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        assert!(
            content.contains("Style: Default,Microsoft YaHei,48,"),
            "字号未写入 ASS: {content}"
        );
        assert!(content.contains("&H0000D7FF"), "黄字预设配色未写入 ASS: {content}");
        // Spacing=4 / BorderStyle=1 / Outline=3 / Shadow=1 / MarginV=80
        assert!(
            content.contains(",100,100,4,0,1,3,1,2,20,20,80,1"),
            "字间距 / 描边 / 底边距未写入 ASS: {content}"
        );
        assert!(content.contains("测试字幕"), "对白文本缺失: {content}");

        let _ = std::fs::remove_file(temp);
    }

    /// 默认预设（白字黑影）应写回白字 + 细黑边 + 投影
    #[test]
    fn ass_default_preset_is_white_with_shadow() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "默认样式")];
        let temp = std::env::temp_dir().join("test_v2w_style_default.ass");
        SubtitleWriter::write_ass(&segs, &temp).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();
        assert!(content.contains("&H00FFFFFF"), "默认应为白字: {content}");
        // Outline=2 / Shadow=1
        assert!(content.contains(",1,2,1,2,20,20,40,1"), "默认描边与投影不正确: {content}");
        let _ = std::fs::remove_file(temp);
    }
}
