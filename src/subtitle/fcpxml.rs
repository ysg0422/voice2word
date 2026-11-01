//! Final Cut Pro XML (FCPXML 1.9) 导出器
//! 支持 Final Cut Pro X 与 DaVinci Resolve 达芬奇

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment};

pub struct FcpXmlExporter;

/// 常见帧率 → (分子, 分母)，帧时长 = 分母/分子 秒。
///
/// 取「最接近的常见广播帧率」而不是直接 `1/fps`：29.97 的精确值是 30000/1001，
/// 写成 `1/29.97s` 会让 FCP 判定为非法帧时长并整段重算。
fn fps_rational(fps: f64) -> (u32, u32) {
    const CANDIDATES: [(u32, u32); 9] = [
        (24000, 1001),
        (30000, 1001),
        (60000, 1001),
        (24, 1),
        (25, 1),
        (30, 1),
        (50, 1),
        (60, 1),
        (120, 1),
    ];
    let mut best = (30, 1);
    let mut best_err = f64::MAX;
    for (num, den) in CANDIDATES {
        let err = (num as f64 / den as f64 - fps).abs();
        if err < best_err {
            best = (num, den);
            best_err = err;
        }
    }
    best
}

impl FcpXmlExporter {
    /// 转义委托到共用的 [`super::xml_util::escape_attr`]：此处既用于属性（`name="…"`）
    /// 也用于文本节点，因此取「属性级」转义（含引号），并顺带剔除 XML 非法控制字符。
    fn escape_xml(s: &str) -> String {
        super::xml_util::escape_attr(s)
    }

