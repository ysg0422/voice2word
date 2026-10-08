//! Adobe Premiere Pro XML (FCP7 XML / xmeml v4) 导出器
//! 支持 Adobe Premiere Pro 与 DaVinci Resolve 达芬奇

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment};

pub struct PremiereXmlExporter;

impl PremiereXmlExporter {
    /// 转义委托到共用的 [`super::xml_util::escape_attr`]：此处既用于属性（`name="…"`）
    /// 也用于文本节点，因此取「属性级」转义（含引号），并顺带剔除 XML 非法控制字符。
    fn escape_xml(s: &str) -> String {
        super::xml_util::escape_attr(s)
    }

    /// 导出 Premiere Pro XML (xmeml) 文件
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

        let mut file = File::create(path).with_context(|| "创建 Premiere XML 文件失败")?;

        let fps = 30.0;
        let total_dur = segments.last().map(|s| s.end).unwrap_or(1.0).max(1.0);
        let total_frames = (total_dur * fps).ceil() as i64;
        let escaped_project_name = Self::escape_xml(project_name);

        writeln!(file, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(file, r#"<!DOCTYPE xmeml>"#)?;
        writeln!(file, r#"<xmeml version="4">"#)?;
        writeln!(file, r#"  <sequence id="sequence-1">"#)?;
        writeln!(file, r#"    <name>{}</name>"#, escaped_project_name)?;
        writeln!(file, r#"    <duration>{}</duration>"#, total_frames)?;
        writeln!(file, r#"    <rate>"#)?;
        writeln!(file, r#"      <timebase>30</timebase>"#)?;
        writeln!(file, r#"      <ntsc>FALSE</ntsc>"#)?;
        writeln!(file, r#"    </rate>"#)?;
        writeln!(file, r#"    <media>"#)?;
        writeln!(file, r#"      <video>"#)?;
        writeln!(file, r#"        <format>"#)?;
        writeln!(file, r#"          <samplecharacteristics>"#)?;
        writeln!(file, r#"            <width>1920</width>"#)?;
        writeln!(file, r#"            <height>1080</height>"#)?;
        writeln!(
            file,
            r#"            <pixelaspectratio>square</pixelaspectratio>"#
        )?;
        writeln!(file, r#"            <rate>"#)?;
        writeln!(file, r#"              <timebase>30</timebase>"#)?;
        writeln!(file, r#"              <ntsc>FALSE</ntsc>"#)?;
        writeln!(file, r#"            </rate>"#)?;
        writeln!(file, r#"          </samplecharacteristics>"#)?;
        writeln!(file, r#"        </format>"#)?;
        writeln!(file, r#"        <track>"#)?;

        for (i, seg) in segments.iter().enumerate() {
            let start_frame = (seg.start * fps).round() as i64;
            let end_frame = ((seg.end * fps).round() as i64).max(start_frame + 1);
            let dur_frames = end_frame - start_frame;
            let text = seg.project_export_text(mode);
            let escaped_text = Self::escape_xml(&text);

            writeln!(
                file,
                r#"          <generatoritem id="generatoritem-{}">"#,
                i + 1
            )?;
            writeln!(file, r#"            <name>{}</name>"#, escaped_text)?;
            writeln!(file, r#"            <duration>{}</duration>"#, dur_frames)?;
            writeln!(file, r#"            <rate>"#)?;
            writeln!(file, r#"              <timebase>30</timebase>"#)?;
            writeln!(file, r#"              <ntsc>FALSE</ntsc>"#)?;
            writeln!(file, r#"            </rate>"#)?;
            writeln!(file, r#"            <start>{}</start>"#, start_frame)?;
            writeln!(file, r#"            <end>{}</end>"#, end_frame)?;
            writeln!(file, r#"            <in>0</in>"#)?;
            writeln!(file, r#"            <out>{}</out>"#, dur_frames)?;
            writeln!(file, r#"            <effect>"#)?;
            writeln!(file, r#"              <name>Text</name>"#)?;
            writeln!(file, r#"              <effectid>Text</effectid>"#)?;
            writeln!(
                file,
                r#"              <effectcategory>Text</effectcategory>"#
            )?;
            writeln!(file, r#"              <effecttype>generator</effecttype>"#)?;
            writeln!(file, r#"              <mediatype>video</mediatype>"#)?;
            writeln!(file, r#"              <parameter>"#)?;
            writeln!(file, r#"                <parameterid>str</parameterid>"#)?;
            writeln!(file, r#"                <name>Text</name>"#)?;
            writeln!(file, r#"                <value>{}</value>"#, escaped_text)?;
            writeln!(file, r#"              </parameter>"#)?;
            writeln!(file, r#"              <parameter>"#)?;
            writeln!(file, r#"                <parameterid>font</parameterid>"#)?;
            writeln!(file, r#"                <name>Font</name>"#)?;
            writeln!(file, r#"                <value>Microsoft YaHei</value>"#)?;
            writeln!(file, r#"              </parameter>"#)?;
            writeln!(file, r#"              <parameter>"#)?;
            writeln!(
                file,
                r#"                <parameterid>fontsize</parameterid>"#
            )?;
            writeln!(file, r#"                <name>Size</name>"#)?;
            writeln!(file, r#"                <valuemin>0</valuemin>"#)?;
            writeln!(file, r#"                <valuemax>1000</valuemax>"#)?;
            writeln!(file, r#"                <value>48</value>"#)?;
            writeln!(file, r#"              </parameter>"#)?;
            writeln!(file, r#"            </effect>"#)?;
            writeln!(file, r#"          </generatoritem>"#)?;
        }

        writeln!(file, r#"        </track>"#)?;
        writeln!(file, r#"      </video>"#)?;
        writeln!(file, r#"    </media>"#)?;
        writeln!(file, r#"  </sequence>"#)?;
        writeln!(file, r#"</xmeml>"#)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_premiere_xml_export() {
        let segs = vec![Segment::new(1, 1.0, 3.5, "测试 PR XML 序列导出")];

        let temp_file = std::env::temp_dir().join("test_pr.xml");
        let res = PremiereXmlExporter::write_to_file(
            &segs,
            &temp_file,
            "测试PR序列",
            ExportMode::RawOnly,
        );
        assert!(res.is_ok());
        assert!(temp_file.exists());

        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert!(content.contains("<xmeml version=\"4\">"));
        assert!(content.contains("<name>测试PR序列</name>"));
        assert!(content.contains("<generatoritem id=\"generatoritem-1\">"));

        let _ = std::fs::remove_file(temp_file);
    }

    /// 双语导出：译文必须真的写进 xmeml，而不是只写原文。
    #[test]
    fn premiere_includes_translation_when_bilingual() {
        let mut seg = Segment::new(1, 1.0, 3.5, "你好世界");
        seg.translation = Some("Hello world".to_string());
        seg.translation_lang = Some("English".to_string());
        let segs = vec![seg];

        let temp_file = std::env::temp_dir().join("test_pr_bi.xml");
        PremiereXmlExporter::write_to_file(&segs, &temp_file, "双向PR工程", ExportMode::Bilingual)
            .unwrap();
        let content = std::fs::read_to_string(&temp_file).unwrap();
        assert!(content.contains("你好世界"), "缺原文");
        assert!(content.contains("Hello world"), "双语导出丢了译文");
        let _ = std::fs::remove_file(temp_file);

        let temp2 = std::env::temp_dir().join("test_pr_raw.xml");
        PremiereXmlExporter::write_to_file(&segs, &temp2, "单向PR工程", ExportMode::RawOnly)
            .unwrap();
        let content2 = std::fs::read_to_string(&temp2).unwrap();
        assert!(!content2.contains("Hello world"), "关闭双语时不应出现译文");
        let _ = std::fs::remove_file(temp2);
    }
}
