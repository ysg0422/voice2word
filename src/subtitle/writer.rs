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
        Self::write_atomic(path, |target| {
            Self::write_in_place(segments, target, format, mode)
        })
    }

    /// ── 原子写盘：先写同目录临时文件，全部成功后再 rename 覆盖目标 ──
    ///
    /// 原先各格式直接 `File::create(path)`：这会**立刻把目标文件截断为 0**，
    /// 然后才逐行写入。于是导出过程中任何失败（磁盘满、用户中途拔盘、
    /// 字符串格式化出错）都会把用户**原有的字幕文件**毁掉，只剩半截内容——
    /// 而「覆盖导出」恰恰是最常见的使用方式（改完字幕重新导一次）。
    ///
    /// 改为：写到同目录的 `.part`，全部写完并 flush 成功后才 rename 覆盖。
    /// rename 在同一目录内是原子的（Windows 上用 MoveFileEx 的替换语义，
    /// 由 `std::fs::rename` 提供），因此目标文件要么是旧内容、要么是完整新内容。
    ///
    /// 剪映导出是个例外：它产出的是**整个草稿目录**（多个 json 文件），
    /// 不是单文件，无法用单次 rename 表达原子性，因此不走这里。
    fn write_atomic<F>(path: &Path, write: F) -> Result<()>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp_path = match Self::temp_sibling(path) {
            Some(p) => p,
            None => {
                // 没有父目录（极端路径）时退化为直接写，至少不 panic
                return write(path);
            }
        };

        match write(&tmp_path) {
            Ok(()) => {
                if let Err(err) = std::fs::rename(&tmp_path, path) {
                    // 收尾失败要把临时文件清掉，否则用户目录里会留下一堆 .part
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(err)
                        .with_context(|| format!("替换字幕文件失败: {}", path.display()));
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
    /// 用 `.part` 后缀而不是前缀，是为了不与「用户自己命名的 .part 文件」冲突，
    /// 同时让任何按扩展名扫描的清理逻辑都不会把它当成正式产物。
    ///
    /// 名字里再带上 **pid + 本次调用序号**：两个实例（或将来引入并发导出）同时写
    /// 同一个目标时，各自写各自的临时文件，不会交叉写入同一个 `.part`——否则两边
    /// 的内容会交替落进同一个文件，谁先 rename，谁就把**拼接出来的半截内容**当成
    /// 成品交给用户。加序号后最坏情形退化为「后写者覆盖先写者」的完整文件。
    fn temp_sibling(path: &Path) -> Option<std::path::PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let parent = path.parent()?;
        let name = path.file_name()?.to_string_lossy().to_string();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        Some(parent.join(format!(".{}.{}-{}.part", name, std::process::id(), seq)))
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
            "fcpxml" => super::fcpxml::FcpXmlExporter::write_to_file(
                segments,
                path,
                "Voice2Word Subtitles",
                mode,
            ),
            "xml" | "premiere" => super::premiere::PremiereXmlExporter::write_to_file(
                segments,
                path,
                "Voice2Word Subtitles",
                mode,
            ),
            // ── 专业 / 结构化格式（P1-10）──
            // 这里从容地从字幕本身**推断**文档语言与目标语言，而不是让调用方逐个传参：
            // 管线（core::pipeline）与剪辑台导出走的是同一个 `write_to_file`，逼它们
            // 各自带上语言上下文只会增加漏传的机会。语言推断的规则集中在
            // [`Self::infer_doc_lang`] / [`Self::infer_target_lang`] 两处，可测。
            "json" => {
                let doc_lang = Self::infer_doc_lang(segments);
                let target = Self::infer_target_lang(segments);
                super::json::JsonSubtitleExporter::write_to_file(
                    segments,
                    path,
                    None,
                    mode,
                    target.as_deref().or(Some(doc_lang.as_str())),
                )
            }
            "ttml" | "ebu-tt-d" | "ebutt" => {
                let doc_lang = Self::infer_doc_lang(segments);
                let target = Self::infer_target_lang(segments);
                super::ttml::TtmlExporter::write_to_file(
                    segments,
                    path,
                    &doc_lang,
                    target.as_deref(),
                    mode,
                    super::ttml::TtmlProfile::EbuTtD,
                )
            }
            "ttal" | "netflix-ttal" => {
                let doc_lang = Self::infer_doc_lang(segments);
                let target = Self::infer_target_lang(segments);
                super::ttml::TtmlExporter::write_to_file(
                    segments,
                    path,
                    &doc_lang,
                    target.as_deref(),
                    mode,
                    super::ttml::TtmlProfile::NetflixTtal,
                )
            }
            "jianying" => {
                let _ = super::jianying::JianYingExporter::export_to_folder(
                    segments,
                    None,
                    path,
                    "Voice2Word Draft",
                    mode,
                )?;
                Ok(())
            }
            other => anyhow::bail!("不支持的字幕格式: {}", other),
        }
    }

    /// 从字幕推断**文档主语言**（TTML `xml:lang` / JSON 的兜底）。
    ///
    /// 优先用 ASR 写进 `Segment.language` 的众数（[`super::segment::dominant_language`]）；
    /// 全部为空时退回 `"zh"`——本项目的主力内容语种，也是最无害的默认。
    fn infer_doc_lang(segments: &[Segment]) -> String {
        super::segment::dominant_language(segments).unwrap_or_else(|| "zh".to_string())
    }

    /// 从字幕推断**译文目标语言**（取已存在译文的众数）。
    ///
    /// 只认 `translation_lang` 非空且确实带译文的句子，避免把「旧译文残留」当成本次目标。
    /// 全部为空时返回 `None`（双语导出会据此安全退化为单语，而不是写出空的 `xml:lang`）。
    fn infer_target_lang(segments: &[Segment]) -> Option<String> {
        use std::collections::HashMap;
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for seg in segments {
            if !seg.has_translation() {
                continue;
            }
            if let Some(lang) = seg
                .translation_lang
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                *counts.entry(lang).or_insert(0) += 1;
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(l, _)| l.to_string())
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
        // 无样式入口：不折行（`wrap == 0`），与历史版本逐字节一致。
        Self::write_srt_inner(segments, path, mode, 0)
    }

    /// 带样式的 SRT 写出：按 [`SubtitleStyleConfig::max_chars_per_line`] 折行。
    ///
    /// 与 [`Self::write_to_file_with_style`] 的 SRT 分支**同源**（同一个
    /// [`Self::write_srt_inner`] + [`Self::write_atomic`]），但只做 SRT：
    /// 预览链路（ffplay 的 `subtitles` 滤镜只认 SRT）需要「调用方显式传样式」
    /// 这一语义，而 `write_to_file_with_style` 会按格式分派、还要处理非 SRT 格式。
    /// 本函数是**新增入口**，`write_srt` / `write_srt_with_mode` 的行为一字未动。
    ///
    /// 原子性：复用 [`Self::write_atomic`]（先写同目录 `.part`，全部成功后再
    /// rename 覆盖），折行只改变单条文本的行数，不影响「要么旧内容、要么完整
    /// 新内容」的保证。
    pub fn write_srt_with_style<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        style: &SubtitleStyleConfig,
    ) -> Result<()> {
        let path = path.as_ref();
        // `max_chars_per_line == 0` 显式表示「不折行」——判定与
        // `write_to_file_with_style` 完全一致，因此老配置 / 极端值行为不变。
        let wrap = style.max_chars_per_line as usize;
        Self::write_atomic(path, |target| {
            Self::write_srt_inner(segments, target, mode, wrap)
        })
    }

    /// SRT 写出。`wrap` 是单行最大字符数，`0` 表示不折行。
    /// 多行 cue 之间用 `\n` 分隔（SRT 规范允许一条字幕含多行文本）。
    fn write_srt_inner<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        wrap: usize,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 SRT 文件失败")?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for (i, seg) in segments.iter().enumerate() {
            let start = seconds_to_srt_time(seg.start);
            let end = seconds_to_srt_time(seg.end);
            // 折行在**转义之前**、在原始文本上做：先按字符数断行，再整体走实体转义，
            // 这样 `&`/`<` 变成的 `&amp;`/`&lt;` 不会被断行逻辑从中间劈开。
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            // 用户文本是直接拼进 SRT 的，出现在里面的 `<`/`&` 会被播放器当成标记
            // 解析（吞掉文本，甚至吃掉后面的字幕块），所以写盘前统一走实体转义。
            let text = escape_srt_text(&wrap_text(&raw, wrap));
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
        // 无样式入口：沿用默认样式表，但**不折行**（保持历史字节一致）。
        Self::write_ass_inner(segments, path, mode, &SubtitleStyleConfig::default(), 0)
    }

    /// 带样式的写出：ASS 用样式表（PlayResY 固定 1080，因此 `font_size` /
    /// `bottom_margin` 可与配置 1:1 对应）；SRT / VTT / ASS 还会按样式里的
    /// [`SubtitleStyleConfig::max_chars_per_line`] **折行**。其余格式委托原实现。
    ///
    /// **折行只发生在「调用方显式传入样式」的路径上。** `write_to_file` /
    /// `write_to_file_with_mode` 这类拿不到样式的入口保持旧行为（不折行），
    /// 否则隐式的默认值会改掉纯文本导出的字节（见
    /// `plain_text_export_is_byte_identical`）。
    pub fn write_to_file_with_style<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        format: &str,
        mode: ExportMode,
        style: &SubtitleStyleConfig,
    ) -> Result<()> {
        let path = path.as_ref();
        // `max_chars_per_line == 0` 显式表示「不折行」（与旧配置 / 极端值兼容）。
        let wrap = style.max_chars_per_line as usize;
        // SRT / VTT / ASS 都走**同一套原子写盘**（与 `write_to_file_with_mode` 一致），
        // 折行只是把单条文本切多行，不影响「要么旧内容、要么完整新内容」的保证。
        match format.to_lowercase().as_str() {
            "srt" => Self::write_atomic(path, |target| {
                Self::write_srt_inner(segments, target, mode, wrap)
            }),
            "vtt" => Self::write_atomic(path, |target| {
                Self::write_vtt_inner(segments, target, mode, wrap)
            }),
            "ass" => Self::write_atomic(path, |target| {
                Self::write_ass_inner(segments, target, mode, style, wrap)
            }),
            // TXT 是给人读 / 给下游喂的纯文本，折行会破坏「一行一句」，
            // 因此**明确不折**（`write_txt_with_mode` 本身也不折）。
            // 其余（json / ttml / ttal / fcpxml / premiere / jianying）由各自模块负责。
            _ => Self::write_to_file_with_mode(segments, path, format, mode),
        }
    }

    /// 按用户配置的字幕样式生成 ASS（并按 `max_chars_per_line` 折行）。
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
        // 有样式 = 用户显式要了排版，按配置折行。
        Self::write_ass_inner(
            segments,
            path,
            mode,
            style,
            style.max_chars_per_line as usize,
        )
    }

    /// ASS 写出的公共实现。`wrap` 为单行最大字符数（`0` = 不折行）。
    fn write_ass_inner<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        style: &SubtitleStyleConfig,
        wrap: usize,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 ASS 文件失败")?;
        let (preset_primary, preset_outline, preset_back, preset_border_style, preset_outline_w, preset_shadow) =
            ass_preset_colors(&style.preset_name);
        let default_cfg = SubtitleStyleConfig::default();
        let primary = if style.preset_name != "白字黑影" && style.primary_color == default_cfg.primary_color {
            preset_primary.to_string()
        } else {
            style.ass_primary_colour()
        };
        let outline = if style.preset_name != "白字黑影" && style.outline_color == default_cfg.outline_color {
            preset_outline.to_string()
        } else {
            style.ass_outline_colour()
        };
        let outline_w = if style.preset_name != "白字黑影" && (style.outline_width - default_cfg.outline_width).abs() < 0.01 {
            preset_outline_w
        } else {
            style.outline_width
        };
        let border_style = if style.preset_name != "白字黑影" && style.bg_style == default_cfg.bg_style {
            preset_border_style
        } else {
            match style.bg_style.as_str() {
                "box" | "pill" => 3,
                _ => 1,
            }
        };
        let bold_flag = if style.is_bold { 1 } else { 0 };
        let back = preset_back;
        let shadow = preset_shadow;
        let alignment = match style.alignment.as_str() {
            "left" => 1,
            "right" => 3,
            _ => 2,
        };

        let header = format!(
            r#"[Script Info]
Title: Voice2Word Subtitle
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
Timer: 100.0000

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Microsoft YaHei,{size},{primary},&H000000FF,{outline},{back},{bold_flag},0,0,0,100,100,{spacing},0,{border_style},{outline_w},{shadow},{alignment},20,20,{margin_v},1
"#,
            size = style.font_size,
            spacing = style.letter_spacing,
            margin_v = style.bottom_margin,
        );
        write!(file, "{}", header)?;

        // 说话人分色：有说话人标签时，为每个说话人生成一条独立 ASS 样式（不同主色），
        // Dialogue 行按说话人引用对应样式。多人访谈/对谈里一眼分清谁在说。
        // 颜色只改 PrimaryColour，其余（描边/阴影/边距）沿用当前预设，保持视觉一致。
        let mut speaker_styles: std::collections::BTreeMap<u32, String> =
            std::collections::BTreeMap::new();
        if super::segment::has_speaker_labels(segments) {
            for seg in segments {
                if let Some(spk) = seg.speaker {
                    speaker_styles
                        .entry(spk)
                        .or_insert_with(|| format!("Speaker{}", spk + 1));
                }
            }
            // 逐个写出说话人样式行
            for (idx, name) in speaker_styles.values().enumerate() {
                let color = speaker_ass_color(idx);
                writeln!(
                    file,
                    "Style: {name},Microsoft YaHei,{size},{color},&H000000FF,{outline},{back},{bold_flag},0,0,0,100,100,{spacing},0,{border_style},{outline_w},{shadow},{alignment},20,20,{margin_v},1",
                    size = style.font_size,
                    spacing = style.letter_spacing,
                    margin_v = style.bottom_margin,
                )?;
            }
        }

        writeln!(
            file,
            "\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text"
        )?;

        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let start = seconds_to_ass_time(seg.start);
            let end = seconds_to_ass_time(seg.end);
            // 顺序固定为「先在原文上折行、再转义、最后套本格式自己的标记」：
            // 折行产出的 `\n` 与原文里已有的 `\n`（双语两行）在**同一次**转换里
            // 变成 ASS 的硬换行 `\N`，折行不会破坏既有的换行标记。
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            let text = escape_ass_text(&wrap_text(&raw, wrap)).replace('\n', "\\N");
            // 有说话人样式就引用它，否则用默认样式
            let style_name = seg
                .speaker
                .and_then(|s| speaker_styles.get(&s))
                .map(String::as_str)
                .unwrap_or("Default");
            writeln!(
                file,
                "Dialogue: 0,{},{},{style_name},,0,0,0,,{}",
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
        // 无样式入口：不折行（`wrap == 0`）。
        Self::write_vtt_inner(segments, path, mode, 0)
    }

    /// VTT 写出。`wrap` 是单行最大字符数，`0` 表示不折行。
    /// 多行 cue 之间用 `\n` 分隔（WebVTT 允许一条 cue 含多行文本）。
    fn write_vtt_inner<P: AsRef<Path>>(
        segments: &[Segment],
        path: P,
        mode: ExportMode,
        wrap: usize,
    ) -> Result<()> {
        let mut file = File::create(path).with_context(|| "创建 VTT 文件失败")?;
        writeln!(file, "WEBVTT")?;
        writeln!(file)?;
        let with_speaker = super::segment::has_speaker_labels(segments);
        for seg in segments {
            let start = seconds_to_srt_time(seg.start).replace(',', ".");
            let end = seconds_to_srt_time(seg.end).replace(',', ".");
            // 与 SRT 同理：先在原文上折行，再整体转义，避免实体被断行劈开。
            // WebVTT 规范明确要求实体（`&amp;`/`&lt;`/`&gt;`），且 `<` 会被当作
            // cue 里的标签起始；与 SRT 共用同一份转义实现。
            let raw = super::segment::export_text_for(seg, mode, with_speaker);
            let text = escape_vtt_text(&wrap_text(&raw, wrap));
            writeln!(file, "{} --> {}", start, end)?;
            writeln!(file, "{}", text)?;
            writeln!(file)?;
        }
        Ok(())
    }

    /// 生成 TXT 纯文本 (每行一句)
    ///
    /// **TXT 刻意不折行**：它是给人通读、也是给下游脚本/检索消费的纯文本，
    /// 「一行 = 一句」才是它的语义。折行会凭空插入换行、破坏逐行处理与可读性，
    /// 因此 `max_chars_per_line` 对 TXT 不生效（与历史行为一致）。
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
            writeln!(
                file,
                "{}",
                super::segment::export_text_for(seg, mode, with_speaker)
            )?;
        }
        Ok(())
    }
}

