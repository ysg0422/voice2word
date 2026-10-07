//! Final Cut Pro XML (FCPXML 1.9) 导出器
//! 支持 Final Cut Pro X 与 DaVinci Resolve 达芬奇

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment};

pub struct FcpXmlExporter;

impl FcpXmlExporter {
    /// 转义委托到共用的 [`super::xml_util::escape_attr`]：此处既用于属性（`name="…"`）
    /// 也用于文本节点，因此取「属性级」转义（含引号），并顺带剔除 XML 非法控制字符。
    fn escape_xml(s: &str) -> String {
        super::xml_util::escape_attr(s)
    }

    /// 导出 FCPXML 文件
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        project_name: &str,
        mode: ExportMode,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = File::create(path).with_context(|| "创建 FCPXML 文件失败")?;

        let total_dur = segments.last().map(|s| s.end).unwrap_or(1.0).max(1.0);
        let escaped_project_name = Self::escape_xml(project_name);

        writeln!(file, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(file, r#"<!DOCTYPE fcpxml>"#)?;
        writeln!(file, r#"<fcpxml version="1.9">"#)?;
        writeln!(file, r#"    <resources>"#)?;
        writeln!(file, r#"        <format id="r1" name="FFVideoFormat1080p30" frameDuration="1/30s" width="1920" height="1080"/>"#)?;
        writeln!(file, r#"        <effect id="r2" name="Basic Title" uid=".../Titles.localized/Bumper:Opener.localized/Basic Title.localized/Basic Title.moti"/>"#)?;
        writeln!(file, r#"    </resources>"#)?;
        writeln!(file, r#"    <library>"#)?;
        writeln!(file, r#"        <event name="Voice2Word">"#)?;
        writeln!(file, r#"            <project name="{}">"#, escaped_project_name)?;
        writeln!(file, r#"                <sequence format="r1" duration="{:.3}s">"#, total_dur)?;
        writeln!(file, r#"                    <spine>"#)?;
        writeln!(file, r#"                        <gap name="Gap" offset="0s" duration="{:.3}s" start="0s">"#, total_dur)?;

        for (i, seg) in segments.iter().enumerate() {
            let start = seg.start;
            let dur = (seg.end - seg.start).max(0.1);
            let text = seg.project_export_text(mode);
            let escaped_text = Self::escape_xml(&text);
            let ts_id = format!("ts{}", i + 1);

            writeln!(file, r#"                            <title ref="r2" offset="{:.3}s" duration="{:.3}s" start="0s" name="{}">"#, start, dur, escaped_text)?;
            writeln!(file, r#"                                <text>"#)?;
            writeln!(file, r#"                                    <text-style ref="{}">{}</text-style>"#, ts_id, escaped_text)?;
            writeln!(file, r#"                                </text>"#)?;
            writeln!(file, r#"                                <text-style-def id="{}">"#, ts_id)?;
            writeln!(file, r#"                                    <text-style font="PingFang SC" fontSize="48" fontColor="1 1 1 1" alignment="center"/>"#)?;
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

    #[test]
    fn test_fcpxml_export() {
        let segs = vec![
            Segment::new(1, 0.5, 2.0, "测试 FCPXML 字幕 & 符号 <转义>"),
        ];

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
        FcpXmlExporter::write_to_file(&segs, &temp_file, "双向工程", ExportMode::Bilingual).unwrap();
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
}

