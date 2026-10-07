//! TTML 系导出器：EBU-TT-D 与 Netflix TTAL
//!
//! 两者都是 **TTML (Timed Text Markup Language)** 的具体配置档（profile），共享同一套
//! 文档骨架，只在根元素属性与元数据上分叉。因此这里用一份实现 + 一个 [`TtmlProfile`]
//! 枚举承载，而不是复制两份 95% 相同的代码——未来新增（如 IMSC1.1、SCC 的 XML 侧）
//! 只要再加一个 profile 分支。
//!
//! # 为什么选 TTML 而不是 EBU-STL
//!
//! 路线图里「EBU-STL」指的是 Tech 3264 那份**二进制**格式，它的字符表只有
//! Latin / Cyrillic / Arabic / Greek / Hebrew 五套（CCT 00–04），**没有中文**。
//! 本项目的主力内容是中文，硬写 EBU-STL 只会把汉字降级成 `?`——「导出成功、
//! 内容报废」是比报错更糟的结果。TTML 家族是 UTF-8，中文、日文、emoji 原样保留，
//! 同时被 Netflix / 欧洲广播联盟 / 各大剪辑与审片系统接受，是当下更正确的选择。
//! EBU-STL 待明确「中文场景的字符表策略」后再补。
//!
//! # 时间基准
//!
//! `ttp:timeBase="media"`：时间从 0 开始、以媒体时间计，不受节目始发时间码（TCP）影响。
//! 时间串用 `HH:MM:SS.mmm`，与 SRT 的毫秒精度对齐。

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::segment::{ExportMode, Segment};
use super::xml_util::{escape_attr, escape_text};

/// TTML 配置档。不同档位在根属性、元数据与样式强度上要求不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtmlProfile {
    /// EBU-TT-D（欧洲广播联盟，`ebuttm` 元数据，适合广播／审片分发）
    EbuTtD,
    /// Netflix TTAL（Timed Text Authoring Lineage，`ttp:contentProfiles` 声明）
    NetflixTtal,
}

impl TtmlProfile {
    /// 档位标识（写进 `ttp:contentProfiles` / `ebuttm` 元数据，也用于文件名后缀）。
    pub fn id(self) -> &'static str {
        match self {
            Self::EbuTtD => "EBU-TT-D",
            Self::NetflixTtal => "Netflix-TTAL",
        }
    }

    /// XML 根 `<tt>` 上的档位专属属性（`xml:lang` 由调用方补）。
    fn root_attrs(self) -> String {
        match self {
            Self::EbuTtD => concat!(
                "ttp:timeBase=\"media\" ",
                "ttp:cellResolution=\"32 15\" ",
                "ebuttm:conformsToStandard=\"urn:ebu:tt:distribution:2018-04\""
            )
            .to_string(),
            Self::NetflixTtal => concat!(
                "ttp:timeBase=\"media\" ",
                "ttp:cellResolution=\"32 15\" ",
                "ttp:contentProfiles=\"http://www.netflix.com/ttml/profile/ntflx-ttal-1.0\""
            )
            .to_string(),
        }
    }
}

pub struct TtmlExporter;

impl TtmlExporter {
    /// 写出一份 TTML 文档。
    ///
    /// `doc_lang` 是文档主语言（如 `zh`）；双语（[`ExportMode::Bilingual`]）时译文
    /// 会包一层 `<span xml:lang="…">`，让读稿系统知道这是另一种语言，而不是同一段中文
    /// 的一部分。`target_lang` 为空时双语退化为单语，不会写出空的 `xml:lang`。
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        doc_lang: &str,
        target_lang: Option<&str>,
        mode: ExportMode,
        profile: TtmlProfile,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = File::create(path).with_context(|| "创建 TTML 文件失败")?;

        let lang = if doc_lang.trim().is_empty() { "zh" } else { doc_lang.trim() };
        let target = target_lang.map(str::trim).filter(|s| !s.is_empty());
        // 双语需要目标语言标记才有意义；没有就按单语走（不写半截 span）。
        let bilingual = mode == ExportMode::Bilingual && target.is_some();