/// 导出格式说明：`(扩展名, 系统文件对话框的过滤器名)`。
///
/// **放在 writer 里的理由**：格式分派知识本来就在这一层（见
/// `SubtitleWriter::write_in_place` 的 match）——`srt`/`ass`/`vtt`/`txt`/
/// `json`/`ttml`/`ttal` 各写哪种文件、认哪些拼写别名，只有这一处说了算。
/// 把「格式名 -> 扩展名 / 过滤器名」的映射也放进同一个文件，二者才可能同步
/// 演进；散在 UI 层的副本一旦漂移，就会出现「文件叫 `.json`、内容却是 SRT」
/// 这类静默不一致（本函数就是从 `src/ui/views/library.rs` 收敛上来的）。
///
/// 约定：
/// - 返回的扩展名**同时也是** [`SubtitleWriter::write_to_file_with_mode`] 的
///   `format` 参数：取值全部来自 `write_in_place` 认的规范名，调用方可以
///   「拿到什么就写什么」，扩展名与内容天然一致。
/// - 认 writer 的拼写别名：`ebu-tt-d`/`ebutt` 与 `ttml` 同源、`netflix-ttal`
///   与 `ttal` 同源。别名只在这里归一，写盘仍用规范扩展名，否则手改过
///   `config.toml` 的用户会拿到「扩展名与内容对不上」的文件。
/// - 大小写与首尾空白不敏感（`"JSON"`、`" json "` 都按 `json` 处理）。
/// - **未知 / 空格式安全回落 `srt`**：与配置抽屉 `fmt_sel` 的兜底
///   （`_ => "srt"`）一致。老配置或被手工改坏的 `config.toml` 不该导出一个
///   writer 根本写不出来的格式（`write_in_place` 对未知格式是 `bail!`，会直接
///   报错）。剪映 / FCPXML / Premiere XML 走剪辑台的专用导出链路（整目录导出
///   或各自的文件对话框），不属于配置项「字幕输出格式」的取值，故同样回落 `srt`。
pub fn export_spec_for(fmt: &str) -> (&'static str, &'static str) {
    match fmt.trim().to_lowercase().as_str() {
        "ass" => ("ass", "ASS 特效字幕"),
        "vtt" => ("vtt", "VTT 网页字幕"),
        "txt" => ("txt", "TXT 纯文本"),
        "json" => ("json", "JSON 结构化字幕"),
        "ttml" | "ebu-tt-d" | "ebutt" => ("ttml", "EBU-TT-D 广播字幕"),
        "ttal" | "netflix-ttal" => ("ttal", "Netflix TTAL 字幕"),
        // 含 "srt"、空串与任何未知值：统一按 SRT 处理
        _ => ("srt", "SRT 字幕"),
    }
}

