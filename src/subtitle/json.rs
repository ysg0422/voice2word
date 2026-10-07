//! JSON 字幕导出器（面向程序化消费 / 二次加工 / 质量评测）
//!
//! 与 SRT/ASS 这类「给人看」的格式不同，JSON 的目标是**无损、可编程**：任何自动化
//! 流程（术语库回填、CER/WER 评测、上传到 CMS、喂给另一套翻译）都希望拿到结构化的
//! 字段而不是再去解析时间轴文本。因此这里每条记录同时给出：
//!
//! - `text`：按当前[`ExportMode`]渲染后的文本（双语时为「原文\n译文」，与字幕文件一致）；
//! - `raw_text` / `translation`：**始终**分别给出原文与译文，不受导出模式裁剪；
//! - `confidence` / `speaker` / `language` / `translation_lang`：质检与说话人相关的元数据。
//!
//! 这样一份 JSON 既能直接当字幕用（读 `text`），又不会在任何导出模式下丢掉原始字段
//! ——「导出成功但信息被裁掉」是结构化导出最需要避免的坑。

use anyhow::{Context, Result};
use std::path::Path;

use super::segment::{ExportMode, Segment};

pub struct JsonSubtitleExporter;

impl JsonSubtitleExporter {
    /// 写出一份结构化 JSON 字幕。
    ///
    /// `source_file` 是源媒体文件名（可为空），`target_lang` 是译文目标语言（可为空）。
    pub fn write_to_file<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        source_file: Option<&str>,
        mode: ExportMode,
        target_lang: Option<&str>,
    ) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let with_speaker = super::segment::has_speaker_labels(segments);
        let items: Vec<serde_json::Value> = segments
            .iter()
            .map(|seg| {
                serde_json::json!({
                    "index": seg.index,
                    "start": seg.start,
                    "end": seg.end,
                    "duration": seg.duration(),
                    "text": super::segment::export_text_for(seg, mode, with_speaker),
                    "raw_text": seg.display_text(),
                    "translation": seg.translation,
                    "translation_lang": seg.translation_lang,
                    "language": seg.language,
                    "confidence": seg.confidence,
                    "speaker": seg.speaker,
                    "speaker_label": seg.speaker_label(),
                })
            })
            .collect();

        let total = segments.last().map(|s| s.end).unwrap_or(0.0);
        let doc = serde_json::json!({
            "format": "voice2word-json",
            "version": 1,
            "generator": concat!("Voice2Word ", env!("CARGO_PKG_VERSION")),
            "source_file": source_file,
            "export_mode": export_mode_id(mode),
            "target_lang": target_lang,
            "segment_count": segments.len(),
            "duration": total,
            "segments": items,
        });

        let file = std::fs::File::create(path)
            .with_context(|| format!("创建 JSON 文件失败: {}", path.display()))?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &doc)
            .with_context(|| format!("写出 JSON 失败: {}", path.display()))?;
        Ok(())
    }
}

/// 导出模式的稳定字符串标识（写进 JSON，供下游程序判断，不使用中文本地化名）。
fn export_mode_id(mode: ExportMode) -> &'static str {
    match mode {
        ExportMode::RawOnly => "raw",
        ExportMode::TranslationOnly => "translation",
        ExportMode::Bilingual => "bilingual",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("v2w_json_{}_{}.json", name, std::process::id()))
    }

    #[test]
    fn writes_lossless_structured_records() {
        let mut s = Segment::new(1, 0.0, 1.5, "你好");
        s.translation = Some("Hello".to_string());
        s.translation_lang = Some("English".to_string());
        s.confidence = Some(-0.42);
        s.language = Some("zh".to_string());
        s.speaker = Some(0);
        let segs = vec![s, Segment::new(2, 1.5, 2.0, "第二句")];

        let p = tmp("lossless");
        JsonSubtitleExporter::write_to_file(&segs, &p, Some("demo.mp4"), ExportMode::Bilingual, Some("English")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).expect("必须是合法 JSON");
        assert_eq!(v["segment_count"], 2);
        assert_eq!(v["segments"][0]["raw_text"], "你好");
        // 双语下 text 同时含原文与译文
        let t0 = v["segments"][0]["text"].as_str().unwrap();
        assert!(t0.contains("你好") && t0.contains("Hello"), "{t0}");
        // 结构化字段不因导出模式被裁掉
        assert_eq!(v["segments"][0]["translation"], "Hello");
        assert_eq!(v["segments"][0]["confidence"], -0.42);
        assert_eq!(v["segments"][0]["speaker"], 0);
        let _ = std::fs::remove_file(p);
    }

    /// 即使选「仅原文」，JSON 里仍应保留 translation 字段（无损）。
    #[test]
    fn translation_survives_raw_only_mode() {
        let mut s = Segment::new(1, 0.0, 1.0, "你好");
        s.translation = Some("Hello".to_string());
        let p = tmp("rawonly");
        JsonSubtitleExporter::write_to_file(&[s], &p, None, ExportMode::RawOnly, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["segments"][0]["translation"], "Hello");
        assert_eq!(v["segments"][0]["text"], "你好");
        assert_eq!(v["export_mode"], "raw");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn empty_segments_still_produces_valid_document() {
        let p = tmp("empty");
        JsonSubtitleExporter::write_to_file(&[], &p, None, ExportMode::RawOnly, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["segment_count"], 0);
        assert!(v["segments"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_file(p);
    }
}