        writeln!(file, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
        writeln!(
            file,
            r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" xmlns:ebuttm="urn:ebu:tt:metadata" xml:lang="{}" {}>"#,
            escape_attr(lang),
            profile.root_attrs()
        )?;
        // ── head：元数据 + 样式 + 布局 ──
        writeln!(file, "  <head>")?;
        writeln!(file, "    <metadata>")?;
        match profile {
            TtmlProfile::EbuTtD => {
                writeln!(file, "      <ebuttm:documentMetadata>")?;
                writeln!(file, "        <ebuttm:documentEbuttVersion>v1.0</ebuttm:documentEbuttVersion>")?;
                writeln!(file, "      </ebuttm:documentMetadata>")?;
            }
            TtmlProfile::NetflixTtal => {
                writeln!(file, "      <ttm:title xmlns:ttm=\"http://www.w3.org/ns/ttml#metadata\">Voice2Word</ttm:title>")?;
            }
        }
        writeln!(file, "    </metadata>")?;
        // 最小可用样式：白字、黑边、底部居中。CJK 字体优先，回落到通用 sansSerif。
        writeln!(file, "    <styling>")?;
        writeln!(
            file,
            "      <style xml:id=\"s0\" tts:fontFamily=\"Microsoft YaHei, PingFang SC, sansSerif\" tts:fontSize=\"100%\" tts:textAlign=\"center\" tts:color=\"#FFFFFF\" tts:textOutline=\"#000000 2px 0px\" />"
        )?;
        writeln!(
            file,
            "      <style xml:id=\"s1\" tts:fontSize=\"80%\" tts:color=\"#FFFFFF\" />"
        )?;
        writeln!(file, "    </styling>")?;
        writeln!(file, "    <layout>")?;
        writeln!(
            file,
            "      <region xml:id=\"bottom\" tts:origin=\"10% 85%\" tts:extent=\"80% 12%\" tts:displayAlign=\"after\" tts:overflow=\"visible\" />"
        )?;
        writeln!(file, "    </layout>")?;
        writeln!(file, "  </head>")?;

        // ── body ──
        writeln!(file, "  <body>")?;
        writeln!(file, "    <div>")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for (i, seg) in segments.iter().enumerate() {
            let begin = crate::utils::time::seconds_to_timestamp(seg.start);
            let end = crate::utils::time::seconds_to_timestamp(seg.end);
            // 段内文本一律单行：TTML 用 `<br/>` 表达换行，源文本里的裸换行必须先压平，
            // 否则会变成「无意义的空白字符」而非字幕换行。
            //
            // 双语**不能**直接复用 `export_text_for(Bilingual)`：那个函数返回的是
            // 「译文\n原文」两行合一的字符串，再包一层语言 span 会把译文写两遍。
            // 这里显式拼装：译文 span（标目标语言）+ `<br/>` + 原文（文档语言）。
            // 顺序与 SRT/ASS 双语一致（译文在前、原文在后），同一份内容换格式不颠倒。
            let body = if bilingual {
                let trans = seg
                    .translation
                    .as_deref()
                    .unwrap_or("")
                    .replace(['\r', '\n'], " ");
                let raw = super::segment::export_text_for(seg, ExportMode::RawOnly, with_speaker)
                    .replace(['\r', '\n'], " ");
                format!(
                    "<span xml:lang=\"{}\">{}</span><br/>{}",
                    escape_attr(target.unwrap_or_default()),
                    escape_text(&trans),
                    escape_text(&raw)
                )
            } else {
                escape_text(
                    &super::segment::export_text_for(seg, mode, with_speaker).replace(['\r', '\n'], " "),
                )
            };
            writeln!(
                file,
                "      <p xml:id=\"p{}\" begin=\"{}\" end=\"{}\" region=\"bottom\" style=\"s0\">{}</p>",
                i + 1,
                begin,
                end,
                body
            )?;
        }
        writeln!(file, "    </div>")?;
        writeln!(file, "  </body>")?;
        writeln!(file, "</tt>")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(i: usize, s: f64, e: f64, t: &str) -> Segment {
        Segment::new(i, s, e, t)
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("v2w_ttml_{}_{}.xml", name, std::process::id()))
    }