/// 按命名模板拼导出文件名（不含目录）。
///
/// # 为什么要有模板
///
/// 导出的默认文件名一直是「工程名 + 扩展名」，而实际工作里同一部片子往往要同时交付
/// 多份（`xx.srt` 给剪辑、`xx.en.srt` 给外语同事、`xx.20261009.srt` 归档）。此前只能
/// 导出后手工改名，而且**六个导出入口各自拼了一遍 `format!("{stem}.{ext}")`**——
/// 想加个日期后缀就得改六处，漏一处就出现「同一个按钮导出的名字规则不一样」。
/// 收成一个纯函数 + 一个模板配置项后，规则只有一处。
///
/// # 占位符
///
/// - `{name}`：工程名（已剥掉原扩展名）
/// - `{ext}`：目标扩展名（不含点）
/// - `{date}`：当天日期 `YYYYMMDD`
///
/// 未识别的 `{...}` 原样保留（用户写错了看得见，而不是被悄悄吃掉）；
/// 模板为空串时回落 `{name}.{ext}`。**模板里若一个占位符都没有**，则把结果当作
/// 文件名主体再补 `.{ext}`——否则用户填「字幕」会导出没有扩展名的文件，
/// 系统认不出格式、双击打不开。
///
/// 为什么 `{date}` 由调用方传入而不是这里取 `chrono::Local::now()`：纯函数才能单测
/// （否则测出来的名字每天都不一样，测试只能写成「包含今天的日期」，等于没测）。
pub fn export_file_name(template: &str, name: &str, ext: &str, date: &str) -> String {
    let tpl = template.trim();
    if tpl.is_empty() {
        return format!("{name}.{ext}");
    }
    let has_placeholder = tpl.contains("{name}") || tpl.contains("{ext}") || tpl.contains("{date}");
    let mut out = tpl
        .replace("{name}", name)
        .replace("{ext}", ext)
        .replace("{date}", date);
    if !has_placeholder {
        out = format!("{out}.{ext}");
    }
    out
}

/// SRT 文本转义：`&` → `&amp;`、`<` → `&lt;`、`>` → `&gt;`。
///
/// 为什么要转义：SRT 规范本身没有标记语法，但主流播放器（VLC / PotPlayer / mpv /
/// ffmpeg）都按 HTML 子集解析字幕文本里的 `<font>`/`<i>`/`<b>`。用户原声里出现
/// `a < b`、歌词里的 `&`、或从网页粘来的 `<div>` 时，未转义的 `<` 会被当成标签
/// 起始：轻则整段不渲染，重则把后续文本一并吞掉（连下一块字幕都可能遭殃）。
/// 实体是播放器普遍兼容的写法，也是 WebVTT 的规范要求。
fn escape_srt_text(text: &str) -> String {
    escape_markup_entities(text)
}

/// WebVTT 文本转义：规则与 SRT 相同（VTT 规范就是要求实体），共用同一实现。
fn escape_vtt_text(text: &str) -> String {
    escape_markup_entities(text)
}

/// `&` `<` `>` → 对应的 HTML 实体，**一遍扫字符**完成。
///
/// 单遍实现把「必须先替换 `&` 再替换 `<`/`>`」这条顺序要求变成了**结构性保证**：
/// 产出的 `&amp;` 是直接 append 到输出缓冲区的，不会回头再被 `&` 规则处理，因此
/// 不可能出现 `&amp;lt;` 这种二次转义——而这正是链式 `replace` 最容易踩的坑
/// （顺序写反时，`? < b` 会变成 `? &amp;lt; b`，播放器最终显示字面量 `&lt;`）。
fn escape_markup_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// ASS 文本转义：`{` → `\{`，`}` → `\}`（ASS 规范里的字面花括号写法）。
///
/// ASS 用 `{...}` 承载覆盖码（`{\i1}`、`{\pos(...)}`）。用户原文里只要出现一个
/// `{`，播放器（Libass / VSFilter）就会把它后面的内容当成覆盖码解析：解析不掉时
/// 常见表现是这一行从该点起整段消失，`\{`/`\}` 才是规范认可的字面量。
///
/// 只处理花括号，**不要**顺手转义反斜杠或其它字符：`\N`（换行）、`\h`（不换行
/// 空格）是本格式自己的合法标记，`write_ass_with_style` 正是靠 `\N` 表达双语/多行。
fn escape_ass_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            c => out.push(c),
        }
    }
    out
}

