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

    /// 导出 Premiere Pro XML (xmeml) 文件（使用默认 30fps / 1920×1080）。
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

    /// 导出 Premiere Pro XML，画布与帧率取源媒体探测结果。
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

        let mut file = File::create(path).with_context(|| "创建 Premiere XML 文件失败")?;

        // xmeml 的 timebase 是整数：29.97 写成 timebase=30 + ntsc=TRUE，帧号仍按
        // 30000/1001 的真实帧率计算，否则 NTSC 素材会逐句累积漂移。
        let timebase = format.fps.round().max(1.0) as u32;
        let ntsc = (format.fps - f64::from(timebase)).abs() > 0.01;
        let ntsc_label = if ntsc { "TRUE" } else { "FALSE" };
        let frame_fps = if ntsc {
            f64::from(timebase) * 1000.0 / 1001.0
        } else {
            f64::from(timebase)
        };
        let total_dur = crate::subtitle::timeline_end_or(segments, 1.0).max(1.0);
        // 总帧与段帧同口径（round）：此前总帧 ceil、段帧 round，最后一段的
        // `end_frame`（round，且至少 start+1）可能越过 ceil 出来的总帧，Premiere
        // 会把越界的那帧截掉。总帧取「round(总时长) 与最大段末帧」的较大者：
        // 段帧恒 ≤ 总帧，且不因 ceil 平白多出 1 帧的空尾。
        let frame_of = |secs: f64| (secs * frame_fps).round() as i64;
        let total_frames = segments
            .iter()
            .map(|seg| {
                let start_frame = frame_of(seg.start);
                frame_of(seg.end).max(start_frame + 1)
            })
            .max()
            .unwrap_or(0)
            .max(frame_of(total_dur))
            .max(1);
        let escaped_project_name = Self::escape_xml(project_name);

        writeln!(file, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(file, r#"<!DOCTYPE xmeml>"#)?;
        writeln!(file, r#"<xmeml version="4">"#)?;
        writeln!(file, r#"  <sequence id="sequence-1">"#)?;
        writeln!(file, r#"    <name>{}</name>"#, escaped_project_name)?;
        writeln!(file, r#"    <duration>{}</duration>"#, total_frames)?;
        writeln!(file, r#"    <rate>"#)?;
        writeln!(file, r#"      <timebase>{timebase}</timebase>"#)?;
        writeln!(file, r#"      <ntsc>{ntsc_label}</ntsc>"#)?;
        writeln!(file, r#"    </rate>"#)?;
        writeln!(file, r#"    <media>"#)?;
        writeln!(file, r#"      <video>"#)?;
        writeln!(file, r#"        <format>"#)?;
        writeln!(file, r#"          <samplecharacteristics>"#)?;
        writeln!(file, r#"            <width>{}</width>"#, format.width)?;
        writeln!(file, r#"            <height>{}</height>"#, format.height)?;
        writeln!(
            file,
            r#"            <pixelaspectratio>square</pixelaspectratio>"#
        )?;
        writeln!(file, r#"            <rate>"#)?;
        writeln!(file, r#"              <timebase>{timebase}</timebase>"#)?;
        writeln!(file, r#"              <ntsc>{ntsc_label}</ntsc>"#)?;
        writeln!(file, r#"            </rate>"#)?;
        writeln!(file, r#"          </samplecharacteristics>"#)?;
        writeln!(file, r#"        </format>"#)?;
        writeln!(file, r#"        <track>"#)?;

        for (i, seg) in segments.iter().enumerate() {
            let start_frame = frame_of(seg.start);
            let end_frame = frame_of(seg.end).max(start_frame + 1);
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

    /// 帧率取探测结果：29.97 → timebase 30 + NTSC 标记，尺寸取源媒体。
    #[test]
    fn format_follows_probed_media() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "句子")];
        let temp = std::env::temp_dir().join(format!("v2w_pr_fmt_{}.xml", std::process::id()));
        PremiereXmlExporter::write_to_file_with_format(
            &segs,
            &temp,
            "竖屏",
            ExportMode::RawOnly,
            crate::subtitle::ProjectFormat::new(29.97, 1080, 1920),
        )
        .unwrap();
        let text = std::fs::read_to_string(&temp).unwrap();
        assert!(text.contains("<timebase>30</timebase>"), "{text}");
        assert!(
            text.contains("<ntsc>TRUE</ntsc>"),
            "29.97 必须标 NTSC: {text}"
        );
        assert!(text.contains("<width>1080</width>"), "{text}");
        assert!(text.contains("<height>1920</height>"), "{text}");
        let _ = std::fs::remove_file(temp);
    }

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

    /// 回归：乱序 segments 的序列总帧必须覆盖到**最大**段末帧（旧实现取
    /// `last().end`，乱序时末句被截）；且段帧恒 ≤ 总帧（旧实现总帧 ceil、
    /// 段帧 round，最后一段可能越界）。
    #[test]
    fn premiere_total_frames_covers_all_segments_in_any_order() {
        // 场景 A（乱序）：最大 end 的句子**不在末位**——旧实现 `last().end` 只有 20.5s。
        let segs = vec![
            Segment::new(1, 5.0, 8.4, "第二句"),
            Segment::new(2, 0.0, 31.49, "真正最长的句子"),
            Segment::new(3, 20.0, 20.5, "末位不是最晚的短句"),
        ];
        let temp_file = std::env::temp_dir().join("test_pr_unordered.xml");
        PremiereXmlExporter::write_to_file(&segs, &temp_file, "乱序PR", ExportMode::RawOnly)
            .unwrap();
        let content = std::fs::read_to_string(&temp_file).unwrap();
        let total_frames = sequence_duration(&content);
        assert!(
            total_frames >= 945,
            "乱序输入的总帧过短（{total_frames}），末尾会被 Premiere 裁掉\
             （round(31.49*30)=945）"
        );
        let _ = std::fs::remove_file(temp_file);

        // 场景 B（帧边界）：末句是 31.49~31.50s 的亚帧段——段帧 = max(round, start+1)
        // = 946，而旧总帧 ceil(31.5*30) = 945，段末帧越过总帧。
        let segs = vec![
            Segment::new(1, 0.0, 1.0, "开头"),
            Segment::new(2, 31.49, 31.5, "贴边短尾"),
        ];
        let temp_file = std::env::temp_dir().join("test_pr_edge.xml");
        PremiereXmlExporter::write_to_file(&segs, &temp_file, "贴边PR", ExportMode::RawOnly)
            .unwrap();
        let content = std::fs::read_to_string(&temp_file).unwrap();
        let total_frames = sequence_duration(&content);
        for end_tag in content.split("<end>").skip(1) {
            let end_frame: i64 = end_tag
                .split('<')
                .next()
                .and_then(|s| s.trim().parse().ok())
                .unwrap();
            assert!(
                end_frame <= total_frames,
                "段末帧 {end_frame} 越过总帧 {total_frames}"
            );
        }
        let _ = std::fs::remove_file(temp_file);
    }

    /// 取 xmeml 序列头部的 `<duration>`（第一个出现的即序列时长，其后是各片段的）。
    fn sequence_duration(content: &str) -> i64 {
        content
            .split("<duration>")
            .nth(1)
            .and_then(|s| s.split('<').next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap()
    }
}