    #[test]
    fn ebu_tt_d_has_profile_attrs_and_metadata() {
        let segs = vec![seg(1, 0.0, 1.5, "第一句")];
        let p = tmp("ebu");
        TtmlExporter::write_to_file(&segs, &p, "zh", None, ExportMode::RawOnly, TtmlProfile::EbuTtD).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("ebuttm:conformsToStandard"), "{t}");
        assert!(t.contains("documentEbuttVersion"), "{t}");
        assert!(t.contains("xml:lang=\"zh\""), "{t}");
        assert!(t.contains("begin=\"00:00:00.000\""), "{t}");
        assert!(t.contains("end=\"00:00:01.500\""), "{t}");
        assert!(t.contains("第一句"));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn netflix_ttal_declares_content_profile() {
        let segs = vec![seg(1, 0.0, 1.0, "hi")];
        let p = tmp("ttal");
        TtmlExporter::write_to_file(&segs, &p, "en", None, ExportMode::RawOnly, TtmlProfile::NetflixTtal).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("contentProfiles"), "{t}");
        assert!(t.contains("ntflx-ttal-1.0"), "{t}");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn bilingual_wraps_translation_in_lang_span() {
        let mut s = seg(1, 0.0, 1.0, "你好");
        s.translation = Some("Hello".to_string());
        s.translation_lang = Some("English".to_string());
        let segs = vec![s];
        let p = tmp("bi");
        TtmlExporter::write_to_file(
            &segs, &p, "zh", Some("en"), ExportMode::Bilingual, TtmlProfile::EbuTtD,
        )
        .unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("<span xml:lang=\"en\">Hello</span>"), "{t}");
        assert!(t.contains("你好"), "{t}");
        // 回归：译文只能出现一次。早期实现把 `export_text_for(Bilingual)`（已含译文+原文）
        // 再包一层 span，导致译文在同一个 <p> 里写了两遍。
        assert_eq!(t.matches("Hello").count(), 1, "译文被重复写出: {t}");
        assert_eq!(t.matches("你好").count(), 1, "原文被重复写出: {t}");
        // 译文 span 在前、原文在后（与 SRT/ASS 双语顺序一致）
        let span_at = t.find("<span").unwrap();
        let raw_at = t.find("你好").unwrap();
        assert!(span_at < raw_at, "双语顺序应为译文在前: {t}");
        let _ = std::fs::remove_file(p);
    }

    /// 没有目标语言时双语必须退化为单语：不能写出 `xml:lang=""` 这种非法属性。
    #[test]
    fn bilingual_without_target_lang_degrades_cleanly() {
        let mut s = seg(1, 0.0, 1.0, "你好");
        s.translation = Some("Hello".to_string());
        let segs = vec![s];
        let p = tmp("nolang");
        TtmlExporter::write_to_file(
            &segs, &p, "zh", None, ExportMode::Bilingual, TtmlProfile::EbuTtD,
        )
        .unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(!t.contains("xml:lang=\"\""), "不能出现空的 xml:lang: {t}");
        assert!(!t.contains("<span"), "无目标语言时不应出现 span: {t}");
        let _ = std::fs::remove_file(p);
    }

    /// 文本里的 `&` `<` 必须转义，否则整份 TTML 解析失败。
    #[test]
    fn escapes_special_chars_in_text() {
        let segs = vec![seg(1, 0.0, 1.0, "A & B <tag>")];
        let p = tmp("esc");
        TtmlExporter::write_to_file(&segs, &p, "en", None, ExportMode::RawOnly, TtmlProfile::EbuTtD).unwrap();
        let t = std::fs::read_to_string(&p).unwrap();
        assert!(t.contains("A &amp; B &lt;tag&gt;"), "{t}");
        assert!(!t.contains("<tag>"), "原始尖括号不应泄漏: {t}");
        let _ = std::fs::remove_file(p);
    }
}