/// 折行：把文本按「每行最多 `max_chars` 个字符」切开，行间用 `\n` 连接。
///
/// 供 SRT / VTT / ASS 导出使用（按 [`SubtitleStyleConfig::max_chars_per_line`] 生效）；
/// TXT **不折行**（理由见 `write_txt_with_mode` 的说明）。
///
/// 断点优先级（都在「尽量让本行更长」的前提下选择）：
/// 1. **标点**：逗号 / 句号 / 顿号 / 分号 / 冒号 / 问叹号 / 破折号 / 省略号 /
///    右括号与右引号（中英文标点都算）。断点落在标点**之后**——标点留在行尾，
///    下一行不会以标点开头；
/// 2. **空格**：英文按词断行，断点处的空格按惯例**被吞掉**（不留在行尾、
///    也不落到行首），因此 **不会把一个单词切成两半**，行首行尾也不会出现空格；
/// 3. 两者都没有（例如一长串无空格无标点的英文单词）才按字符数**硬切**。
///
/// 实现：在「本行还能容纳的字符窗口」内**从右往左**找第一个候选断点，
/// 于是每行取到允许范围内最靠右的断点（行尽量长）；同一位置既有标点又有空格时
/// 标点优先。中文 / 日文 / 韩文没有词间空格，自然退化为「按字符数切、
/// 优先在标点处断」。
///
/// 约定：
/// - `max_chars == 0`、`text` 为空、或每一行都不足 `max_chars` 时**原样返回**
///   ——老配置 / 未显式传入样式时保持历史导出**逐字节一致**；
/// - 原文里**已有的 `\n`**（双语的第二行、用户手打的换行）逐行独立折行，
///   不会被当成一个长串重排；
/// - 一律按 **Unicode 标量（`char`）** 计数：一个汉字、一个英文字母都算 1，
///   与预览侧 `preview_font_px() * max_chars_per_line` 的「单行最大字数」口径一致。
fn wrap_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.is_empty() {
        return text.to_string();
    }
    text.split('\n')
        .map(|line| wrap_line(line, max_chars))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`wrap_text`] 的单行实现（入参不含 `\n`）。
fn wrap_line(line: &str, max_chars: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 16);
    let mut start = 0usize;
    while chars.len() - start > max_chars {
        // `limit` 是本行最多能放下的字符数；由循环条件可知 `limit < chars.len()`，
        // 因此下面读 `chars[limit]` 不会越界。
        let limit = start + max_chars;
        // 从右往左找最靠右的候选断点；同一位置标点优先于空格。
        let mut brk = None;
        let mut drop_space = false;
        for p in (start + 1..=limit).rev() {
            if is_wrap_break_after(chars[p - 1]) {
                brk = Some(p); // 断在标点「之后」
                break;
            }
            if chars[p] == ' ' {
                brk = Some(p); // 断在空格处（空格归入断点，不回吐到行首）
                drop_space = true;
                break;
            }
        }
        let (mut line_end, mut next) = match brk {
            Some(p) if drop_space => (p, p + 1),
            Some(p) => (p, p),
            // 窗口内既无标点也无空格：只能硬切
            None => (limit, limit),
        };
        // 收尾标点不能被挤到下一行行首：把它们并回本行（连排最多 2 个）。
        // 这会让本行最多**超出 `max_chars` 两个字符**——刻意接受的取舍：
        // 硬切（窗口内既无标点也无空格）本就少见，而「下一行以句号/逗号开头」
        // 是肉眼可见的排版缺陷，宁可让本行多收一个标点。
        let mut merged = 0;
        while next < chars.len() && merged < 2 && is_wrap_break_after(chars[next]) {
            next += 1;
            merged += 1;
        }
        if merged > 0 {
            line_end = next;
        }
        // 下一行开头若是空白，一并跳过，避免行首出现空格。
        while next < chars.len() && chars[next] == ' ' {
            next += 1;
        }
        out.extend(chars[start..line_end].iter());
        out.push('\n');
        start = next;
    }
    out.extend(chars[start..].iter());
    out
}

/// 可作为折行断点的标点（断点落在它**之后**：标点留在行尾）。
///
/// 只收「句读 / 收尾」类标点：**不含**左括号与左引号（它们后面断行没意义，
/// 还会把左括号孤零零留在行尾）；也**不含**英文撇号（半角 `'` 与全角 `’`）——
/// 它们常出现在词内部（`don't` / `don’t`），当成断点会把单词切开。
fn is_wrap_break_after(c: char) -> bool {
    matches!(
        c,
        // 中文 / 全角标点
        '，' | '。' | '、' | '；' | '：' | '！' | '？' | '…' | '—' | '～'
            | '）' | '】' | '」' | '』' | '》' | '〉' | '〕' | '｝' | '”'
        // 英文 / 半角标点
            | ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}'
    )
}

