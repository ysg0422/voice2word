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
        // 用 `Segment::has_translation` 而非 `translation.is_some()`：空串译文不算
        // 「有译文」，否则会导出成双语却多出一行空白。
        let mode = if segments.iter().any(|s| s.has_translation()) {
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

        // ── 先写临时文件，成功后再替换目标 ──
        //
        // 原先各格式直接 `File::create(path)`：这会**立刻把目标文件截断为 0**，
        // 然后才逐行写入。于是导出过程中任何失败（磁盘满、用户中途拔盘、
        // 字符串格式化出错）都会把用户**原有的字幕文件**毁掉，只剩半截内容——
        // 而「覆盖导出」恰恰是最常见的使用方式（改完字幕重新导一次）。
        //
        // 改为：写到同目录的 `.part`，全部写完并 flush 成功后才 rename 覆盖。
        // rename 在同一目录内是原子的（Windows 上用 MoveFileEx 的替换语义，
        // 由 `std::fs::rename` 提供），因此目标文件要么是旧内容、要么是完整新内容。
        //
        // 剪映导出是个例外：它产出的是**整个草稿目录**（多个 json 文件），
        // 不是单文件，无法用单次 rename 表达原子性，因此保持原样（见下）。
        let tmp_path = match Self::temp_sibling(path) {
            Some(p) => p,
            None => {
                // 没有父目录（极端路径）时退化为直接写，至少不 panic
                return Self::write_in_place(segments, path, format, mode);
            }
        };

        let result = Self::write_in_place(segments, &tmp_path, format, mode);
        match result {
            Ok(()) => {
                if let Err(err) = std::fs::rename(&tmp_path, path) {
                    // 收尾失败要把临时文件清掉，否则用户目录里会留下一堆 .part
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(err).with_context(|| {
                        format!("替换字幕文件失败: {}", path.display())
                    });
                }
                Ok(())
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                Err(err)
            }
        }
    }

    /// 同目录的临时文件名（保留原扩展名，便于外部工具识别格式）。
    ///
    /// 用 `.part` 前缀而不是后缀，是为了不与「用户自己命名的 .part 文件」冲突，
    /// 同时让任何按扩展名扫描的清理逻辑都不会把它当成正式产物。
    fn temp_sibling(path: &Path) -> Option<std::path::PathBuf> {
        let parent = path.parent()?;
        let name = path.file_name()?.to_string_lossy().to_string();
        Some(parent.join(format!(".{}.part", name)))
    }

    /// 实际写盘（不经临时文件）。剪映目录导出与临时文件路径都走这里。
    fn write_in_place(
        segments: &[Segment],
        path: &Path,
        format: &str,
        mode: ExportMode,
    ) -> Result<()> {
        // 工程文件（剪映 / FCPXML / Premiere）同样认「导出模式」：原文 / 仅译文 / 双语，
        // 有译文时压成一行。此前这三个分支只写原文、直接丢弃译文，用户在剪辑软件里
        // 根本看不到已译好的字幕。
        match format.to_lowercase().as_str() {
            "srt" => Self::write_srt_with_mode(segments, path, mode),
            "ass" => Self::write_ass_with_mode(segments, path, mode),
            "vtt" => Self::write_vtt_with_mode(segments, path, mode),
            "txt" => Self::write_txt_with_mode(segments, path, mode),
            "fcpxml" => super::fcpxml::FcpXmlExporter::write_to_file(segments, path, "Voice2Word Subtitles", mode),
            "xml" | "premiere" => super::premiere::PremiereXmlExporter::write_to_file(segments, path, "Voice2Word Subtitles", mode),
            "jianying" => {
                let _ = super::jianying::JianYingExporter::export_to_folder(segments, None, path, "Voice2Word Draft", mode)?;
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
    // ─────────── 导出原子性 ───────────

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("v2w_export_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample() -> Vec<Segment> {
        vec![
            Segment::new(1, 0.0, 1.5, "第一句"),
            Segment::new(2, 1.5, 3.0, "第二句"),
        ]
    }

    /// 正常导出：内容正确，且**不留下**临时文件。
    #[test]
    fn export_writes_content_and_leaves_no_temp_file() {
        let dir = tmp_dir("ok");
        let out = dir.join("out.srt");
        SubtitleWriter::write_to_file(&sample(), &out, "srt").expect("导出应成功");

        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("第一句"), "内容应写入: {text}");
        assert!(text.contains("-->"), "应含时间轴");

        // 目录里只应有目标文件，没有 .part 残留
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("part"))
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 覆盖导出：目标已存在时被完整替换（这是最常见的用法——改完字幕重导一次）。
    #[test]
    fn export_overwrites_existing_file_completely() {
        let dir = tmp_dir("overwrite");
        let out = dir.join("out.srt");
        // 先放一个「旧的长文件」，若新内容比它短，非原子实现会留下尾巴
        std::fs::write(&out, "旧内容\n".repeat(500)).unwrap();

        SubtitleWriter::write_to_file(&sample(), &out, "srt").expect("覆盖导出应成功");

        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("旧内容"), "旧内容必须被完全替换，不能留尾巴");
        assert!(text.contains("第一句"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出失败时**不能破坏已存在的目标文件**。
    ///
    /// 这是原子写要解决的核心问题：旧实现直接 `File::create(target)`，
    /// 一上来就把用户的字幕文件截断成 0 字节，之后失败就只剩半截内容。
    /// 这里用「不支持的格式」触发失败（它在写盘**之前**就返回错误），
    /// 验证目标文件原封不动。
    #[test]
    fn failed_export_preserves_existing_target() {
        let dir = tmp_dir("fail");
        let out = dir.join("out.srt");
        let original = "用户原有的字幕内容，不能被导出失败毁掉\n";
        std::fs::write(&out, original).unwrap();

        let err = SubtitleWriter::write_to_file(&sample(), &out, "不存在的格式");
        assert!(err.is_err(), "未知格式应报错");

        let after = std::fs::read_to_string(&out).unwrap();
        assert_eq!(after, original, "导出失败后原文件必须一字不变");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 各种格式都能导出且不留临时文件（覆盖 srt/ass/vtt/txt/fcpxml/xml 六个分支）。
    #[test]
    fn all_single_file_formats_export_cleanly() {
        for fmt in ["srt", "ass", "vtt", "txt", "fcpxml", "xml"] {
            let dir = tmp_dir(&format!("fmt_{fmt}"));
            let out = dir.join(format!("out.{fmt}"));
            SubtitleWriter::write_to_file(&sample(), &out, fmt)
                .unwrap_or_else(|e| panic!("{fmt} 导出失败: {e}"));
            assert!(out.exists(), "{fmt} 应产出文件");
            let leftovers: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.contains("part"))
                .collect();
            assert!(leftovers.is_empty(), "{fmt} 留下了临时文件: {leftovers:?}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