    /// 导出 FCPXML 文件（使用默认 30fps / 1920×1080）。
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        project_name: &str,
        mode: ExportMode,
    ) -> Result<()> {
        Self::write_to_file_with_format(
            segments,
            path,
            project_name,
            mode,
            super::ProjectFormat::default(),
        )
    }

    /// 导出 FCPXML 文件，画布与帧率取源媒体探测结果。
    pub fn write_to_file_with_format<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        project_name: &str,
        mode: ExportMode,
        format: super::ProjectFormat,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = File::create(path).with_context(|| "创建 FCPXML 文件失败")?;

        let total_dur = crate::subtitle::timeline_end_or(segments, 1.0).max(1.0);
        let escaped_project_name = Self::escape_xml(project_name);
        // FCPXML 的 frameDuration 是「一帧的时长」（1/fps），因此分子分母要倒过来；
        // 29.97 / 23.976 这类广播帧率必须写成 1001/30000s 的有理数，FCP 不接受
        // 三位小数秒。
        let (fps_num, fps_den) = fps_rational(format.fps);
        // FCP 的格式名按 FCP 的习惯写：整数帧率用 `30`，广播帧率用百分之一
        // （29.97 → `2997`），与服务端识别的 FFVideoFormat1080p2997 一致。
        let fps_label = if fps_den == 1 {
            fps_num.to_string()
        } else {
            ((fps_num as f64 / fps_den as f64 * 100.0).round() as u32).to_string()
        };

        writeln!(file, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(file, r#"<!DOCTYPE fcpxml>"#)?;
        writeln!(file, r#"<fcpxml version="1.9">"#)?;
        writeln!(file, r#"    <resources>"#)?;
        writeln!(
            file,
            r#"        <format id="r1" name="FFVideoFormat{}p{}" frameDuration="{}/{}s" width="{}" height="{}"/>"#,
            format.height, fps_label, fps_den, fps_num, format.width, format.height
        )?;
        writeln!(
            file,
            r#"        <effect id="r2" name="Basic Title" uid=".../Titles.localized/Bumper:Opener.localized/Basic Title.localized/Basic Title.moti"/>"#
        )?;
        writeln!(file, r#"    </resources>"#)?;
        writeln!(file, r#"    <library>"#)?;
        writeln!(file, r#"        <event name="Voice2Word">"#)?;
        writeln!(
            file,
            r#"            <project name="{}">"#,
            escaped_project_name
        )?;
        writeln!(
            file,
            r#"                <sequence format="r1" duration="{:.3}s">"#,
            total_dur
        )?;
        writeln!(file, r#"                    <spine>"#)?;
        writeln!(
            file,
            r#"                        <gap name="Gap" offset="0s" duration="{:.3}s" start="0s">"#,
            total_dur
        )?;

        for (i, seg) in segments.iter().enumerate() {
            let start = seg.start;
            let dur = (seg.end - seg.start).max(0.1);
            let text = seg.project_export_text(mode);
            let escaped_text = Self::escape_xml(&text);
            let ts_id = format!("ts{}", i + 1);

            writeln!(
                file,
                r#"                            <title ref="r2" offset="{:.3}s" duration="{:.3}s" start="0s" name="{}">"#,
                start, dur, escaped_text
            )?;
            writeln!(file, r#"                                <text>"#)?;
            writeln!(
                file,
                r#"                                    <text-style ref="{}">{}</text-style>"#,
                ts_id, escaped_text
            )?;
            writeln!(file, r#"                                </text>"#)?;
            writeln!(
                file,
                r#"                                <text-style-def id="{}">"#,
                ts_id
            )?;
            writeln!(
                file,
                r#"                                    <text-style font="PingFang SC" fontSize="48" fontColor="1 1 1 1" alignment="center"/>"#
            )?;
            writeln!(file, r#"                                </text-style-def>"#)?;
            writeln!(file, r#"                            </title>"#)?;
        }

        writeln!(file, r#"                        </gap>"#)?;
        writeln!(file, r#"                    </spine>"#)?;
        writeln!(file, r#"                </sequence>"#)?;
        writeln!(file, r#"            </project>"#)?;
        writeln!(file, r#"        </event>"#)?;
        writeln!(file, r#"    </library>"#)?;
        writeln!(file, r#"</fcpxml>"#)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧率取探测结果：29.97 必须写成 1001/30000s 的有理数，尺寸取源媒体。
    #[test]
    fn format_follows_probed_media() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "句子")];
        let temp = std::env::temp_dir().join(format!("v2w_fcp_fmt_{}.fcpxml", std::process::id()));
        FcpXmlExporter::write_to_file_with_format(
            &segs,
            &temp,
            "竖屏",
            ExportMode::RawOnly,
            crate::subtitle::ProjectFormat::new(29.97, 1080, 1920),
        )
        .unwrap();
        let text = std::fs::read_to_string(&temp).unwrap();
        assert!(
            text.contains(r#"frameDuration="1001/30000s""#),
            "29.97 应写成有理数帧时长: {text}"
        );
        assert!(text.contains(r#"width="1080""#), "{text}");
        assert!(text.contains(r#"height="1920""#), "{text}");
        let _ = std::fs::remove_file(temp);
    }

    #[test]
    fn test_fcpxml_export() {
        let segs = vec![Segment::new(1, 0.5, 2.0, "测试 FCPXML 字幕 & 符号 <转义>")];

        let temp_file = std::env::temp_dir().join("test_fcpxml.fcpxml");
        let res = FcpXmlExporter::write_to_file(&segs, &temp_file, "测试工程", ExportMode::RawOnly);
        assert!(res.is_ok());
        assert!(temp_file.exists());

        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert!(content.contains("<fcpxml version=\"1.9\">"));
        assert!(content.contains("&amp;"));
        assert!(content.contains("&lt;转义&gt;"));

        let _ = std::fs::remove_file(temp_file);
    }

    /// 双语导出：译文必须真的写进 FCPXML，而不是只写原文（回归「工程文件丢译文」）。
    #[test]
    fn fcpxml_includes_translation_when_bilingual() {
        let mut seg = Segment::new(1, 0.5, 2.0, "你好世界");
        seg.translation = Some("Hello world".to_string());
        seg.translation_lang = Some("English".to_string());
        let segs = vec![seg];

        let temp_file = std::env::temp_dir().join("test_fcpxml_bi.fcpxml");
        FcpXmlExporter::write_to_file(&segs, &temp_file, "双向工程", ExportMode::Bilingual)
            .unwrap();
        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert!(content.contains("你好世界"), "缺原文");
        assert!(content.contains("Hello world"), "双语导出丢了译文");
        let _ = std::fs::remove_file(temp_file);

        // 关闭双语 = 只写原文
        let temp2 = std::env::temp_dir().join("test_fcpxml_raw.fcpxml");
        FcpXmlExporter::write_to_file(&segs, &temp2, "单向工程", ExportMode::RawOnly).unwrap();
        let content2 = std::fs::read_to_string(&temp2).unwrap();
        assert!(!content2.contains("Hello world"), "关闭双语时不应出现译文");
        let _ = std::fs::remove_file(temp2);
    }

    /// 回归：乱序 segments 的 sequence/gap 时长必须覆盖到**最大** end
    /// （旧实现取 `last().end`，乱序时工程时长偏短、末句被裁）。
    /// 注意「最大 end 的段**不在末位**」才是有效哨兵——放末位旧实现也能过。
    #[test]
    fn fcpxml_duration_covers_max_end_in_any_order() {
        let segs = vec![
            Segment::new(1, 0.0, 12.5, "真正最长的尾巴（不在末位）"),
            Segment::new(2, 5.0, 8.0, "末位短句"),
        ];
        let temp_file = std::env::temp_dir().join("test_fcpxml_unordered.fcpxml");
        FcpXmlExporter::write_to_file(&segs, &temp_file, "乱序工程", ExportMode::RawOnly).unwrap();
        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert!(
            content.contains(r#"duration="12.500s""#),
            "乱序输入的工程时长应取 max(end)=12.5（旧实现会取末位的 8.0）: {content}"
        );
        let _ = std::fs::remove_file(temp_file);
    }
}