/// 说话人序号 → ASS 主色（`&HAABBGGRR`，不透明）。
///
/// 取一组在高对比深色/浅色背景下都清晰、且彼此可区分的颜色（薄荷 / 蓝 / 琥珀 /
/// 品红 / 青 / 紫 / 珊瑚 / 青柠），循环使用。数量超出时回绕，保证永远给得出颜色。
fn speaker_ass_color(idx: usize) -> &'static str {
    const PALETTE: [&str; 8] = [
        "&H00D1B110", // 薄荷绿（与主色一致）
        "&H00F8BD38", // 天蓝
        "&H000B9EF5", // 琥珀
        "&H00D64FD8", // 品红
        "&H00E0E05A", // 青
        "&H00F06BC0", // 紫
        "&H008080F4", // 珊瑚红
        "&H0080E010", // 青柠
    ];
    PALETTE[idx % PALETTE.len()]
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
        // 霓虹极光：青字 + 深蓝描边 + 底框
        "霓虹极光" => ("&H00BFD42D", "&H002A170F", "&H4018181A", 1, 2.5, 1.0),
        // 活力暖橙：暖橙 + 黑边 + 阴影
        "活力暖橙" => ("&H003C92FB", "&H00000000", "&H80000000", 1, 2.5, 1.0),
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
        assert!(
            content.contains("00:00:01.000 --> 00:00:02.500"),
            "毫秒应以点号分隔: {content}"
        );
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

        // 一次性构造（clippy::field_reassign_with_default）
        let style = SubtitleStyleConfig {
            font_size: 48,
            letter_spacing: 4,
            bottom_margin: 80,
            preset_name: "黄字黑边".to_string(),
            ..SubtitleStyleConfig::default()
        };
        SubtitleWriter::write_ass_with_style(&segs, &temp, ExportMode::RawOnly, &style).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        assert!(
            content.contains("Style: Default,Microsoft YaHei,48,"),
            "字号未写入 ASS: {content}"
        );
        assert!(
            content.contains("&H0000D7FF"),
            "黄字预设配色未写入 ASS: {content}"
        );
        // Spacing=4 / BorderStyle=1 / Outline=3 / Shadow=1 / MarginV=80
        assert!(
            content.contains(",100,100,4,0,1,3,1,2,20,20,80,1"),
            "字间距 / 描边 / 底边距未写入 ASS: {content}"
        );
        assert!(content.contains("测试字幕"), "对白文本缺失: {content}");

        let _ = std::fs::remove_file(temp);
    }

    /// 有说话人标签时，ASS 应为每个说话人生成独立样式，且 Dialogue 引用对应样式。
    #[test]
    fn ass_emits_per_speaker_styles() {
        let mut a = Segment::new(1, 0.0, 1.0, "你好");
        a.speaker = Some(0);
        let mut b = Segment::new(2, 1.0, 2.0, "你好呀");
        b.speaker = Some(1);
        let segs = vec![a, b];

        let temp = std::env::temp_dir().join("test_v2w_speakers.ass");
        SubtitleWriter::write_ass_with_style(
            &segs,
            &temp,
            ExportMode::RawOnly,
            &SubtitleStyleConfig::default(),
        )
        .unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        assert!(
            content.contains("Style: Speaker1,"),
            "缺 Speaker1 样式: {content}"
        );
        assert!(
            content.contains("Style: Speaker2,"),
            "缺 Speaker2 样式: {content}"
        );
        // 两个说话人颜色不同
        let s1 = content
            .lines()
            .find(|l| l.starts_with("Style: Speaker1,"))
            .unwrap();
        let s2 = content
            .lines()
            .find(|l| l.starts_with("Style: Speaker2,"))
            .unwrap();
        assert_ne!(s1, s2, "不同说话人应有不同样式（含颜色）");
        // Dialogue 行引用了对应说话人样式
        assert!(
            content.contains(",Speaker1,,"),
            "第 1 句应引用 Speaker1: {content}"
        );
        assert!(
            content.contains(",Speaker2,,"),
            "第 2 句应引用 Speaker2: {content}"
        );

        let _ = std::fs::remove_file(temp);
    }

    /// 无说话人标签时不应生成任何 SpeakerN 样式（保持旧行为）。
    #[test]
    fn ass_without_speakers_uses_default_only() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "独白")];
        let temp = std::env::temp_dir().join("test_v2w_nospeaker.ass");
        SubtitleWriter::write_ass_with_style(
            &segs,
            &temp,
            ExportMode::RawOnly,
            &SubtitleStyleConfig::default(),
        )
        .unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();
        assert!(
            !content.contains("Style: Speaker"),
            "无说话人不应生成 SpeakerN 样式"
        );
        assert!(content.contains(",Default,,"), "应回落到 Default 样式");
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
        assert!(
            content.contains(",1,2,1,2,20,20,40,1"),
            "默认描边与投影不正确: {content}"
        );
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

    /// `export_spec_for` 是「格式字符串 -> (扩展名, 对话框过滤器名)」的唯一来源，
    /// 必须与 `write_in_place` 的分派表逐一对齐。这里不满足于比对字面量：
    /// **真的按返回的扩展名写一遍**，断言 writer 接受该格式名且产出非空文件，
    /// 否则「映射表加了新格式、分派表没跟上」这类漂移照样会漏过。
    #[test]
    fn export_spec_matches_writer_and_falls_back_to_srt() {
        for (fmt, ext) in [
            ("srt", "srt"),
            ("ass", "ass"),
            ("vtt", "vtt"),
            ("txt", "txt"),
            ("json", "json"),
            ("ttml", "ttml"),
            ("ttal", "ttal"),
        ] {
            let (spec_ext, label) = export_spec_for(fmt);
            assert_eq!(spec_ext, ext, "{fmt} 的扩展名应一致");
            assert!(!label.is_empty(), "{fmt} 必须有对话框过滤器名");

            // 关键断言：返回的格式字符串必须**真的被 writer 接受**
            let dir = tmp_dir(&format!("spec_{fmt}"));
            let out = dir.join(format!("out.{spec_ext}"));
            SubtitleWriter::write_to_file_with_mode(&sample(), &out, spec_ext, ExportMode::RawOnly)
                .unwrap_or_else(|e| {
                    panic!("{fmt} 映射出的格式名 {spec_ext} 未被 writer 接受: {e}")
                });
            let text = std::fs::read_to_string(&out).unwrap();
            assert!(!text.trim().is_empty(), "{fmt} 导出内容为空");
            let _ = std::fs::remove_dir_all(&dir);
        }

        // 别名：writer 认的多种拼写必须归一到同一个规范扩展名，否则手改
        // config.toml 写成 "ebu-tt-d" 会得到「名字 .srt、内容却是 TTML」。
        for (alias, ext) in [
            ("ebu-tt-d", "ttml"),
            ("ebutt", "ttml"),
            ("netflix-ttal", "ttal"),
        ] {
            assert_eq!(export_spec_for(alias).0, ext, "{alias} 应归一为 {ext}");
        }

        // 大小写 / 首尾空白不敏感
        assert_eq!(export_spec_for("JSON").0, "json");
        assert_eq!(export_spec_for("  vtt  ").0, "vtt");

        // 未知 / 空格式必须安全回落到 srt（write_in_place 对未知格式是 bail!）
        assert_eq!(export_spec_for("").0, "srt");
        assert_eq!(export_spec_for("   ").0, "srt");
        assert_eq!(export_spec_for("fcpxml").0, "srt");
        assert_eq!(export_spec_for("SRT").0, "srt");
        assert_eq!(export_spec_for("不存在的格式").0, "srt");
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

    /// 各种格式都能导出且不留临时文件（覆盖 srt/ass/vtt/txt/fcpxml/xml 与新增的
    /// json / ttml(ebu-tt-d) / ttal 分支）。
    #[test]
    fn all_single_file_formats_export_cleanly() {
        for fmt in [
            "srt", "ass", "vtt", "txt", "fcpxml", "xml", "json", "ttml", "ttal",
        ] {
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

    /// 专业格式（JSON / TTML / TTAL）导出后必须是**可解析**的合法文档，而不只是「有字节」。
    /// 这是「导出成功但下游打不开」这类静默失败的最后一道防线。
    #[test]
    fn professional_formats_are_parseable() {
        // JSON：serde 能反序列化回 Value
        let dir = tmp_dir("pro_json");
        let out = dir.join("out.json");
        SubtitleWriter::write_to_file(&sample(), &out, "json").expect("json 导出应成功");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).expect("json 必须可解析");
        assert_eq!(v["segment_count"], 2);
        assert_eq!(v["segments"][0]["text"], "第一句");

        // TTML 家族：起止标签配对、含 <tt 根元素、时间格式正确
        for fmt in ["ttml", "ttal"] {
            let d = tmp_dir(&format!("pro_{fmt}"));
            let o = d.join(format!("out.{fmt}"));
            SubtitleWriter::write_to_file(&sample(), &o, fmt)
                .unwrap_or_else(|e| panic!("{fmt} 导出失败: {e}"));
            let text = std::fs::read_to_string(&o).unwrap();
            assert!(text.starts_with("<?xml"), "{fmt} 应以 XML 声明开头");
            assert!(text.contains("<tt "), "{fmt} 缺 <tt> 根元素");
            assert!(text.trim_end().ends_with("</tt>"), "{fmt} 未正确闭合");
            assert!(text.contains("<p "), "{fmt} 缺字幕段落");
            // 每个 <p> 都要有 begin/end
            assert!(
                text.contains(r#"begin="00:00:00.000""#),
                "{fmt} 起始时间格式不对: {text}"
            );
            let _ = std::fs::remove_dir_all(&d);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 语言推断：文档语言取众数，目标语言取「已带译文」句子的众数。
    #[test]
    fn infers_document_and_target_languages() {
        let mut a = Segment::new(1, 0.0, 1.0, "你好");
        a.language = Some("zh".to_string());
        a.translation = Some("Hello".to_string());
        a.translation_lang = Some("English".to_string());
        let mut b = Segment::new(2, 1.0, 2.0, "世界");
        b.language = Some("zh".to_string());
        // 没有译文的句子不能把目标语言带偏
        let segs = vec![a, b];

        assert_eq!(SubtitleWriter::infer_doc_lang(&segs), "zh");
        assert_eq!(
            SubtitleWriter::infer_target_lang(&segs).as_deref(),
            Some("English")
        );

        // 全空时：文档语言回落 zh，目标语言为 None
        let plain = vec![Segment::new(1, 0.0, 1.0, "无语言标记")];
        assert_eq!(SubtitleWriter::infer_doc_lang(&plain), "zh");
        assert_eq!(SubtitleWriter::infer_target_lang(&plain), None);
    }

    /// 并发导出同一个目标路径时，两个调用者不能共用同一个 `.part` 文件——
    /// 否则两边的内容会交替写进同一个文件，谁先 rename，谁就把拼接出来的
    /// 半截内容当成成品。这里断言临时文件名两两不同。
    #[test]
    fn concurrent_exports_use_distinct_temp_files() {
        let dir = tmp_dir("concurrent");
        let out = dir.join("out.srt");

        let a = SubtitleWriter::temp_sibling(&out).expect("应有临时路径");
        let b = SubtitleWriter::temp_sibling(&out).expect("应有临时路径");
        assert_ne!(a, b, "同一目标的两次导出必须用不同的临时文件");
        // 都落在目标同目录（保证 rename 是原子的）
        assert_eq!(a.parent(), out.parent());
        assert_eq!(b.parent(), out.parent());

        // 并行写同一目标：结果必须是**某一次完整**的内容，而不是两者的混合
        let segs_a = vec![Segment::new(1, 0.0, 1.0, "AAAA")];
        let segs_b = vec![Segment::new(1, 0.0, 1.0, "BBBB")];
        std::thread::scope(|s| {
            for segs in [&segs_a, &segs_b] {
                let out = out.clone();
                s.spawn(move || {
                    let _ = SubtitleWriter::write_to_file(segs, &out, "srt");
                });
            }
        });
        let text = std::fs::read_to_string(&out).expect("目标文件应存在");
        let has_a = text.contains("AAAA");
        let has_b = text.contains("BBBB");
        assert!(has_a ^ has_b, "结果应是其中一次的完整内容: {text:?}");

        // 目录里不应残留任何 .part
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("part"))
            .collect();
        assert!(leftovers.is_empty(), "不应留下临时文件: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────── 特殊字符转义（格式串扰修复） ───────────

    /// SRT：`&` `<` `>` 必须写成实体，且必须**一遍扫字符**完成。
    ///
    /// 链式 `replace` 的经典错误是顺序写反：先把 `<` 换成 `&lt;`，再用 `&` 规则替换，
    /// 于是 `&lt;` 里的 `&` 被二次转义成 `&amp;lt;`——播放器最终显示字面量 `&lt;`
    /// 而不是 `<`。这里既断言正确结果，也断言没有二次转义产物。
    #[test]
    fn srt_escapes_markup_without_double_escaping() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "A & B <b>x</b> C > D &amp; E")];
        let temp = std::env::temp_dir().join("test_v2w_escape.srt");
        SubtitleWriter::write_srt(&segs, &temp).unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        assert!(
            content.contains("A &amp; B &lt;b&gt;x&lt;/b&gt; C &gt; D &amp;amp; E"),
            "SRT 未按实体转义: {content}"
        );
        assert!(!content.contains("&amp;lt;"), "`<` 被二次转义: {content}");
        assert!(!content.contains("&amp;gt;"), "`>` 被二次转义: {content}");
        assert!(
            !content.contains("&amp;amp;amp;"),
            "`&` 被二次转义: {content}"
        );
        // 序号与时间轴不受转义影响
        assert!(
            content.contains("1\n00:00:00,000 --> 00:00:01,000\n"),
            "{content}"
        );

        let _ = std::fs::remove_file(temp);
    }

    /// VTT：WebVTT 规范要求实体，规则与 SRT 一致（同一份单遍实现）。
    #[test]
    fn vtt_escapes_markup_without_double_escaping() {
        let segs = vec![Segment::new(1, 0.0, 1.0, "x <i>y</i> & z > w")];
        let temp = std::env::temp_dir().join("test_v2w_escape.vtt");
        SubtitleWriter::write_to_file(&segs, &temp, "vtt").unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        assert!(content.starts_with("WEBVTT\n"), "{content}");
        assert!(
            content.contains("x &lt;i&gt;y&lt;/i&gt; &amp; z &gt; w"),
            "VTT 未按实体转义: {content}"
        );
        assert!(!content.contains("&amp;lt;"), "`<` 被二次转义: {content}");
        assert!(!content.contains("&amp;amp;"), "`&` 被二次转义: {content}");
        // 时间戳仍是点号毫秒，不能被文本转义逻辑碰到
        assert!(
            content.contains("00:00:00.000 --> 00:00:01.000"),
            "{content}"
        );

        let _ = std::fs::remove_file(temp);
    }

    /// ASS：`{` `}` 必须转成本格式的字面量 `\{` `\}`，否则会被当作覆盖码起始，
    /// 把这一行后半段（乃至后续字幕）吞掉；同时 `\N` 换行必须保持原样。
    #[test]
    fn ass_escapes_braces_and_keeps_newline_marker() {
        let mut seg = Segment::new(1, 0.0, 1.0, "原文 {A} 结束");
        seg.translation = Some("first\nsecond {B}".to_string());
        let segs = vec![seg, Segment::new(2, 1.0, 2.0, "下一句不背锅")];

        let temp = std::env::temp_dir().join("test_v2w_escape.ass");
        SubtitleWriter::write_ass_with_style(
            &segs,
            &temp,
            ExportMode::Bilingual,
            &SubtitleStyleConfig::default(),
        )
        .unwrap();
        let content = std::fs::read_to_string(&temp).unwrap();

        // 双语：第一行译文、第二行原文；换行 → `\N`，花括号 → `\{` `\}`
        assert!(
            content.contains(r"first\Nsecond \{B\}\N原文 \{A\} 结束"),
            "ASS 花括号未转义或换行标记被破坏: {content}"
        );
        // 花括号没有把后续内容吞掉：下一条 Dialogue 仍是完整一行
        assert!(
            content.contains(",Default,,0,0,0,,下一句不背锅"),
            "后续字幕行被吞: {content}"
        );
        // 不应再出现裸露的覆盖码起始
        assert!(!content.contains("{A}"), "仍有未转义花括号: {content}");

        let _ = std::fs::remove_file(temp);
    }

    /// 纯文本（无 `&` `<` `>` `{` `}`）的导出必须与修复前**逐字节一致**：
    /// 转义只应影响真的含特殊字符的字幕，不能顺手改动正常输出。
    #[test]
    fn plain_text_export_is_byte_identical() {
        let segs = sample();

        let srt_dir = tmp_dir("escape_plain_srt");
        let srt = srt_dir.join("plain.srt");
        SubtitleWriter::write_to_file(&segs, &srt, "srt").unwrap();
        assert_eq!(
            std::fs::read_to_string(&srt).unwrap(),
            "1\n00:00:00,000 --> 00:00:01,500\n第一句\n\n2\n00:00:01,500 --> 00:00:03,000\n第二句\n\n"
        );

        let vtt_dir = tmp_dir("escape_plain_vtt");
        let vtt = vtt_dir.join("plain.vtt");
        SubtitleWriter::write_to_file(&segs, &vtt, "vtt").unwrap();
        assert_eq!(
            std::fs::read_to_string(&vtt).unwrap(),
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\n第一句\n\n00:00:01.500 --> 00:00:03.000\n第二句\n\n"
        );

        let ass_dir = tmp_dir("escape_plain_ass");
        let ass = ass_dir.join("plain.ass");
        SubtitleWriter::write_ass(&segs, &ass).unwrap();
        let ass_text = std::fs::read_to_string(&ass).unwrap();
        assert!(
            ass_text.contains("Dialogue: 0,0:00:00.00,0:00:01.50,Default,,0,0,0,,第一句\n"),
            "{ass_text}"
        );
        assert!(
            ass_text.contains("Dialogue: 0,0:00:01.50,0:00:03.00,Default,,0,0,0,,第二句\n"),
            "{ass_text}"
        );

        let _ = std::fs::remove_dir_all(&srt_dir);
        let _ = std::fs::remove_dir_all(&vtt_dir);
        let _ = std::fs::remove_dir_all(&ass_dir);
    }

    /// 三个转义 helper 的单遍语义：绝不回头处理自己产出的实体 / 标记。
    #[test]
    fn escape_helpers_are_single_pass() {
        assert_eq!(escape_srt_text("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        assert_eq!(escape_vtt_text("<i>&</i>"), "&lt;i&gt;&amp;&lt;/i&gt;");
        // 输入本身长得像实体时，只对 `&` 转义一次（正确），不会变成 `&amp;lt;lt;`
        assert_eq!(escape_srt_text("&lt;"), "&amp;lt;");
        assert_eq!(escape_srt_text(""), "");
        // ASS 只动花括号：反斜杠与 `\N` 原样保留
        assert_eq!(escape_ass_text(r"{\i1}正文\N"), r"\{\i1\}正文\N");
        assert_eq!(escape_ass_text("无花括号"), "无花括号");
    }

    // ─────────── 按行宽折行（max_chars_per_line） ───────────

    /// 中文长句：按字符数折行，且**优先在标点处断**（标点留在行尾）。
    #[test]
    fn wrap_chinese_prefers_punctuation() {
        let text = "今天天气很好，我们一起去公园散步，然后回家吃饭。";
        let out = wrap_text(text, 10);
        // 每个断点都落在标点之后：逗号/句号绝不出现在行首
        for line in out.split('\n') {
            assert!(
                !line.starts_with(['，', '。', '、', '；', '：', '！', '？']),
                "标点不应出现在行首: {out:?}"
            );
            assert!(line.chars().count() <= 11, "行超长: {line:?} / {out:?}");
        }
        // 标点留在行尾
        assert!(
            out.contains("很好，\n") || out.contains("很好，"),
            "{out:?}"
        );
        // 折行不应增删字符（去掉换行后与原串一致）
        assert_eq!(out.replace('\n', ""), text, "{out:?}");
    }

    /// 英文长句：优先在空格断，**不在词中间切**（每行都由完整单词组成）。
    #[test]
    fn wrap_english_breaks_at_spaces_not_mid_word() {
        let text = "The quick brown fox jumps over the lazy dog near the river bank";
        let out = wrap_text(text, 20);
        for line in out.split('\n') {
            assert!(line.chars().count() <= 20, "行超长: {line:?} / {out:?}");
            for word in line.split(' ') {
                assert!(
                    text.split(' ').any(|w| w == word),
                    "行内出现被切断的单词 {word:?}: {out:?}"
                );
            }
            // 行首行尾都不应是空格
            assert_eq!(line.trim(), line, "行首/行尾残留空格: {line:?}");
        }
        assert!(out.contains('\n'), "长句应被折行: {out:?}");
    }

    /// 边界：标点恰好落在「下一行行首」的位置时，必须并回本行，
    /// 否则会出现以句号 / 逗号开头的行（肉眼可见的排版缺陷）。
    #[test]
    fn wrap_never_starts_line_with_punctuation() {
        // 前 10 个汉字正好填满一行，逗号紧随其后（正是会被挤到下一行行首的位置）
        let text = "甲乙丙丁戊己庚辛壬癸，后续还有很多内容需要继续折行处理";
        let out = wrap_text(text, 10);
        for line in out.split('\n') {
            assert!(
                !line.starts_with(['，', '。', '、', '；', '：', '！', '？', ',', '.', ';']),
                "行首出现了标点: {out:?}"
            );
        }
        // 第一行应把逗号一并收下（本行最多超 2 字）
        let first = out.split('\n').next().unwrap();
        assert_eq!(first.chars().count(), 11, "逗号应并回本行: {out:?}");
        assert!(first.ends_with('，'), "{out:?}");

        // 英文同理：句号/逗号不能落到行首
        let en = "abcdefghij, klmnopqrst. uvwxyz";
        let out_en = wrap_text(en, 10);
        for line in out_en.split('\n') {
            assert!(
                !line.starts_with([',', '.', ';', ':', '!', '?', '，', '。']),
                "英文行首出现标点: {out_en:?}"
            );
        }
    }

    /// 无空格无标点的一长串英文只能硬切，但绝不能死循环 / 丢字符。
    #[test]
    fn wrap_unbreakable_token_hard_cuts() {
        let text = "aaaaaaaaaabbbbbbbbbbccccccccccdddddddddd";
        let out = wrap_text(text, 7);
        for line in out.split('\n') {
            assert!(line.chars().count() <= 7, "行超长: {line:?}");
        }
        assert_eq!(out.replace('\n', ""), text, "硬切不能丢字符: {out:?}");
    }

    /// 已经是多行的原文（双语 / 用户手打换行）按行独立折行，不会被揉成一团。
    #[test]
    fn wrap_preserves_existing_newlines_per_line() {
        let text = "这是一句很长很长的中文原文字幕内容\nHello there this is a rather long translated subtitle line";
        let out = wrap_text(text, 12);
        // 原文里的 1 个换行必须保留，且在它两侧各自继续折行（换行数 ≥ 原换行数）
        assert!(
            out.matches('\n').count() > text.matches('\n').count(),
            "{out:?}"
        );
        // 原文行与译文行各自折行后仍按原顺序拼接
        let joined: Vec<String> = out.split('\n').map(str::to_string).collect();
        assert!(joined[0].starts_with("这是一句"), "{out:?}");
        assert!(
            joined.contains(&"文字幕内容".to_string()),
            "原文第二行独立折行: {out:?}"
        );
        assert!(
            joined.contains(&"Hello there".to_string()),
            "译文行独立折行: {out:?}"
        );
        assert!(joined.iter().any(|l| l == "line"), "译文尾词保留: {out:?}");
        // 折行不丢**非空白**字符（英文断点处的空格按惯例被吞掉，不留在行尾/行首）
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(strip(&out), strip(text), "{out:?}");
    }

    /// `max_chars == 0` 或文本本身够短 → 原样返回（向后兼容的关键：老配置不折行）。
    #[test]
    fn wrap_is_noop_when_disabled_or_text_fits() {
        let long = "今天天气很好，我们一起去公园散步，然后回家吃饭。";
        assert_eq!(wrap_text(long, 0), long, "0 表示不折行");
        assert_eq!(wrap_text(long, 100), long, "文本短于阈值应原样返回");
        assert_eq!(
            wrap_text(long, long.chars().count()),
            long,
            "恰好等长不折行"
        );
        assert_eq!(wrap_text("", 10), "");
        // 每一行都不超限时也不应插入多余换行
        assert_eq!(wrap_text("短句\n另一短句", 10), "短句\n另一短句");
    }

    /// ASS 路径：折行后仍用硬换行 `\N`，且原有 `\N` 转换不受影响。
    #[test]
    fn ass_wrap_uses_hard_newline_marker() {
        let mut seg = Segment::new(1, 0.0, 2.0, "这是一条很长的中文原文字幕用来验证折行");
        seg.translation = Some("A rather long translated subtitle used to verify wrapping".into());
        let segs = vec![seg];
        let style = SubtitleStyleConfig {
            max_chars_per_line: 12,
            ..SubtitleStyleConfig::default()
        };
        let dir = tmp_dir("wrap_ass");
        let out = dir.join("wrapped.ass");
        SubtitleWriter::write_ass_with_style(&segs, &out, ExportMode::Bilingual, &style).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();

        let dialogue = content
            .lines()
            .find(|l| l.starts_with("Dialogue: 0,0:00:00.00"))
            .expect("应有对白行");
        let text = dialogue.rsplit(",,").next().unwrap();
        // 折行必须表达为 ASS 硬换行，不能是裸 `\n`
        assert!(text.contains(r"\N"), "ASS 折行应为 `\\N`: {text:?}");
        assert!(
            !text.contains('\n'),
            "Dialogue 文本里不能有裸换行: {text:?}"
        );
        // 每段（被 `\N` 分隔）都不超过 max_chars（允许并回标点的 2 字余量）
        for part in text.split(r"\N") {
            assert!(
                part.chars().count() <= 14,
                "ASS 段超长: {part:?} / {text:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：SRT 每行 ≤ max_chars（并回标点允许 +2）、VTT 同理、TXT 完全不折。
    #[test]
    fn end_to_end_wrap_srt_vtt_and_txt_untouched() {
        let long_zh = "今天天气很好，我们一起去公园散步，然后回家吃饭，最后各自回家睡觉。";
        let long_en =
            "The quick brown fox jumps over the lazy dog and keeps running to the river bank";
        let mut seg = Segment::new(1, 0.0, 3.0, long_zh);
        seg.translation = Some(long_en.to_string());
        let segs = vec![seg];
        let style = SubtitleStyleConfig {
            max_chars_per_line: 14,
            ..SubtitleStyleConfig::default()
        };
        let dir = tmp_dir("wrap_e2e");

        // SRT
        let srt = dir.join("wrapped.srt");
        SubtitleWriter::write_to_file_with_style(&segs, &srt, "srt", ExportMode::Bilingual, &style)
            .unwrap();
        let srt_text = std::fs::read_to_string(&srt).unwrap();
        let body: Vec<&str> = srt_text
            .lines()
            .filter(|l| {
                !l.is_empty() && !l.contains("-->") && !l.chars().all(|c| c.is_ascii_digit())
            })
            .collect();
        assert!(body.len() >= 3, "长字幕应折成多行: {srt_text}");
        for line in &body {
            assert!(line.chars().count() <= 16, "SRT 行超长: {line:?}");
        }
        assert!(srt_text.contains('\n'), "{srt_text}");

        // VTT
        let vtt = dir.join("wrapped.vtt");
        SubtitleWriter::write_to_file_with_style(&segs, &vtt, "vtt", ExportMode::Bilingual, &style)
            .unwrap();
        let vtt_text = std::fs::read_to_string(&vtt).unwrap();
        assert!(vtt_text.starts_with("WEBVTT\n"), "{vtt_text}");
        let vtt_body: Vec<&str> = vtt_text
            .lines()
            .filter(|l| !l.is_empty() && !l.contains("-->") && *l != "WEBVTT")
            .collect();
        for line in &vtt_body {
            assert!(line.chars().count() <= 16, "VTT 行超长: {line:?}");
        }

        // TXT：即使传了样式也不折行（一行一句）
        let txt = dir.join("wrapped.txt");
        SubtitleWriter::write_to_file_with_style(&segs, &txt, "txt", ExportMode::Bilingual, &style)
            .unwrap();
        let txt_text = std::fs::read_to_string(&txt).unwrap();
        assert_eq!(
            txt_text,
            format!("{long_en}\n{long_zh}\n"),
            "TXT 必须保持一行一句、不折行"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 向后兼容：`max_chars_per_line == 0` 时，走样式的导出与不带样式的导出逐字节一致。
    #[test]
    fn zero_max_chars_keeps_legacy_output() {
        let mut seg = Segment::new(
            1,
            0.0,
            1.5,
            "今天天气很好，我们一起去公园散步，然后回家吃饭。",
        );
        seg.translation = Some("A rather long translated subtitle line for the byte check".into());
        let segs = vec![seg];
        let dir = tmp_dir("wrap_zero");

        let no_style = dir.join("nostyle.srt");
        SubtitleWriter::write_to_file_with_mode(&segs, &no_style, "srt", ExportMode::Bilingual)
            .unwrap();

        let zero_style = dir.join("zerostyle.srt");
        let style = SubtitleStyleConfig {
            max_chars_per_line: 0,
            ..SubtitleStyleConfig::default()
        };
        SubtitleWriter::write_to_file_with_style(
            &segs,
            &zero_style,
            "srt",
            ExportMode::Bilingual,
            &style,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&no_style).unwrap(),
            std::fs::read_to_string(&zero_style).unwrap(),
            "max_chars == 0 必须与无样式导出逐字节一致"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `write_to_file_with_style` 对**不支持折行的格式**回落到无样式入口。
    ///
    /// 批量导出（`export_selected_library_tasks`）现在无条件走带样式的入口，
    /// 而 `export_spec_for` 可能返回 `json` / `ttml` / `ttal` / `txt`——它们由
    /// 各自模块写盘、折行对它们没有意义。这里逐一断言「带样式」与「无样式」
    /// 的产物**逐字节一致**，即接线没有改变这些格式的任何输出（不因接线报错）。
    #[test]
    fn style_path_falls_back_for_formats_without_wrapping() {
        let mut seg = Segment::new(
            1,
            0.0,
            1.5,
            "今天天气很好，我们一起去公园散步，然后回家吃饭。",
        );
        seg.translation = Some("A rather long translated subtitle line for the byte check".into());
        let segs = vec![seg];
        // 故意用小阈值：若这些格式真的折了行，字节比对立刻失败
        let style = SubtitleStyleConfig {
            max_chars_per_line: 8,
            ..SubtitleStyleConfig::default()
        };

        for fmt in ["json", "ttml", "ttal", "txt"] {
            let dir = tmp_dir(&format!("style_fallback_{fmt}"));
            let plain = dir.join(format!("plain.{fmt}"));
            let styled = dir.join(format!("styled.{fmt}"));

            SubtitleWriter::write_to_file_with_mode(&segs, &plain, fmt, ExportMode::Bilingual)
                .unwrap_or_else(|e| panic!("{fmt} 无样式导出失败: {e}"));
            SubtitleWriter::write_to_file_with_style(
                &segs,
                &styled,
                fmt,
                ExportMode::Bilingual,
                &style,
            )
            .unwrap_or_else(|e| panic!("{fmt} 带样式导出应回落而非报错: {e}"));

            assert_eq!(
                std::fs::read_to_string(&plain).unwrap(),
                std::fs::read_to_string(&styled).unwrap(),
                "{fmt} 无折行能力，带样式导出必须与无样式逐字节一致"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 端到端：`write_srt_with_style` 真的按 `max_chars_per_line` 折行。
    ///
    /// 两条断言同时锁住「折了」与「折得对」：与无样式的 `write_srt_with_mode`
    /// 输出**必须不同**（否则这次接线等于没接），且每条文本行 ≤ max_chars
    /// （允许并回行尾标点的 +2 余量）。末尾再确认折行没有破坏原子写盘。
    #[test]
    fn write_srt_with_style_wraps_and_differs_from_mode() {
        let long_zh = "今天天气很好，我们一起去公园散步，然后回家吃饭，最后各自回家睡觉。";
        let segs = vec![Segment::new(1, 0.0, 3.0, long_zh)];
        let style = SubtitleStyleConfig {
            max_chars_per_line: 16,
            ..SubtitleStyleConfig::default()
        };
        let dir = tmp_dir("srt_style_wrap");

        let plain = dir.join("plain.srt");
        SubtitleWriter::write_srt_with_mode(&segs, &plain, ExportMode::RawOnly).unwrap();

        let wrapped = dir.join("wrapped.srt");
        SubtitleWriter::write_srt_with_style(&segs, &wrapped, ExportMode::RawOnly, &style).unwrap();

        let plain_text = std::fs::read_to_string(&plain).unwrap();
        let wrapped_text = std::fs::read_to_string(&wrapped).unwrap();
        assert_ne!(
            plain_text, wrapped_text,
            "带样式导出必须真的折行，不能与无样式输出逐字节相同"
        );

        // 只取文本行：跳过序号行、时间轴行与空行
        let body: Vec<&str> = wrapped_text
            .lines()
            .filter(|l| {
                !l.is_empty() && !l.contains("-->") && !l.chars().all(|c| c.is_ascii_digit())
            })
            .collect();
        assert!(body.len() >= 2, "长句应折成多行: {wrapped_text:?}");
        for line in &body {
            assert!(
                line.chars().count() <= 18,
                "SRT 行超过 max_chars(+2 标点余量): {line:?} / {wrapped_text:?}"
            );
        }
        // 折行不增删字符：去掉换行后应与原文逐字一致
        assert_eq!(body.concat(), long_zh, "{wrapped_text:?}");

        // 原子写盘：目录里只该有两条正式产物，不应残留 `.part` 临时文件
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出文件名模板：占位符替换、空模板回落、无占位符补扩展名、未知占位符原样保留。
    ///
    /// 这条锁住「六个导出入口共用同一套命名规则」：此前每个入口各写一遍
    /// `format!("{stem}.{ext}")`，想加个日期后缀得改六处，漏一处就出现「同一个
    /// 按钮导出的名字规则不一样」。
    #[test]
    fn export_file_name_renders_placeholders_and_falls_back() {
        // 默认模板 = 历史行为，老用户感知不到新功能
        assert_eq!(
            super::export_file_name("{name}.{ext}", "课程01", "srt", "20261009"),
            "课程01.srt"
        );
        // 三个占位符都能用，且可重复
        assert_eq!(
            super::export_file_name("{name}.{date}.{ext}", "课程01", "srt", "20261009"),
            "课程01.20261009.srt"
        );
        assert_eq!(
            super::export_file_name("{name}-{name}.{ext}", "a", "vtt", "20261009"),
            "a-a.vtt"
        );
        // 空 / 全空白模板 → 回落默认
        assert_eq!(super::export_file_name("", "n", "ass", "20261009"), "n.ass");
        assert_eq!(
            super::export_file_name("   ", "n", "ass", "20261009"),
            "n.ass"
        );
        // 没有占位符：把模板当文件名主体，补上扩展名——否则导出无扩展名文件，
        // 系统认不出格式、双击打不开
        assert_eq!(
            super::export_file_name("字幕", "n", "srt", "20261009"),
            "字幕.srt"
        );
        // 未知占位符原样保留：用户写错了看得见，而不是被悄悄吃掉
        assert_eq!(
            super::export_file_name("{name}-{lang}.{ext}", "n", "srt", "20261009"),
            "n-{lang}.srt"
        );
    }
}
